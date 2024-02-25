use crate::bpf::bpf::{ProfilerSkel, ProfilerSkelBuilder};
use crate::object::{build_id, elf_load, is_dynamic, is_go};
use crate::perf_events::setup_perf_event;
use crate::unwind_info::{
    end_of_function_marker, CfaType, CompactUnwindRow, UnwindData, UnwindInfoBuilder,
};
use crate::usym::symbolize_native_stack_blaze;
use anyhow::anyhow;
use libbpf_rs::skel::OpenSkel;
use libbpf_rs::skel::SkelBuilder;
use libbpf_rs::Link;
use libbpf_rs::MapFlags;
use libbpf_rs::PerfBufferBuilder;
use procfs;
use std::collections::HashMap;
use std::fs;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use crate::bpf::bindings::*;

fn show_profile(
    profile: &RawProfile,
    procs: &HashMap<i32, ProcessInfo>,
    objs: &HashMap<String, ObjectFileInfo>,
) {
    let mut addresses_per_sample: HashMap<PathBuf, HashMap<u64, String>> = HashMap::new();
    symbolize_profile(&mut addresses_per_sample, procs, objs, profile);

    for sample in profile {
        let task_id = sample.pid;
        let Some(ustack) = sample.ustack else { return };
        let _kstack = sample.kstack;

        let _ = show_native_stack(&mut addresses_per_sample, procs, objs, task_id, &ustack);
    }
}

fn find_mapping(mappings: &[ExecutableMapping], addr: u64) -> Option<ExecutableMapping> {
    let mapping = mappings.binary_search_by(|p: &ExecutableMapping| p.start_addr.cmp(&addr));

    let insertion_idx = match mapping {
        Ok(idx) => idx,
        Err(insertion_idx) => insertion_idx,
    };

    let found_mapping = &mappings[insertion_idx - 1];
    if addr > found_mapping.end_addr || addr < found_mapping.start_addr {
        return None;
    }

    Some(found_mapping.clone())
}

fn symbolize_profile(
    addresses_per_sample: &mut HashMap<PathBuf, HashMap<u64, String>>,
    procs: &HashMap<i32, ProcessInfo>,
    objs: &HashMap<String, ObjectFileInfo>,
    profile: &RawProfile,
) -> anyhow::Result<()> {
    // fill addresses_per_sample
    for sample in profile {
        let native_stack = sample.ustack.unwrap();
        let task_id = sample.pid;

        let p = procfs::process::Process::new(task_id)?;
        let status = p.status()?;
        let tgid = status.tgid;

        for (i, addr) in native_stack.addresses.into_iter().enumerate() {
            if native_stack.len <= i.try_into().unwrap() {
                break;
            }

            let Some(info) = procs.get(&tgid) else {
                return Err(anyhow!("process not found"));
            };

            let Some(mapping) = find_mapping(&info.mappings, addr) else {
                return Err(anyhow!("could not find mapping"));
            };

            match &mapping.build_id {
                Some(build_id) => {
                    match objs.get(build_id) {
                        Some(obj) => {
                            // We need the normalized address for normal object files
                            // and might need the absolute addresses for JITs
                            let normalized_addr = addr - mapping.start_addr + mapping.offset
                                - obj.elf_load.0
                                + obj.elf_load.1;

                            let key = (obj.path.clone());
                            let mut addrs: &mut HashMap<u64, String> = addresses_per_sample
                                .entry(key)
                                .or_insert_with(|| HashMap::new());
                            addrs.insert(normalized_addr, "".to_string());
                        }
                        None => {
                            println!("\t\t - [no build id found]");
                        }
                    }
                }
                None => {
                    println!("\t\t - mapping is not backed by a file, could be a JIT segment");
                }
            }
        }
    }

    // second pass, symbolize
    for (path, addr_to_symbol_mapping) in addresses_per_sample.iter_mut() {
        let addresses = addr_to_symbol_mapping.iter().map(|a| *a.0 - 1).collect();
        let symbols = symbolize_native_stack_blaze(addresses, &path);
        for (addr, symbol) in addr_to_symbol_mapping.clone().iter_mut().zip(symbols) {
            addr_to_symbol_mapping.insert(*addr.0, symbol.to_string());
        }
    }

    Ok(())
}

fn show_native_stack(
    addresses_per_sample: &mut HashMap<PathBuf, HashMap<u64, String>>,
    procs: &HashMap<i32, ProcessInfo>,
    objs: &HashMap<String, ObjectFileInfo>,
    task_id: i32,
    native_stack: &native_stack_t,
) -> anyhow::Result<()> {
    let p = procfs::process::Process::new(task_id)?;
    let status = p.status()?;

    let tgid = status.tgid;
    println!("!! sample -- pid: {}, task_id: {}", tgid, p.pid());

    for (i, addr) in native_stack.addresses.into_iter().enumerate() {
        if native_stack.len <= i.try_into().unwrap() {
            break;
        }

        let Some(info) = procs.get(&tgid) else {
            return Err(anyhow!("process not found"));
        };

        let Some(mapping) = find_mapping(&info.mappings, addr) else {
            return Err(anyhow!("could not find mapping"));
        };

        // finally
        match &mapping.build_id {
            Some(build_id) => match objs.get(build_id) {
                Some(obj) => {
                    let normalized_addr = addr - mapping.start_addr + mapping.offset
                        - obj.elf_load.0
                        + obj.elf_load.1;

                    let name = addresses_per_sample
                        .get(&obj.path)
                        .unwrap()
                        .get(&normalized_addr);
                    println!("\t\t - {:?}", name);
                }
                None => {
                    println!("\t\t - [no build id found]");
                }
            },
            None => {
                println!("\t\t - mapping is not backed by a file, could be a JIT segment");
            }
        }
    }
    Ok(())
}

// Some temporary data structures to get things going, this could use lots of
// improvements
#[derive(Debug, Clone)]
enum MappingType {
    FileBacked,
    Anonymous,
    Vdso,
}

#[derive(Clone)]
struct ProcessInfo {
    mappings: Vec<ExecutableMapping>,
}

struct ObjectFileInfo {
    file: fs::File,
    path: PathBuf,
    // p_offset, p_vaddr
    elf_load: (u64, u64),
    is_dyn: bool,
    main_bin: bool,
}

#[derive(Debug, Clone)]
enum Unwinder {
    Unknown,
    NativeFramePointers,
    NativeDwarf,
}

#[derive(Debug, Clone)]
struct ExecutableMapping {
    // No build id means either JIT or that we could not fetch it. Change this.
    build_id: Option<String>,
    kind: MappingType,
    start_addr: u64,
    end_addr: u64,
    offset: u64,
    load_address: u64,
    unwinder: Unwinder,
    // Add (inode, ctime) and whether the file is in the root namespace
}

fn in_memory_unwind_info(path: &str) -> anyhow::Result<Vec<stack_unwind_row_t>> {
    let mut unwind_info = Vec::new();
    let mut last_function_end_addr: Option<u64> = None;
    let mut last_row = None;
    let builder = UnwindInfoBuilder::with_callback(path, |unwind_data| {
        match unwind_data {
            UnwindData::Function(_, end_addr) => {
                // Add the end addr when we hit a new func
                match last_function_end_addr {
                    Some(addr) => {
                        let marker = end_of_function_marker(addr);

                        let row: stack_unwind_row_t = stack_unwind_row_t {
                            pc: marker.pc,
                            cfa_offset: marker.cfa_offset,
                            cfa_type: marker.cfa_type,
                            rbp_type: marker.rbp_type,
                            rbp_offset: marker.rbp_offset,
                        };
                        unwind_info.push(row)
                    }
                    None => {}
                }
                last_function_end_addr = Some(*end_addr);
            }
            UnwindData::Instruction(compact_row) => {
                let row = stack_unwind_row_t {
                    pc: compact_row.pc,
                    cfa_offset: compact_row.cfa_offset,
                    cfa_type: compact_row.cfa_type,
                    rbp_type: compact_row.rbp_type,
                    rbp_offset: compact_row.rbp_offset,
                };
                unwind_info.push(row);
                last_row = Some(*compact_row)
            }
        }
    });

    builder?.process()?;

    if last_function_end_addr.is_none() {
        println!("no last func end addr");
        return Err(anyhow!("not sure what's going on"));
    }

    // Add the last marker
    let marker: CompactUnwindRow = end_of_function_marker(last_function_end_addr.unwrap());
    let row = stack_unwind_row_t {
        pc: marker.pc,
        cfa_offset: marker.cfa_offset,
        cfa_type: marker.cfa_type,
        rbp_type: marker.rbp_type,
        rbp_offset: marker.rbp_offset,
    };
    unwind_info.push(row);

    Ok(unwind_info)
}

pub struct NativeUnwindState {
    dirty: bool,
    last_persisted: Instant,
    live_shard: Vec<stack_unwind_row_t>,
    build_id_to_executable_id: HashMap<String, u32>,
    shard_index: u64,
    low_index: u64,
    high_index: u64,
}

pub struct Profiler<'bpf> {
    // Prevent the links from being removed
    _links: Vec<Link>,
    bpf: ProfilerSkel<'bpf>,
    // Profiler state
    procs: Arc<Mutex<HashMap<i32, ProcessInfo>>>,
    object_files: Arc<Mutex<HashMap<String, ObjectFileInfo>>>,
    // Channel for bpf events,.
    chan_send: Arc<Mutex<mpsc::Sender<Event>>>,
    chan_receive: Arc<Mutex<mpsc::Receiver<Event>>>,
    // Native unwinding state
    native_unwind_state: NativeUnwindState,
    // Debug options
    filter_pids: HashMap<i32, bool>,
    // Profile channel
    profile_send: Arc<Mutex<mpsc::Sender<RawProfile>>>,
    profile_receive: Arc<Mutex<mpsc::Receiver<RawProfile>>>,
}

pub struct Collector {
    profiles: Vec<RawProfile>,
    procs: HashMap<i32, ProcessInfo>,
    objs: HashMap<String, ObjectFileInfo>,
}

type ThreadSafeCollector = Arc<Mutex<Collector>>;

impl Collector {
    pub fn new() -> ThreadSafeCollector {
        Arc::new(Mutex::new(Self {
            profiles: Vec::new(),
            procs: HashMap::new(),
            objs: HashMap::new(),
        }))
    }

    pub fn collect(
        &mut self,
        profile: RawProfile,
        procs: &HashMap<i32, ProcessInfo>,
        objs: &HashMap<String, ObjectFileInfo>,
    ) {
        self.profiles.push(profile);

        for (k, v) in procs {
            self.procs.insert(*k, v.clone());
        }

        for (k, v) in objs {
            self.objs.insert(
                k.clone(),
                ObjectFileInfo {
                    file: std::fs::File::open(v.path.clone()).unwrap(),
                    path: v.path.clone(),
                    elf_load: v.elf_load,
                    is_dyn: v.is_dyn,
                    main_bin: v.main_bin,
                },
            );
        }
    }

    pub fn finish(&self) {
        println!("Collector::finish {}", self.profiles.len());

        for profile in &self.profiles {
            show_profile(profile, &self.procs, &self.objs);
        }
    }
}

// Static config
const SAMPLE_PERIOD_HZ: u64 = 200;
const MAX_UNWIND_INFO_SHARDS: u64 = 50;
const SHARD_CAPACITY: usize = MAX_UNWIND_TABLE_SIZE as usize;
const PERF_BUFFER_PAGES: usize = 512 * 1024;

struct RawSample {
    pid: i32,
    ustack: Option<native_stack_t>,
    kstack: Option<native_stack_t>,
    count: u64,
}

type RawProfile = Vec<RawSample>;

impl Default for Profiler<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl Profiler<'_> {
    pub fn new() -> Self {
        let mut skel_builder = ProfilerSkelBuilder::default();
        skel_builder.obj_builder.debug(true);
        let open_skel = skel_builder.open().expect("open skel");
        let bpf = open_skel.load().expect("load skel");

        let procs = Arc::new(Mutex::new(HashMap::new()));
        let object_files = Arc::new(Mutex::new(HashMap::new()));

        let (sender, receiver) = mpsc::channel();
        let chan_send = Arc::new(Mutex::new(sender));
        let chan_receive = Arc::new(Mutex::new(receiver));

        let live_shard = Vec::with_capacity(SHARD_CAPACITY);
        let build_id_to_executable_id = HashMap::new();
        let shard_index = 0;
        let low_index = 0;
        let high_index = 0;

        let native_unwind_state = NativeUnwindState {
            dirty: false,
            last_persisted: Instant::now() - Duration::from_secs(1_000), // old enough to trigger it the first time
            live_shard,
            build_id_to_executable_id,
            shard_index,
            low_index,
            high_index,
        };

        let (sender, receiver) = mpsc::channel();
        let profile_send: Arc<Mutex<mpsc::Sender<_>>> = Arc::new(Mutex::new(sender));
        let profile_receive = Arc::new(Mutex::new(receiver));

        let filter_pids = HashMap::new();

        Profiler {
            _links: Vec::new(),
            bpf,
            procs,
            object_files,
            chan_send,
            chan_receive,
            native_unwind_state,
            filter_pids,
            profile_send,
            profile_receive,
        }
    }

    pub fn profile_pids(&mut self, pids: Vec<i32>) {
        for pid in pids {
            self.filter_pids.insert(pid, true);
        }
    }

    pub fn send_profile(&mut self, profile: RawProfile) {
        self.profile_send
            .lock()
            .expect("sender lock")
            .send(profile)
            .expect("handle send");
    }

    pub fn run(mut self, duration: Duration, collector: Arc<Mutex<Collector>>) {
        self.setup_perf_events();
        self.set_bpf_map_info();

        let chan_send = self.chan_send.clone();
        let perf_buffer = PerfBufferBuilder::new(self.bpf.maps().events())
            .pages(PERF_BUFFER_PAGES / page_size::get())
            .sample_cb(move |cpu: i32, data: &[u8]| {
                Self::handle_event(&chan_send, cpu, data);
            })
            .lost_cb(Self::handle_lost_events)
            .build()
            .unwrap();

        let _poll_thread = thread::spawn(move || loop {
            perf_buffer.poll(Duration::from_millis(100)).expect("poll");
        });

        let profile_receive = self.profile_receive.clone();
        let procs = self.procs.clone();
        let object_files = self.object_files.clone();
        let collector = collector.clone();

        thread::spawn(move || loop {
            match profile_receive.lock().unwrap().recv() {
                Ok(profile) => {
                    collector.lock().unwrap().collect(
                        profile,
                        &procs.lock().unwrap(),
                        &object_files.lock().unwrap(),
                    );
                }
                Err(_e) => {
                    // println!("failed to receive event {:?}", e);
                }
            }
        });

        let mut start = Instant::now();

        loop {
            println!("main loop {:?}", start.elapsed());
            if start.elapsed() >= duration {
                println!("done after running for {:?}", start.elapsed());
                let profile = self.collect_profile();
                self.send_profile(profile);
                break;
            }

            println!("== elapsed {:?}", start.elapsed());
            if start.elapsed() >= Duration::from_secs(5) {
                let profile = self.collect_profile();
                self.send_profile(profile);
                start = Instant::now();
            }

            let s = Instant::now();
            let read = self.chan_receive.lock().expect("receive lock").try_recv();
            println!("event_new_proc lock took {:?}", s.elapsed());

            match read {
                Ok(event) => {
                    let pid = event.pid;

                    if event.type_ == event_type_EVENT_NEW_PROCESS {
                        let s = Instant::now();
                        self.event_new_proc(pid);
                        let mut pname = "<unknown>".to_string();
                        if let Ok(proc) = procfs::process::Process::new(pid) {
                            if let Ok(name) = proc.cmdline() {
                                pname = name.join("").to_string();
                            }
                        }

                        println!("event_new_proc took {:?} for {}", s.elapsed(), pname);
                    } else {
                        println!("unknow event {}", event.type_);
                    }
                }
                Err(_) => {
                    // todo
                }
            }

            if self.native_unwind_state.dirty
                && self.native_unwind_state.last_persisted.elapsed() > Duration::from_millis(100)
            {
                self.persist_unwind_info(&self.native_unwind_state.live_shard);
                self.native_unwind_state.dirty = false;
                self.native_unwind_state.last_persisted = Instant::now();
            }
        }
    }

    pub fn collect_profile(&mut self) -> RawProfile {
        println!("collecting profile");

        self.teardown_perf_events();

        let mut result = Vec::new();
        let maps = self.bpf.maps();
        let aggregated_stacks = maps.aggregated_stacks();
        let stacks = maps.stacks();

        let mut all_stacks_bytes = Vec::new();
        for aggregated_stack_key_bytes in aggregated_stacks.keys() {
            match aggregated_stacks.lookup(&aggregated_stack_key_bytes, MapFlags::ANY) {
                Ok(Some(aggregated_value_bytes)) => {
                    let mut result_ustack: Option<native_stack_t> = None;
                    let mut result_kstack: Option<native_stack_t> = None;

                    let key: &stack_count_key_t =
                        plain::from_bytes(&aggregated_stack_key_bytes).unwrap();
                    let count: &u64 = plain::from_bytes(&aggregated_value_bytes).unwrap();

                    all_stacks_bytes.push(aggregated_stack_key_bytes.clone());

                    // Maybe check if procinfo is up to date
                    // Fetch actual stacks
                    // Handle errors later
                    if key.user_stack_id > 0 {
                        match stacks.lookup(&key.user_stack_id.to_ne_bytes(), MapFlags::ANY) {
                            Ok(Some(stack_bytes)) => {
                                result_ustack = Some(*plain::from_bytes(&stack_bytes).unwrap());
                            }
                            _ => {
                                eprintln!("\tfailed getting user stack");
                            }
                        }
                    }
                    if key.kernel_stack_id > 0 {
                        match stacks.lookup(&key.kernel_stack_id.to_ne_bytes(), MapFlags::ANY) {
                            Ok(Some(stack_bytes)) => {
                                result_kstack = Some(*plain::from_bytes(&stack_bytes).unwrap());
                            }
                            _ => {
                                eprintln!("\tfailed getting kernel stack");
                            }
                        }
                    }

                    let raw_sample = RawSample {
                        pid: key.task_id,
                        ustack: result_ustack,
                        kstack: result_kstack,
                        count: *count,
                    };

                    result.push(raw_sample);
                }
                _ => continue,
            }
        }

        println!("===== got {} unique stacks", all_stacks_bytes.len());

        // Now we should delete these entries. We should ensure that this is correct and safe
        // as we are iterating while still profiling
        for stacks_bytes in all_stacks_bytes {
            let _ = aggregated_stacks.delete(&stacks_bytes);
        }

        self.setup_perf_events();
        result
    }

    fn process_is_known(&self, pid: i32) -> bool {
        self.procs.lock().expect("lock").get(&pid).is_some()
    }

    fn persist_unwind_info(&self, live_shard: &Vec<stack_unwind_row_t>) {
        println!("calling persist!");
        let start = Instant::now();

        let key = self.native_unwind_state.shard_index.to_ne_bytes();
        let val = unsafe {
            // Probs we need to zero this mem?
            std::slice::from_raw_parts(
                live_shard.as_ptr() as *const u8,
                live_shard.capacity() * ::std::mem::size_of::<stack_unwind_row_t>(),
            )
        };

        self.bpf
            .maps()
            .unwind_tables()
            .update(&key, val, MapFlags::ANY)
            .expect("update"); // error with  value: System(7)', src/main.rs:663:26

        println!("persist_unwind_info took {:?}", start.elapsed());
    }

    fn add_unwind_info(&mut self, pid: i32) {
        if !self.process_is_known(pid) {
            panic!("add_unwind_info -- expected process to be known");
        }

        // Local unwind info state
        let mut dwarf_mappings = Vec::with_capacity(MAX_MAPPINGS_PER_PROCESS as usize);
        let mut num_mappings: u32 = 0;

        // hack for kworkers and such
        let mut got_some_unwind_info: bool = true;

        // Get unwind info
        for (_i, mapping) in self
            .procs
            .lock()
            .expect("lock")
            .get(&pid)
            .unwrap()
            .mappings
            .iter()
            .enumerate()
        {
            if self.native_unwind_state.shard_index > MAX_UNWIND_INFO_SHARDS {
                println!("No more unwind info shards available");
                break;
            }

            // Skip vdso / jit mappings
            match mapping.kind {
                MappingType::Anonymous => {
                    dwarf_mappings.push(mapping_t {
                        load_address: 0,
                        begin: mapping.start_addr,
                        end: mapping.end_addr,
                        executable_id: 0,
                        type_: 1, // jitted
                    });
                    num_mappings += 1;
                    continue;
                }
                MappingType::Vdso => {
                    dwarf_mappings.push(mapping_t {
                        load_address: 0,
                        begin: mapping.start_addr,
                        end: mapping.end_addr,
                        executable_id: 0,
                        type_: 2, // vdso
                    });
                    num_mappings += 1;
                    continue;
                }
                MappingType::FileBacked => {
                    // Handled below
                }
            }

            if mapping.build_id.is_none() {
                panic!("build id should not be none for file backed mappings");
            }

            let my_lock: std::sync::MutexGuard<'_, HashMap<String, ObjectFileInfo>> =
                self.object_files.lock().unwrap();

            let object_file_info = my_lock.get(&mapping.build_id.clone().unwrap()).unwrap();
            let obj_path = object_file_info.path.clone();

            let mut load_address = 0;
            // Hopefully this is the main object
            // TODO: make this work with ASLR + static
            if object_file_info.main_bin {
                if object_file_info.is_dyn {
                    load_address = mapping.load_address; // mapping.start_addr - mapping.offset - object_file_info.elf_load.0;
                }
            } else {
                load_address = mapping.load_address; //mapping.start_addr - object_file_info.elf_load.0;
            }

            // Avoid deadlock
            std::mem::drop(my_lock);

            let build_id = mapping.build_id.clone().unwrap();
            match self
                .native_unwind_state
                .build_id_to_executable_id
                .get(&build_id)
            {
                Some(executable_id) => {
                    // println!("==== in cache");
                    // == Add mapping
                    dwarf_mappings.push(mapping_t {
                        load_address,
                        begin: mapping.start_addr,
                        end: mapping.end_addr,
                        executable_id: *executable_id,
                        type_: 0, // normal i think
                    });
                    num_mappings += 1;
                    continue;
                }
                None => {}
            }

            // This will be populated once we start chunking the unwind info
            // we don't do this yet
            let mut chunks = Vec::with_capacity(MAX_UNWIND_TABLE_CHUNKS as usize);

            // == Add mapping
            dwarf_mappings.push(mapping_t {
                load_address,
                begin: mapping.start_addr,
                end: mapping.end_addr,
                executable_id: self.native_unwind_state.build_id_to_executable_id.len() as u32,
                type_: 0, // normal i think
            });
            num_mappings += 1;

            let build_id = mapping.build_id.clone().unwrap();
            // This is not released (see note "deadlock")
            let first_mapping_ = self.object_files.lock().unwrap();
            let first_mapping = first_mapping_.get(&build_id).unwrap();

            // == Fetch unwind info, so far, this is in mem
            // todo, pass file handle
            let Ok(mut found_unwind_info) =
                in_memory_unwind_info(&first_mapping.path.to_string_lossy())
            else {
                continue;
            };

            found_unwind_info.sort_by(|a, b| {
                let a_pc = a.pc;
                let b_pc = b.pc;
                a_pc.cmp(&b_pc)
            });
            // validate_unwind_info(&found_unwind_info);

            println!(
                "\n======== Unwind rows for executable {}: {} with id {}",
                obj_path.display(),
                &found_unwind_info.len(),
                self.native_unwind_state.build_id_to_executable_id.len(),
            );

            println!("~~ shard index: {}", self.native_unwind_state.shard_index);

            // no unwind info / errors
            if found_unwind_info.is_empty() {
                got_some_unwind_info = false;
                break;
            }

            let first_pc = found_unwind_info[0].pc;
            let last_pc = found_unwind_info[found_unwind_info.len() - 1].pc;
            println!("~~ PC range {:x}-{:x}", first_pc, last_pc,);

            let mut rest_chunk = &found_unwind_info[..];
            loop {
                if rest_chunk.is_empty() {
                    println!("[info-unwind] done chunkin'");
                    break;
                }

                let max_space: usize = SHARD_CAPACITY - self.native_unwind_state.live_shard.len();
                let available_space: usize = std::cmp::min(max_space, rest_chunk.len());
                println!(
                    "-- space used so far in live shard {}",
                    self.native_unwind_state.live_shard.len()
                );
                println!(
                    "-- live shard space left {}, rest_chunk size: {}",
                    max_space,
                    rest_chunk.len()
                );

                if self.native_unwind_state.shard_index > 49 {
                    println!("[info unwind ]too many shards");
                    break;
                }

                if available_space == 0 {
                    println!("!!!! [not fully implemented] no space in live shard");
                    self.native_unwind_state.low_index = 0;
                    self.native_unwind_state.high_index = 0;

                    self.persist_unwind_info(&self.native_unwind_state.live_shard);

                    self.native_unwind_state.live_shard.truncate(0); // Vec::with_capacity(250_000);
                                                                     // - create shard (check if over limit)
                    self.native_unwind_state.shard_index = 0;
                    continue;
                }
                let candidate_chunk: &[stack_unwind_row_t] = &rest_chunk[..available_space];

                let mut real_offset = 0;
                for (i, row) in candidate_chunk.iter().enumerate().rev() {
                    // println!("- {} {:?}", i, row.cfa_type);
                    if row.cfa_type == CfaType::EndFdeMarker as u8 {
                        real_offset = i;
                        break;
                    }
                }

                if real_offset == 0 {
                    println!("[dwarf-debug] we need a new shard, could not find marker!");
                    self.native_unwind_state.low_index = 0;
                    self.native_unwind_state.high_index = 0;
                    self.native_unwind_state.dirty = true;

                    self.persist_unwind_info(&self.native_unwind_state.live_shard);

                    // - clear / reset
                    self.native_unwind_state.live_shard.truncate(0); //= Vec::with_capacity(250_000);
                                                                     // - create shard (check if over limit)
                    self.native_unwind_state.shard_index += 1;
                    continue;
                }

                let current_chunk = &rest_chunk[..=real_offset];
                rest_chunk = &rest_chunk[(real_offset + 1)..];
                println!(
                    "-- current_chunk.len {} rest_chunk.len {}",
                    current_chunk.len(),
                    rest_chunk.len()
                );

                /*                 if current_chunk[0].cfa_type == CfaType::EndFdeMarker as u8 {
                    panic!("wrong start of unwind info {:?}", current_chunk[0].cfa_type);
                }
                if current_chunk[current_chunk.len() - 1].cfa_type != CfaType::EndFdeMarker as u8 {
                    panic!(
                        "wrong end of unwind info {:?}",
                        current_chunk[current_chunk.len() - 1].cfa_type
                    );
                } */

                let _prev_index = self.native_unwind_state.live_shard.len();
                self.native_unwind_state.low_index =
                    self.native_unwind_state.live_shard.len() as u64;

                // Copy unwind info to the live shard
                self.native_unwind_state
                    .live_shard
                    .append(&mut current_chunk.to_vec());
                /*
                if live_shard[prev_index].cfa_type == CfaType::EndFdeMarker as u8 {
                    panic!("wrong start of unwind info");
                } */
                self.native_unwind_state.high_index =
                    self.native_unwind_state.live_shard.len() as u64 - 1;

                /*                 if live_shard[live_shard.len() - 1].cfa_type != CfaType::EndFdeMarker as u8 {
                                   panic!("wrong end of live_shard");
                               }
                */
                // == Add chunks
                // Right now we only fnhave one
                chunks.push(chunk_info_t {
                    low_pc: current_chunk[0].pc,
                    high_pc: current_chunk[current_chunk.len() - 1].pc,
                    shard_index: self.native_unwind_state.shard_index,
                    low_index: self.native_unwind_state.low_index,
                    high_index: self.native_unwind_state.high_index,
                });

                println!(
                    "-- indices ([{}:{}])",
                    self.native_unwind_state.low_index, self.native_unwind_state.high_index,
                );
            }

            // == Add chunks
            // "default"
            chunks.resize(
                MAX_UNWIND_TABLE_CHUNKS as usize,
                chunk_info_t {
                    low_pc: 0,
                    high_pc: 0,
                    shard_index: 0,
                    low_index: 0,
                    high_index: 0,
                },
            );
            let all_chunks_boxed: Box<[chunk_info_t; MAX_UNWIND_TABLE_CHUNKS as usize]> =
                chunks.try_into().expect("try into");
            let all_chunks = unwind_info_chunks_t {
                chunks: *all_chunks_boxed,
            };
            self.bpf
                .maps()
                .unwind_info_chunks()
                .update(
                    &(self.native_unwind_state.build_id_to_executable_id.len() as u32)
                        .to_ne_bytes(),
                    unsafe { plain::as_bytes(&all_chunks) },
                    MapFlags::ANY,
                )
                .unwrap();

            let executable_id = self.native_unwind_state.build_id_to_executable_id.len();
            self.native_unwind_state
                .build_id_to_executable_id
                .insert(build_id, executable_id as u32);
        } // Added all mappings

        if got_some_unwind_info {
            self.native_unwind_state.dirty = true;

            // == Add process info
            // "default"
            dwarf_mappings.resize(
                MAX_MAPPINGS_PER_PROCESS as usize,
                mapping_t {
                    load_address: 0,
                    begin: 0,
                    end: 0,
                    executable_id: 0,
                    type_: 0,
                },
            );
            let boxed_slice = dwarf_mappings.into_boxed_slice();
            let boxed_array: Box<[mapping_t; MAX_MAPPINGS_PER_PROCESS as usize]> =
                boxed_slice.try_into().expect("try into");
            let proc_info = process_info_t {
                is_jit_compiler: 0,
                len: num_mappings,
                mappings: *boxed_array, // check this doesn't go into the stack!!
            };
            self.bpf
                .maps()
                .process_info()
                .update(
                    &pid.to_ne_bytes(),
                    unsafe { plain::as_bytes(&proc_info) },
                    MapFlags::ANY,
                )
                .unwrap();
        }
    }

    fn should_profile(&self, pid: i32) -> bool {
        if self.filter_pids.is_empty() {
            return true;
        }

        return self.filter_pids.get(&pid).is_some();
    }

    fn event_new_proc(&mut self, pid: i32) {
        if self.process_is_known(pid) {
            return;
        }

        if !self.should_profile(pid) {
            return;
        }

        match self.fetch_mapping_info(pid) {
            Ok(()) => {
                self.add_unwind_info(pid);
            }
            Err(_e) => {
                // probabaly a procfs race
            }
        }
    }

    pub fn fetch_mapping_info(&mut self, pid: i32) -> anyhow::Result<()> {
        let proc = procfs::process::Process::new(pid)?;
        let maps = proc.maps()?;

        let mut mappings = vec![];
        let object_files_clone = self.object_files.clone();

        for (i, map) in maps.iter().enumerate() {
            if !map.perms.contains(procfs::process::MMPermissions::EXECUTE) {
                continue;
            }
            match &map.pathname {
                procfs::process::MMapPath::Path(path) => {
                    let mut abs_path = proc.exe()?;
                    abs_path.push("/root");
                    abs_path.push(path);

                    let file = fs::File::open(path)?;

                    let mut unwinder = Unwinder::NativeDwarf;
                    let use_fp = is_go(&abs_path); // todo: deal with CGO and friends
                    if use_fp {
                        unwinder = Unwinder::NativeFramePointers;
                    }

                    let Ok(build_id) = build_id(path) else {
                        continue;
                    };

                    let mut load_address = map.address.0;
                    match maps.iter().nth(i.wrapping_sub(1)) {
                        Some(thing) => {
                            if thing.pathname == map.pathname {
                                load_address = thing.address.0;
                            }
                        }
                        _ => {}
                    }

                    mappings.push(ExecutableMapping {
                        build_id: Some(build_id.clone()),
                        kind: MappingType::FileBacked,
                        start_addr: map.address.0,
                        end_addr: map.address.1,
                        offset: map.offset,
                        load_address,
                        unwinder,
                    });

                    let mut my_lock: std::sync::MutexGuard<'_, HashMap<String, ObjectFileInfo>> =
                        object_files_clone.lock().expect("lock");

                    my_lock.insert(
                        build_id,
                        ObjectFileInfo {
                            path: abs_path,
                            file,
                            elf_load: elf_load(path),
                            is_dyn: is_dynamic(path),
                            main_bin: i < 3,
                        },
                    );
                }
                procfs::process::MMapPath::Anonymous => {
                    mappings.push(ExecutableMapping {
                        build_id: None,
                        kind: MappingType::Anonymous,
                        start_addr: map.address.0,
                        end_addr: map.address.1,
                        offset: map.offset,
                        load_address: 0,
                        unwinder: Unwinder::Unknown,
                    });
                }
                procfs::process::MMapPath::Vsyscall
                | procfs::process::MMapPath::Vdso
                | procfs::process::MMapPath::Vsys(_)
                | procfs::process::MMapPath::Vvar => {
                    mappings.push(ExecutableMapping {
                        build_id: None,
                        kind: MappingType::Vdso,
                        start_addr: map.address.0,
                        end_addr: map.address.1,
                        offset: map.offset,
                        load_address: 0,
                        unwinder: Unwinder::NativeFramePointers,
                    });
                }
                _ => {}
            }
        }

        mappings.sort_by(|a, _b| a.start_addr.cmp(&a.start_addr));
        let proc_info = ProcessInfo { mappings };
        self.procs
            .clone()
            .lock()
            .expect("lock")
            .insert(pid, proc_info);

        Ok(())
    }

    fn handle_event(sender: &Arc<Mutex<std::sync::mpsc::Sender<Event>>>, _cpu: i32, data: &[u8]) {
        let event = plain::from_bytes(data).expect("handle event serde");
        sender
            .lock()
            .expect("sender lock")
            .send(*event)
            .expect("handle event send");
    }

    fn handle_lost_events(cpu: i32, count: u64) {
        println!("lost {count} events on cpu {cpu}");
    }

    pub fn set_bpf_map_info(&mut self) {
        let native_unwinder_prog_id = program_PROGRAM_NATIVE_UNWINDER;
        let native_unwinder_prog_fd = self
            .bpf
            .obj
            .prog_mut("dwarf_unwind")
            .expect("get map")
            .as_fd()
            .as_raw_fd();
        let mut maps = self.bpf.maps_mut();
        let programs = maps.programs();
        programs
            .update(
                &native_unwinder_prog_id.to_le_bytes(),
                &native_unwinder_prog_fd.to_le_bytes(),
                MapFlags::ANY,
            )
            .expect("update map");
    }

    pub fn setup_perf_events(&mut self) {
        let mut prog_fds = Vec::new();
        for i in 0..num_cpus::get() {
            let perf_fd = unsafe { setup_perf_event(i.try_into().unwrap(), SAMPLE_PERIOD_HZ) }
                .expect("setup perf event");
            prog_fds.push(perf_fd);
        }

        for prog_fd in prog_fds {
            let prog = self.bpf.obj.prog_mut("on_event").expect("get prog");
            let link = prog.attach_perf_event(prog_fd);
            self._links.push(link.expect("bpf link is present"));
        }
    }
    pub fn teardown_perf_events(&mut self) {
        self._links = vec![];
    }
}
