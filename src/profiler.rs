use std::collections::HashMap;
use std::fs;
use std::os::fd::{AsFd, AsRawFd};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use libbpf_rs::num_possible_cpus;
use libbpf_rs::skel::SkelBuilder;
use libbpf_rs::skel::{OpenSkel, Skel};
use libbpf_rs::{Link, MapFlags, PerfBufferBuilder};
use procfs;
use tracing::{debug, error, info, span, warn, Level};

use crate::bpf::profiler_bindings::*;
use crate::bpf::profiler_skel::{ProfilerSkel, ProfilerSkelBuilder};
use crate::collector::*;
use crate::object::{build_id, elf_load, is_dynamic, is_go};
use crate::perf_events::setup_perf_event;
use crate::unwind_info::{in_memory_unwind_info, remove_redundant, remove_unnecesary_markers};

// Some temporary data structures to get things going, this could use lots of
// improvements
#[derive(Debug, Clone)]
pub enum MappingType {
    FileBacked,
    Anonymous,
    Vdso,
}

#[derive(Clone)]
pub struct ProcessInfo {
    pub mappings: Vec<ExecutableMapping>,
}

#[allow(dead_code)]
pub struct ObjectFileInfo {
    pub file: fs::File,
    pub path: PathBuf,
    pub load_offset: u64,
    pub load_vaddr: u64,
    pub is_dyn: bool,
    pub main_bin: bool,
}

#[derive(Debug, Clone)]
pub enum Unwinder {
    Unknown,
    NativeFramePointers,
    NativeDwarf,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ExecutableMapping {
    // No build id means either JIT or that we could not fetch it. Change this.
    pub build_id: Option<String>,
    pub kind: MappingType,
    pub start_addr: u64,
    pub end_addr: u64,
    pub offset: u64,
    pub load_address: u64,
    pub unwinder: Unwinder,
    // Add (inode, ctime) and whether the file is in the root namespace
}
pub struct NativeUnwindState {
    dirty: bool,
    last_persisted: Instant,
    live_shard: Vec<stack_unwind_row_t>,
    build_id_to_executable_id: HashMap<String, u32>,
    shard_index: u64,
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
    profile_send: Arc<Mutex<mpsc::Sender<RawAggregatedProfile>>>,
    profile_receive: Arc<Mutex<mpsc::Receiver<RawAggregatedProfile>>>,
    // Duration of this profile
    duration: Duration,
    // Per-CPU Sampling Frequency of this profile in Hz
    sample_freq: u16,
    session_duration: Duration,
}

// Static config
const MAX_SHARDS: u64 = MAX_UNWIND_INFO_SHARDS as u64;
const SHARD_CAPACITY: usize = MAX_UNWIND_TABLE_SIZE as usize;
// Make each perf buffer 512 KB
// TODO: should make this configurable via a command line argument in future
const PERF_BUFFER_BYTES: usize = 512 * 1024;

#[allow(dead_code)]
pub struct RawAggregatedSample {
    pub pid: i32,
    pub ustack: Option<native_stack_t>,
    pub kstack: Option<native_stack_t>,
    pub count: u64,
}

#[derive(Default, Debug)]
#[allow(dead_code)]
pub struct SymbolizedAggregatedSample {
    pub pid: i32,
    pub ustack: Vec<String>,
    pub kstack: Vec<String>,
    pub count: u64,
}

pub type RawAggregatedProfile = Vec<RawAggregatedSample>;
pub type SymbolizedAggregatedProfile = Vec<SymbolizedAggregatedSample>;

impl Default for Profiler<'_> {
    fn default() -> Self {
        Self::new(false, Duration::MAX, 19)
    }
}

impl Profiler<'_> {
    pub fn new(bpf_debug: bool, duration: Duration, sample_freq: u16) -> Self {
        let mut skel_builder: ProfilerSkelBuilder = ProfilerSkelBuilder::default();
        skel_builder.obj_builder.debug(bpf_debug);
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

        let native_unwind_state = NativeUnwindState {
            dirty: false,
            last_persisted: Instant::now() - Duration::from_secs(1_000), // old enough to trigger it the first time
            live_shard,
            build_id_to_executable_id,
            shard_index,
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
            duration,
            sample_freq,
            session_duration: Duration::from_secs(5),
        }
    }

    pub fn profile_pids(&mut self, pids: Vec<i32>) {
        for pid in pids {
            self.filter_pids.insert(pid, true);
        }
    }

    pub fn send_profile(&mut self, profile: RawAggregatedProfile) {
        self.profile_send
            .lock()
            .expect("sender lock")
            .send(profile)
            .expect("handle send");
    }

    pub fn run(mut self, collector: Arc<Mutex<Collector>>) {
        let num_cpus = num_possible_cpus().expect("get possible CPUs") as u64;
        let max_samples_per_session =
            self.sample_freq as u64 * num_cpus * self.session_duration.as_secs();
        if max_samples_per_session >= MAX_AGGREGATED_STACKS_ENTRIES.into() {
            warn!("samples might be lost due to too many samples in a profile session");
        }

        self.setup_perf_events();
        self.set_bpf_map_info();

        let chan_send = self.chan_send.clone();
        let perf_buffer = PerfBufferBuilder::new(self.bpf.maps().events())
            .pages(PERF_BUFFER_BYTES / page_size::get())
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

        let start_time: Instant = Instant::now();
        let mut time_since_last_scheduled_collection: Instant = Instant::now();

        loop {
            if start_time.elapsed() >= self.duration {
                debug!("done after running for {:?}", start_time.elapsed());
                let profile = self.collect_profile();
                self.send_profile(profile);
                break;
            }

            if time_since_last_scheduled_collection.elapsed() >= self.session_duration {
                debug!("collecting profiles on schedule");
                let profile = self.collect_profile();
                self.send_profile(profile);
                time_since_last_scheduled_collection = Instant::now();
            }

            let read = self.chan_receive.lock().expect("receive lock").try_recv();

            match read {
                Ok(event) => {
                    let pid = event.pid;

                    if event.type_ == event_type_EVENT_NEW_PROCESS {
                        // let span = span!(Level::DEBUG, "calling event_new_proc").entered();
                        self.event_new_proc(pid);

                        //let mut pname = "<unknown>".to_string();
                        /*                         if let Ok(proc) = procfs::process::Process::new(pid) {
                            if let Ok(name) = proc.cmdline() {
                                pname = name.join("").to_string();
                            }
                        } */
                    } else {
                        error!("unknown event {}", event.type_);
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

    /// Clears a BPF map in a iterator-stable way.
    pub fn clear_map(&mut self, name: &str) {
        let map = self.bpf.object().map(name).expect("map exists");
        let mut total_entries = 0;
        let mut failures = 0;
        let mut previous_key: Option<Vec<u8>> = None;

        let mut delete_entry = |previous_key: Option<Vec<u8>>| {
            if let Some(previous_key) = previous_key {
                if map.delete(&previous_key).is_err() {
                    failures += 1;
                }
            }
        };

        for key in map.keys() {
            delete_entry(previous_key);
            total_entries += 1;
            previous_key = Some(key);
        }

        // Delete last entry.
        delete_entry(previous_key);

        debug!(
            "clearing map {} found {} entries, failed to delete {} entries",
            name, total_entries, failures
        );
    }

    /// Collect the BPF unwinder statistics and aggregate the per CPU values.
    pub fn collect_unwinder_stats(&self) {
        for key in self.bpf.maps().percpu_stats().keys() {
            let per_cpu_value = self
                .bpf
                .maps()
                .percpu_stats()
                .lookup_percpu(&key, MapFlags::ANY)
                .expect("failed to lookup stats value")
                .expect("empty stats");

            let total_value = per_cpu_value
                .iter()
                .map(|value| {
                    let stats: unwinder_stats_t =
                        *plain::from_bytes(value).expect("failed serde of bpf stats");
                    stats
                })
                .fold(unwinder_stats_t::default(), |a, b| a + b);

            info!("unwinder stats: {:?}", total_value);
        }
    }

    pub fn clear_stats_map(&mut self) {
        let key = 0_u32.to_le_bytes();
        let default = unwinder_stats_t::default();
        let value = unsafe { plain::as_bytes(&default) };

        let mut values: Vec<Vec<u8>> = Vec::new();
        let num_cpus = num_possible_cpus().expect("get possible CPUs") as u64;
        for _ in 0..num_cpus {
            values.push(value.to_vec());
        }

        self.bpf
            .maps()
            .percpu_stats()
            .update_percpu(&key, &values, MapFlags::ANY)
            .expect("zero percpu_stats");
    }

    /// Clear the `percpu_stats`, `stacks`, and `aggregated_stacks` maps one entry at a time.
    pub fn clear_maps(&mut self) {
        let _span = span!(Level::DEBUG, "clear_maps").entered();

        self.clear_stats_map();
        self.clear_map("stacks");
        self.clear_map("aggregated_stacks");
    }

    pub fn collect_profile(&mut self) -> RawAggregatedProfile {
        debug!("collecting profile");

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
                            Ok(None) => {}
                            Err(e) => {
                                error!("\tfailed getting user stack {}", e);
                            }
                        }
                    }
                    if key.kernel_stack_id > 0 {
                        match stacks.lookup(&key.kernel_stack_id.to_ne_bytes(), MapFlags::ANY) {
                            Ok(Some(stack_bytes)) => {
                                result_kstack = Some(*plain::from_bytes(&stack_bytes).unwrap());
                            }
                            _ => {
                                error!("\tfailed getting kernel stack");
                            }
                        }
                    }

                    let raw_sample = RawAggregatedSample {
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

        debug!("===== got {} unique stacks", all_stacks_bytes.len());

        self.collect_unwinder_stats();
        self.clear_maps();
        self.setup_perf_events();
        result
    }

    fn process_is_known(&self, pid: i32) -> bool {
        self.procs.lock().expect("lock").get(&pid).is_some()
    }

    fn persist_unwind_info(&self, live_shard: &Vec<stack_unwind_row_t>) {
        let _span = span!(Level::DEBUG, "persist_unwind_info").entered();

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
    }

    fn add_unwind_info(&mut self, pid: i32) {
        if !self.process_is_known(pid) {
            panic!("add_unwind_info -- expected process to be known");
        }

        // Local unwind info state
        let mut mappings = Vec::with_capacity(MAX_MAPPINGS_PER_PROCESS as usize);
        let mut num_mappings: u32 = 0;

        // hack for kworkers and such
        let mut got_some_unwind_info: bool = true;

        // Get unwind info
        for mapping in self
            .procs
            .lock()
            .expect("lock")
            .get(&pid)
            .unwrap()
            .mappings
            .iter()
        {
            if self.native_unwind_state.shard_index > MAX_SHARDS {
                error!("No more unwind info shards available");
                break;
            }

            // Skip vdso / jit mappings
            match mapping.kind {
                MappingType::Anonymous => {
                    mappings.push(mapping_t {
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
                    mappings.push(mapping_t {
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
                    mappings.push(mapping_t {
                        load_address,
                        begin: mapping.start_addr,
                        end: mapping.end_addr,
                        executable_id: *executable_id,
                        type_: 0, // normal i think
                    });
                    num_mappings += 1;
                    debug!("unwind info CACHED for executable");
                    continue;
                }
                None => {
                    debug!("unwind info not found for executable");
                }
            }

            let mut chunks = Vec::with_capacity(MAX_UNWIND_TABLE_CHUNKS as usize);

            // == Add mapping
            mappings.push(mapping_t {
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
            let span = span!(
                Level::DEBUG,
                "calling in_memory_unwind_info",
                "{}",
                first_mapping.path.to_string_lossy()
            )
            .entered();

            let Ok(mut found_unwind_info) =
                in_memory_unwind_info(&first_mapping.path.to_string_lossy())
            else {
                continue;
            };
            span.exit();

            let span: span::EnteredSpan = span!(Level::DEBUG, "sort unwind info").entered();
            found_unwind_info.sort_by(|a, b| {
                let a_pc = a.pc;
                let b_pc = b.pc;
                a_pc.cmp(&b_pc)
            });
            span.exit();

            let span: span::EnteredSpan = span!(Level::DEBUG, "optimize unwind info").entered();
            let found_unwind_info = remove_unnecesary_markers(&found_unwind_info);
            let found_unwind_info = remove_redundant(&found_unwind_info);
            span.exit();

            debug!(
                "======== Unwind rows for executable {}: {} with id {}",
                obj_path.display(),
                &found_unwind_info.len(),
                self.native_unwind_state.build_id_to_executable_id.len(),
            );

            // no unwind info / errors
            if found_unwind_info.is_empty() {
                got_some_unwind_info = false;
                break;
            }

            let first_pc = found_unwind_info[0].pc;
            let last_pc = found_unwind_info[found_unwind_info.len() - 1].pc;
            debug!(
                "-- unwind information covers PCs: [{:x}-{:x}]",
                first_pc, last_pc,
            );

            let mut chunk_cumulative_len = 0;
            let mut current_chunk;
            let mut rest_chunk = &found_unwind_info[..];

            let span: span::EnteredSpan = span!(Level::DEBUG, "chunk unwind info").entered();
            loop {
                if rest_chunk.is_empty() {
                    debug!("done chunkin'");
                    break;
                }

                assert!(
                    self.native_unwind_state.live_shard.len() <= SHARD_CAPACITY,
                    "live shard exceeds the maximum capacity"
                );

                if self.native_unwind_state.shard_index >= MAX_SHARDS {
                    warn!("used all the shards, wiping state");

                    self.native_unwind_state.live_shard.truncate(0);
                    self.native_unwind_state.shard_index = 0;

                    // TODO: clear BPF maps but ensure this doesn't happen too often.
                    self.native_unwind_state.build_id_to_executable_id = HashMap::new();

                    // The next call to this method will continue populating the needed data.
                    return;
                }

                let free_space: usize = SHARD_CAPACITY - self.native_unwind_state.live_shard.len();
                let available_space: usize = std::cmp::min(free_space, rest_chunk.len());

                debug!(
                    "-- space used so far in live shard {} {}",
                    self.native_unwind_state.shard_index,
                    self.native_unwind_state.live_shard.len()
                );
                debug!(
                    "-- live shard free space: {}, rest_chunk size: {}",
                    free_space,
                    rest_chunk.len()
                );

                if available_space == 0 {
                    info!("no space in live shard, allocating a new one");

                    self.persist_unwind_info(&self.native_unwind_state.live_shard);
                    self.native_unwind_state.live_shard.truncate(0);
                    self.native_unwind_state.shard_index += 1;
                    continue;
                }

                current_chunk = &rest_chunk[..available_space];
                rest_chunk = &rest_chunk[available_space..];
                chunk_cumulative_len += current_chunk.len();

                let low_index = self.native_unwind_state.live_shard.len() as u64;
                // Copy unwind info to the live shard
                self.native_unwind_state
                    .live_shard
                    .append(&mut current_chunk.to_vec());
                let high_index = self.native_unwind_state.live_shard.len() as u64;

                let low_pc = current_chunk[0].pc;
                let high_pc = if rest_chunk.is_empty() {
                    current_chunk[current_chunk.len() - 1].pc
                } else {
                    rest_chunk[0].pc - 1
                };
                let shard_index = self.native_unwind_state.shard_index;

                // Add chunk
                chunks.push(chunk_info_t {
                    low_pc,
                    high_pc,
                    shard_index,
                    low_index,
                    high_index,
                });

                debug!("-- chunk covers PCs [{:x}:{:x}]", low_pc, high_pc);
                debug!(
                    "-- chunk is in shard: {} in range [{}:{}]",
                    shard_index, low_index, high_index
                );
            }
            span.exit();

            assert!(found_unwind_info.len() == chunk_cumulative_len, "total length of chunks should be as big as the size of the whole unwind information");

            // Add chunks
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

            // Add process info
            // "default"
            mappings.resize(
                MAX_MAPPINGS_PER_PROCESS as usize,
                mapping_t {
                    load_address: 0,
                    begin: 0,
                    end: 0,
                    executable_id: 0,
                    type_: 0,
                },
            );
            let boxed_slice = mappings.into_boxed_slice();
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

                    let unwinder = Unwinder::NativeDwarf;

                    // Disable profiling Go applications as they are not properly supported yet.
                    // Among other things, blazesym doesn't support symbolizing Go binaries.
                    if is_go(&abs_path) {
                        // todo: deal with CGO and friends
                        return Err(anyhow!("Go applications are not supported yet"));
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
                        _ => {
                            // todo: cleanup
                        }
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

                    let elf_load = elf_load(path);

                    my_lock.insert(
                        build_id,
                        ObjectFileInfo {
                            path: abs_path,
                            file,
                            load_offset: elf_load.offset,
                            load_vaddr: elf_load.vaddr,
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

        mappings.sort_by_key(|k| k.start_addr.cmp(&k.start_addr));
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
        error!("lost {count} events on cpu {cpu}");
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
        for i in 0..num_possible_cpus().expect("get possible CPUs") {
            let perf_fd =
                unsafe { setup_perf_event(i.try_into().unwrap(), self.sample_freq as u64) }
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
