use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd};
use std::path::PathBuf;
use std::process;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{bounded, select, tick, unbounded, Receiver, Sender};

use std::thread;
use std::time::Duration;

use anyhow::anyhow;
use libbpf_rs::num_possible_cpus;
use libbpf_rs::skel::SkelBuilder;
use libbpf_rs::skel::{OpenSkel, Skel};
use libbpf_rs::{Link, MapFlags, PerfBufferBuilder};
use procfs;
use tracing::{debug, error, info, span, warn, Level};

use crate::bpf::profiler_bindings::*;
use crate::bpf::profiler_skel::{ProfilerSkel, ProfilerSkelBuilder};
use crate::bpf::tracers_bindings::*;
use crate::bpf::tracers_skel::{TracersSkel, TracersSkelBuilder};
use crate::collector::*;
use crate::object::{BuildId, ExecutableId, ObjectFile};
use crate::perf_events::setup_perf_event;
use crate::unwind_info::CompactUnwindRow;
use crate::unwind_info::{in_memory_unwind_info, remove_redundant, remove_unnecesary_markers};
use crate::util::{get_online_cpus, summarize_address_range};

pub enum TracerEvent {
    ProcessExit(i32),
    Munmap(i32, u64),
}

// Some temporary data structures to get things going, this could use lots of
// improvements
#[derive(Debug, Clone)]
pub enum MappingType {
    FileBacked,
    Anonymous,
    Vdso,
}

#[derive(Clone)]
pub enum ProcessStatus {
    Running,
    Exited,
}

#[derive(Clone)]
pub struct ProcessInfo {
    pub status: ProcessStatus,
    pub mappings: ExecutableMappings,
}

pub struct ObjectFileInfo {
    pub file: fs::File,
    pub path: PathBuf,
    pub load_offset: u64,
    pub load_vaddr: u64,
    pub is_dyn: bool,
    pub references: i64,
}

impl Clone for ObjectFileInfo {
    fn clone(&self) -> Self {
        ObjectFileInfo {
            file: self.open_file_from_procfs_fd(),
            path: self.path.clone(),
            load_offset: self.load_offset,
            load_vaddr: self.load_vaddr,
            is_dyn: self.is_dyn,
            references: self.references,
        }
    }
}

impl ObjectFileInfo {
    /// Files might be removed at any time from the file system and they won't
    /// be accessible anymore with their path. We work around this by doing the
    /// following:
    ///
    /// - We open object files as soon as we learn about them, that way we increase
    ///   the reference count of the file in the kernel. Files won't really be deleted
    ///   until the reference count drops to zero.
    /// - In order to re-open files even if they've been deleted, we can use the procfs
    ///   interface, as long as their reference count hasn't reached zero and the kernel
    ///   hasn't removed the file from the file system and the various caches.
    fn open_file_from_procfs_fd(&self) -> File {
        let raw_fd = self.file.as_raw_fd();
        File::open(format!("/proc/{}/fd/{}", process::id(), raw_fd)).expect(
            "re-opening the file from procfs will never fail as we have an already opened file",
        )
    }
}

#[derive(Debug, Clone)]
pub struct ExecutableMapping {
    pub executable_id: ExecutableId,
    // No build id means either JIT or that we could not fetch it. Change this.
    pub build_id: Option<BuildId>,
    pub kind: MappingType,
    pub start_addr: u64,
    pub end_addr: u64,
    pub offset: u64,
    pub load_address: u64,
    pub main_exec: bool,
    pub unmapped: bool,
    // Add (inode, ctime) and whether the file is in the root namespace
}

#[derive(Clone)]
pub struct ExecutableMappings(Vec<ExecutableMapping>);

impl ExecutableMappings {
    pub fn for_address(&self, addr: u64) -> Option<ExecutableMapping> {
        for mapping in &self.0 {
            if mapping.start_addr <= addr && addr <= mapping.end_addr {
                return Some(mapping.clone());
            }
        }

        None
    }
}

impl ExecutableMapping {
    fn mark_as_deleted(&mut self, object_files: &mut HashMap<ExecutableId, ObjectFileInfo>) {
        // Avoid decrementing the reference count logic more than once if called multiple times.
        if self.unmapped {
            return;
        }

        self.unmapped = true;

        if let Some(el) = object_files.get_mut(&self.executable_id) {
            el.references -= 1;

            debug_assert!(
                el.references >= 0,
                "Reference count for {:?} is negative ",
                el.path
            );
        }
    }
}
pub struct NativeUnwindState {
    dirty: bool,
    live_shard: Vec<CompactUnwindRow>,
    known_executables: HashSet<ExecutableId>,
    shard_index: u64,
}

impl Default for NativeUnwindState {
    fn default() -> Self {
        Self {
            dirty: false,
            live_shard: Vec::with_capacity(SHARD_CAPACITY),
            known_executables: HashSet::new(),
            shard_index: 0,
        }
    }
}

pub struct Profiler<'bpf> {
    // Prevent the links from being removed
    _links: Vec<Link>,
    bpf: ProfilerSkel<'bpf>,
    tracers: TracersSkel<'bpf>,
    // Profiler state
    procs: Arc<Mutex<HashMap<i32, ProcessInfo>>>,
    object_files: Arc<Mutex<HashMap<ExecutableId, ObjectFileInfo>>>,
    // Channel for new process events.
    new_proc_chan_send: Arc<Sender<Event>>,
    new_proc_chan_receive: Arc<Receiver<Event>>,
    // Channel for tracer events such as munmaps and process exits.
    tracers_chan_send: Arc<Sender<TracerEvent>>,
    tracers_chan_receive: Arc<Receiver<TracerEvent>>,
    // Profiler stop channel.
    stop_chan_receive: Receiver<()>,
    // Native unwinding state
    native_unwind_state: NativeUnwindState,
    // Debug options
    filter_pids: HashMap<i32, bool>,
    // Profile channel
    profile_send: Arc<Sender<RawAggregatedProfile>>,
    profile_receive: Arc<Receiver<RawAggregatedProfile>>,
    // Duration of this profile
    duration: Duration,
    // Per-CPU Sampling Frequency of this profile in Hz
    sample_freq: u16,
    // Size of each perf buffer, in bytes
    perf_buffer_bytes: usize,
    session_duration: Duration,
}

// Static config
const MAX_SHARDS: u64 = MAX_UNWIND_INFO_SHARDS as u64;
const SHARD_CAPACITY: usize = MAX_UNWIND_TABLE_SIZE as usize;
const MAX_CHUNKS: usize = MAX_UNWIND_TABLE_CHUNKS as usize;

#[derive(Debug, Hash, Eq, PartialEq)]
pub struct RawAggregatedSample {
    pub pid: i32,
    pub tid: i32,
    pub ustack: Option<native_stack_t>,
    pub kstack: Option<native_stack_t>,
    pub count: u64,
}

impl fmt::Display for RawAggregatedSample {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let format_native_stack = |native_stack: Option<native_stack_t>| -> String {
            let mut res: Vec<String> = Vec::new();
            match native_stack {
                Some(native_stack) => {
                    for (i, addr) in native_stack.addresses.into_iter().enumerate() {
                        if native_stack.len <= i.try_into().unwrap() {
                            break;
                        }
                        res.push(format!("{:3}: {:#018x}", i, addr));
                    }
                }
                None => res.push("NONE".into()),
            };
            format!("[{}]", res.join(","))
        };

        fmt.debug_struct("RawAggregatedSample")
            .field("pid", &self.pid)
            .field("tid", &self.tid)
            .field("ustack", &format_native_stack(self.ustack))
            .field("kstack", &format_native_stack(self.kstack))
            .field("count", &self.count)
            .finish()
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct FrameAddress {
    /// Address from the process, as collected from the BPF program.
    pub virtual_address: u64,
    /// The offset in the object file after converting the virtual_address its relative position.
    pub file_offset: u64,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct Frame {
    /// Address from the process, as collected from the BPF program.
    pub virtual_address: u64,
    /// The offset in the object file after converting the virtual_address its relative position.
    pub file_offset: Option<u64>,
    pub name: String,
    pub inline: bool,
}

impl fmt::Display for Frame {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let inline_str = if self.inline { "[inlined] " } else { "" };
        write!(fmt, "{}{}", inline_str, self.name)
    }
}

impl Frame {
    pub fn with_error(msg: String) -> Self {
        Self {
            virtual_address: 0xBAD,
            file_offset: None,
            name: msg,
            inline: false,
        }
    }
}

#[derive(Default, Debug, Hash, Eq, PartialEq)]
pub struct SymbolizedAggregatedSample {
    pub pid: i32,
    pub tid: i32,
    pub ustack: Vec<Frame>,
    pub kstack: Vec<Frame>,
    pub count: u64,
}

impl fmt::Display for SymbolizedAggregatedSample {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let format_symbolized_stack = |symbolized_stack: &Vec<Frame>| -> String {
            let mut res = vec![];
            if symbolized_stack.is_empty() {
                res.push("NONE".to_string());
            } else {
                for (i, symbol) in symbolized_stack.iter().enumerate() {
                    res.push(format!("{:3}: {}", i, symbol));
                }
            }
            res.join("\n");
            format!("[{}]", res.join(","))
        };

        fmt.debug_struct("SymbolizedAggregatedSample")
            .field("pid", &self.pid)
            .field("tid", &self.tid)
            .field("ustack", &format_symbolized_stack(&self.ustack))
            .field("kstack", &format_symbolized_stack(&self.kstack))
            .field("count", &self.count)
            .finish()
    }
}

pub type RawAggregatedProfile = Vec<RawAggregatedSample>;
pub type SymbolizedAggregatedProfile = Vec<SymbolizedAggregatedSample>;

pub struct ProfilerConfig {
    pub libbpf_debug: bool,
    pub bpf_logging: bool,
    pub duration: Duration,
    pub sample_freq: u16,
    pub perf_buffer_bytes: usize,
    pub mapsize_info: bool,
    pub mapsize_stacks: u32,
    pub mapsize_aggregated_stacks: u32,
    pub mapsize_unwind_info_chunks: u32,
    pub mapsize_unwind_tables: u32,
    pub mapsize_rate_limits: u32,
}

// Note that we normally pass in the defaults from Clap, and we don't want
// to be in the business of keeping the default values defined in Clap in sync
// with the defaults defined here.  So these are some defaults that will
// almost always be overridden.
impl Default for ProfilerConfig {
    fn default() -> Self {
        Self {
            libbpf_debug: false,
            bpf_logging: false,
            duration: Duration::MAX,
            sample_freq: 19,
            perf_buffer_bytes: 512 * 1024,
            mapsize_info: false,
            mapsize_stacks: 100000,
            mapsize_aggregated_stacks: 10000,
            mapsize_unwind_info_chunks: 5000,
            mapsize_unwind_tables: 65,
            mapsize_rate_limits: 5000,
        }
    }
}

impl Default for Profiler<'_> {
    fn default() -> Self {
        let (_stop_signal_send, stop_signal_receive) = bounded(1);

        Self::new(ProfilerConfig::default(), stop_signal_receive)
    }
}

impl Profiler<'_> {
    pub fn new(profiler_config: ProfilerConfig, stop_signal_receive: Receiver<()>) -> Self {
        let duration = profiler_config.duration;
        let sample_freq = profiler_config.sample_freq;
        let perf_buffer_bytes = profiler_config.perf_buffer_bytes;
        let mut skel_builder: ProfilerSkelBuilder = ProfilerSkelBuilder::default();
        skel_builder.obj_builder.debug(profiler_config.libbpf_debug);
        let mut open_skel = skel_builder.open().expect("open skel");
        // mapsize modifications can only be made before the maps are actually loaded
        // Initialize map sizes with defaults or modifications
        open_skel
            .maps_mut()
            .stacks()
            .set_max_entries(profiler_config.mapsize_stacks)
            .expect("Unable to set stacks map max_entries");
        open_skel
            .maps_mut()
            .aggregated_stacks()
            .set_max_entries(profiler_config.mapsize_aggregated_stacks)
            .expect("Unable to set aggregated_stacks map max_entries");
        open_skel
            .maps_mut()
            .unwind_info_chunks()
            .set_max_entries(profiler_config.mapsize_unwind_info_chunks)
            .expect("Unable to set unwind_info_chunks map max_entries");
        open_skel
            .maps_mut()
            .unwind_tables()
            .set_max_entries(profiler_config.mapsize_unwind_tables)
            .expect("Unable to set unwind_tables map max_entries");
        open_skel
            .maps_mut()
            .rate_limits()
            .set_max_entries(profiler_config.mapsize_rate_limits)
            .expect("Unable to set rate_limits map max_entries");
        open_skel
            .rodata_mut()
            .lightswitch_config
            .verbose_logging
            .write(profiler_config.bpf_logging);
        let bpf = open_skel.load().expect("load skel");
        info!("native unwinder BPF program loaded");
        let native_unwinder_maps = bpf.maps();
        let exec_mappings_fd = native_unwinder_maps.exec_mappings().as_fd();

        // If mapsize_info requested, pull the max_entries from each map of
        // interest and print out
        if profiler_config.mapsize_info {
            info!("eBPF ACTUAL map size Configuration:");
            info!(
                "stacks:             {}",
                bpf.maps().stacks().info().unwrap().info.max_entries
            );
            info!(
                "aggregated_stacks:  {}",
                bpf.maps()
                    .aggregated_stacks()
                    .info()
                    .unwrap()
                    .info
                    .max_entries
            );
            info!(
                "unwind_info_chunks: {}",
                bpf.maps()
                    .unwind_info_chunks()
                    .info()
                    .unwrap()
                    .info
                    .max_entries
            );
            info!(
                "unwind_tables:      {}",
                bpf.maps().unwind_tables().info().unwrap().info.max_entries
            );
            info!(
                "rate_limits:        {}",
                bpf.maps().rate_limits().info().unwrap().info.max_entries
            );
        }

        let mut tracers_builder = TracersSkelBuilder::default();
        tracers_builder
            .obj_builder
            .debug(profiler_config.libbpf_debug);
        let open_tracers = tracers_builder.open().expect("open skel");
        open_tracers
            .maps()
            .exec_mappings()
            .reuse_fd(exec_mappings_fd)
            .expect("reuse exec_mappings");

        let tracers = open_tracers.load().expect("load skel");
        info!("munmap and process exit tracing BPF programs loaded");

        let procs = Arc::new(Mutex::new(HashMap::new()));
        let object_files = Arc::new(Mutex::new(HashMap::new()));

        let (sender, receiver) = unbounded();
        let chan_send = Arc::new(sender);
        let chan_receive = Arc::new(receiver);

        let (sender, receiver) = unbounded();
        let tracers_chan_send = Arc::new(sender);
        let tracers_chan_receive = Arc::new(receiver);

        let native_unwind_state = NativeUnwindState::default();

        let (sender, receiver) = unbounded();
        let profile_send = Arc::new(sender);
        let profile_receive = Arc::new(receiver);

        let filter_pids = HashMap::new();

        Profiler {
            _links: Vec::new(),
            bpf,
            tracers,
            procs,
            object_files,
            new_proc_chan_send: chan_send,
            new_proc_chan_receive: chan_receive,
            tracers_chan_send,
            tracers_chan_receive,
            stop_chan_receive: stop_signal_receive,
            native_unwind_state,
            filter_pids,
            profile_send,
            profile_receive,
            duration,
            sample_freq,
            perf_buffer_bytes,
            session_duration: Duration::from_secs(5),
        }
    }

    pub fn profile_pids(&mut self, pids: Vec<i32>) {
        for pid in pids {
            self.filter_pids.insert(pid, true);
            self.event_new_proc(pid);
        }
    }

    pub fn send_profile(&mut self, profile: RawAggregatedProfile) {
        self.profile_send.send(profile).expect("handle send");
    }

    pub fn run(mut self, collector: ThreadSafeCollector) {
        // In this case, we only want to calculate maximum sampling buffer sizes based on the
        // number of online CPUs, NOT possible CPUs, when they differ - which is often.
        let num_cpus = get_online_cpus().expect("get online CPUs").len() as u64;
        let max_samples_per_session =
            self.sample_freq as u64 * num_cpus * self.session_duration.as_secs();
        if max_samples_per_session >= MAX_AGGREGATED_STACKS_ENTRIES.into() {
            warn!("samples might be lost due to too many samples in a profile session");
        }

        self.setup_perf_events();
        self.set_bpf_map_info();

        self.tracers.attach().expect("attach tracers");

        // New process events.
        let chan_send = self.new_proc_chan_send.clone();
        let perf_buffer = PerfBufferBuilder::new(self.bpf.maps().events())
            .pages(self.perf_buffer_bytes / page_size::get())
            .sample_cb(move |_cpu: i32, data: &[u8]| {
                Self::handle_event(&chan_send, data);
            })
            .lost_cb(Self::handle_lost_events)
            .build()
            // TODO: Instead of unwrap, consume and emit any error, with
            // .expect() perhaps?
            .unwrap();

        let _poll_thread = thread::spawn(move || loop {
            perf_buffer.poll(Duration::from_millis(100)).expect("poll");
        });

        // Trace events are received here, such as memory unmaps.
        let tracers_send = self.tracers_chan_send.clone();
        let tracers_events_perf_buffer =
            PerfBufferBuilder::new(self.tracers.maps().tracer_events())
                .pages(self.perf_buffer_bytes / page_size::get())
                .sample_cb(move |_cpu: i32, data: &[u8]| {
                    let mut event = tracer_event_t::default();
                    plain::copy_from_bytes(&mut event, data).expect("serde tracers event");
                    tracers_send
                        .send(TracerEvent::from(event))
                        .expect("handle event send");
                })
                .lost_cb(|_cpu, lost_count| {
                    warn!("lost {} events from the tracers", lost_count);
                })
                .build()
                // TODO: Instead of unwrap, consume and emit any error, with
                // .expect() perhaps?
                .unwrap();

        let _tracers_poll_thread = thread::spawn(move || loop {
            tracers_events_perf_buffer
                .poll(Duration::from_millis(100))
                .expect("poll");
        });

        let profile_receive = self.profile_receive.clone();
        let procs = self.procs.clone();
        let object_files = self.object_files.clone();
        let collector = collector.clone();

        thread::spawn(move || loop {
            match profile_receive.recv() {
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

        let total_duration_tick = tick(self.duration);
        let session_tick = tick(self.session_duration);
        let persist_ticks = tick(Duration::from_millis(100));

        loop {
            select! {
                recv(self.stop_chan_receive) -> _ => {
                    debug!("received ctrl+c");
                    let profile = self.collect_profile();
                    self.send_profile(profile);
                    break;
                },
                recv(total_duration_tick) -> _ => {
                    debug!("done profiling");
                    let profile = self.collect_profile();
                    self.send_profile(profile);
                    break;
                },
                recv(session_tick) -> _ => {
                    debug!("collecting profiles on schedule");
                    let profile = self.collect_profile();
                    self.send_profile(profile);
                }
                recv(self.tracers_chan_receive) -> read => {
                        match read {
                            Ok(TracerEvent::Munmap(pid, start_address)) => {
                                self.handle_munmap(pid, start_address);
                            }
                            Ok(TracerEvent::ProcessExit(pid)) => {
                                self.handle_process_exit(pid);
                            }
                            Err(_) => {}
                        }
                },
                recv(self.new_proc_chan_receive) -> read => {
                        if let Ok(event) = read {
                            if event.type_ == event_type_EVENT_NEW_PROCESS {
                                self.event_new_proc(event.pid);
                                // Ensure we only remove the rate limits only if the above works.
                                // This is probably suited for a batched operation.
                                // let _ = self
                                //    .bpf
                                //    .maps()
                                //    .rate_limits()
                                //    .delete(unsafe { plain::as_bytes(&event) });
                            } else {
                                error!("unknown event type {}", event.type_);
                            }
                        }
                    },
                recv(persist_ticks) -> _ => {
                    if self.native_unwind_state.dirty && self.persist_unwind_info(&self.native_unwind_state.live_shard) {
                        self.native_unwind_state.dirty = false;
                    }
                },
                default(Duration::from_millis(100)) => {},
            }
        }
    }

    pub fn handle_process_exit(&self, pid: i32) {
        // TODO: remove ratelimits for this process.
        let mut procs = self.procs.lock().expect("lock");
        match procs.get_mut(&pid) {
            Some(proc_info) => {
                debug!("marking process {} as exited", pid);
                proc_info.status = ProcessStatus::Exited;
                for mapping in &mut proc_info.mappings.0 {
                    let mut object_files = self.object_files.lock().expect("lock");
                    mapping.mark_as_deleted(&mut object_files);
                }
            }
            None => {
                debug!("could not find process {} while marking as exited", pid);
            }
        }
    }

    pub fn handle_munmap(&self, pid: i32, start_address: u64) {
        let mut procs = self.procs.lock().expect("lock");

        match procs.get_mut(&pid) {
            Some(proc_info) => {
                for mapping in &mut proc_info.mappings.0 {
                    if mapping.start_addr <= start_address && start_address <= mapping.end_addr {
                        debug!("found memory mapping starting at {:x} for pid {} while handling munmap", start_address, pid);
                        let mut object_files = self.object_files.lock().expect("lock");
                        mapping.mark_as_deleted(&mut object_files);
                    }
                }

                debug!(
                    "could not find memory mapping starting at {:x} for pid {} while handling munmap",
                    start_address, pid
                );
            }
            None => {
                debug!("could not find pid {} while handling munmap", pid);
            }
        }
    }

    /// Clears a BPF map in a iterator-stable way.
    pub fn clear_map(&self, name: &str) {
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

    pub fn clear_stats_map(&self) {
        let key = 0_u32.to_le_bytes();
        let default = unwinder_stats_t::default();
        let value = unsafe { plain::as_bytes(&default) };

        let mut values: Vec<Vec<u8>> = Vec::new();
        // This is a place where you need to know the POSSIBLE, not ONLINE CPUs, because eBPF's
        // internals require setting up certain buffers for all possible CPUs, even if the CPUs
        // don't all exist.
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

        self.clear_map("stacks");
        self.clear_map("aggregated_stacks");
        self.clear_map("rate_limits");
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
                            Ok(None) => {
                                warn!("NO USER STACK FOUND");
                            }
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
                        pid: key.pid,
                        tid: key.task_id,
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

    fn persist_unwind_info(&self, live_shard: &Vec<CompactUnwindRow>) -> bool {
        let _span = span!(Level::DEBUG, "persist_unwind_info").entered();
        let mut bpf_unwind_info: Vec<stack_unwind_row_t> =
            Vec::with_capacity(live_shard.capacity());
        for row in live_shard {
            bpf_unwind_info.push(row.into());
        }

        let key = self.native_unwind_state.shard_index.to_ne_bytes();
        let val = unsafe {
            // Probs we need to zero this mem?
            std::slice::from_raw_parts(
                bpf_unwind_info.as_ptr() as *const u8,
                bpf_unwind_info.capacity() * ::std::mem::size_of::<stack_unwind_row_t>(),
            )
        };

        match self
            .bpf
            .maps()
            .unwind_tables()
            .update(&key, val, MapFlags::ANY)
        {
            Ok(_) => {
                debug!("unwind info persisted succesfully");
                true
            }
            Err(e) => {
                warn!("failed to persist unwind info with {:?}", e);
                false
            }
        }
    }

    fn add_bpf_mapping(
        &mut self,
        key: &exec_mappings_key,
        value: &mapping_t,
    ) -> Result<(), libbpf_rs::Error> {
        self.bpf.maps().exec_mappings().update(
            unsafe { plain::as_bytes(key) },
            unsafe { plain::as_bytes(value) },
            MapFlags::ANY,
        )
    }

    fn add_unwind_info(&mut self, pid: i32) {
        if !self.process_is_known(pid) {
            panic!("add_unwind_info -- expected process to be known");
        }

        // Local unwind info state
        let mut mappings = Vec::with_capacity(MAX_MAPPINGS_PER_PROCESS as usize);

        // Get unwind info
        for mapping in self
            .procs
            .lock()
            .expect("lock")
            .get(&pid)
            .unwrap()
            .mappings
            .0
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
                    continue;
                }
                MappingType::FileBacked => {
                    // Handled below
                }
            }

            if mapping.build_id.is_none() {
                panic!("build id should not be none for file backed mappings");
            }

            let object_files = self.object_files.lock().unwrap();

            // We might know about a mapping that failed to open for some reason.
            let object_file_info = object_files.get(&mapping.executable_id);
            if object_file_info.is_none() {
                warn!("mapping not found");
                continue;
            }
            let object_file_info = object_file_info.unwrap();
            let obj_path = object_file_info.path.clone();

            // TODO: rework this logic as it's quite kludgy at the moment and this is broken with
            // some loaders. Particularly, Rust statically linked with musl does not work. We must
            // ensure everything works with ASLR enabled loading as well.
            let mut load_address = 0;
            if mapping.main_exec {
                if object_file_info.is_dyn {
                    load_address = mapping.load_address;
                }
            } else {
                load_address = mapping.load_address;
            }

            // Avoid deadlock
            std::mem::drop(object_files);

            match self
                .native_unwind_state
                .known_executables
                .get(&mapping.executable_id)
            {
                Some(_) => {
                    // == Add mapping
                    mappings.push(mapping_t {
                        executable_id: mapping.executable_id,
                        load_address,
                        begin: mapping.start_addr,
                        end: mapping.end_addr,
                        type_: 0, // normal i think
                    });
                    debug!("unwind info CACHED for executable {:?}", obj_path);
                    continue;
                }
                None => {
                    debug!("unwind info not found for executable {:?}", obj_path);
                }
            }

            let mut chunks = Vec::with_capacity(MAX_CHUNKS);

            // == Add mapping
            mappings.push(mapping_t {
                load_address,
                begin: mapping.start_addr,
                end: mapping.end_addr,
                executable_id: mapping.executable_id,
                type_: 0, // normal i think
            });

            // This is not released (see note "deadlock")
            let first_mapping_ = self.object_files.lock().unwrap();
            let first_mapping = first_mapping_.get(&mapping.executable_id).unwrap();

            // == Fetch unwind info, so far, this is in mem
            // todo, pass file handle
            let span = span!(
                Level::DEBUG,
                "calling in_memory_unwind_info",
                "{}",
                first_mapping.path.to_string_lossy()
            )
            .entered();

            let mut found_unwind_info: Vec<CompactUnwindRow>;

            match in_memory_unwind_info(&first_mapping.path.to_string_lossy()) {
                Ok(unwind_info) => {
                    found_unwind_info = unwind_info;
                }
                Err(e) => {
                    warn!(
                        "failed to get unwind information for {} with {:?}",
                        first_mapping.path.to_string_lossy(),
                        e
                    );

                    use crate::unwind_info::log_unwind_info_sections;
                    if let Err(e) = log_unwind_info_sections(&first_mapping.path) {
                        info!("log_unwind_info_sections failed with {}", e);
                    }
                    continue;
                }
            }
            span.exit();

            let span: span::EnteredSpan = span!(Level::DEBUG, "optimize unwind info").entered();
            remove_unnecesary_markers(&mut found_unwind_info);
            remove_redundant(&mut found_unwind_info);
            span.exit();

            debug!(
                "======== Unwind rows for executable {}: {} with id {}",
                obj_path.display(),
                &found_unwind_info.len(),
                self.native_unwind_state.known_executables.len(),
            );

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

                    // TODO: Avoid resetting the state too often.
                    self.native_unwind_state = NativeUnwindState::default();

                    // With the current implementation of the unwind information reset, we might
                    // wipe stacks that we would like to read from userspace. We could fix this by
                    // resetting after a profiling session is done.
                    self.clear_map("stacks");
                    self.clear_map("aggregated_stacks");
                    self.clear_map("unwind_info_chunks");
                    self.clear_map("exec_mappings");

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
                    info!(
                        "live shard is full, allocating a new one [{}/{}]",
                        self.native_unwind_state.shard_index, MAX_SHARDS
                    );

                    if self.persist_unwind_info(&self.native_unwind_state.live_shard) {
                        self.native_unwind_state.dirty = false;
                        self.native_unwind_state.live_shard.truncate(0);
                        self.native_unwind_state.shard_index += 1;
                    }
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
            if chunks.len() > MAX_CHUNKS {
                error!(
                    "maximum allowed chunks {} but found {}",
                    MAX_CHUNKS,
                    chunks.len()
                );
            }

            chunks.resize(MAX_CHUNKS, chunk_info_t::default());

            let all_chunks_boxed: Box<[chunk_info_t; MAX_CHUNKS]> =
                chunks.try_into().expect("try into");

            let all_chunks = unwind_info_chunks_t {
                chunks: *all_chunks_boxed,
            };
            self.bpf
                .maps()
                .unwind_info_chunks()
                .update(
                    &mapping.executable_id.to_ne_bytes(),
                    unsafe { plain::as_bytes(&all_chunks) },
                    MapFlags::ANY,
                )
                .unwrap();

            self.native_unwind_state
                .known_executables
                .insert(mapping.executable_id);
        } // Added all mappings

        self.native_unwind_state.dirty = true;

        // Add entry just with the pid to signal processes that we already know about.
        let key = exec_mappings_key::new(
            pid.try_into().unwrap(),
            0x0,
            32, // pid bits
        );
        self.add_bpf_mapping(&key, &mapping_t::default()).unwrap();

        // Add process info
        for mapping in mappings {
            for address_range in summarize_address_range(mapping.begin, mapping.end - 1) {
                let key = exec_mappings_key::new(
                    pid.try_into().unwrap(),
                    address_range.addr,
                    32 + address_range.prefix_len,
                );

                self.add_bpf_mapping(&key, &mapping).unwrap();
            }
        }
    }

    fn should_profile(&self, pid: i32) -> bool {
        if self.filter_pids.is_empty() {
            return true;
        }

        self.filter_pids.contains_key(&pid)
    }

    fn event_new_proc(&mut self, pid: i32) {
        if !self.should_profile(pid) {
            return;
        }

        if self.process_is_known(pid) {
            // We hit this when we had to reset the state of the BPF maps but we know about this process.
            self.add_unwind_info(pid);
            return;
        }

        match self.add_proc(pid) {
            Ok(()) => {
                self.add_unwind_info(pid);
            }
            Err(_e) => {
                // probabaly a procfs race
            }
        }
    }

    pub fn add_proc(&mut self, pid: i32) -> anyhow::Result<()> {
        let proc = procfs::process::Process::new(pid)?;
        let maps = proc.maps()?;

        let mut mappings = vec![];
        let object_files_clone = self.object_files.clone();

        for map in maps.iter() {
            if !map.perms.contains(procfs::process::MMPermissions::EXECUTE) {
                continue;
            }
            match &map.pathname {
                procfs::process::MMapPath::Path(path) => {
                    let mut abs_path = proc.exe()?;
                    abs_path.push("/root");
                    abs_path.push(path);

                    // We've seen debug info executables that get deleted in Rust applications.
                    // There are probably other cases, but we'll handle them as we bump into them.
                    if abs_path.to_str().unwrap().contains("(deleted)") {
                        continue;
                    }

                    // We want to open the file as quickly as possible to minimise the chances of races
                    // if the file is deleted.
                    let file = match fs::File::open(&abs_path) {
                        Ok(f) => f,
                        Err(e) => {
                            warn!("failed to open file {} due to {:?}", abs_path.display(), e);
                            // Rather than returning here, we prefer to be able to profile some
                            // parts of the binary
                            continue;
                        }
                    };

                    let object_file = match ObjectFile::new(&abs_path) {
                        Ok(f) => f,
                        Err(e) => {
                            warn!("object_file {} failed with {:?}", abs_path.display(), e);
                            // Rather than returning here, we prefer to be able to profile some
                            // parts of the binary
                            continue;
                        }
                    };

                    // Disable profiling Go applications as they are not properly supported yet.
                    // Among other things, blazesym doesn't support symbolizing Go binaries.
                    if object_file.is_go() {
                        // todo: deal with CGO and friends
                        return Err(anyhow!("Go applications are not supported yet"));
                    }

                    let Ok(build_id) = object_file.build_id() else {
                        continue;
                    };

                    let Ok(executable_id) = object_file.id() else {
                        debug!("could not get id for object file: {:?}", abs_path);
                        continue;
                    };

                    let load_address = || {
                        for map2 in maps.iter() {
                            if map2.pathname == map.pathname {
                                return map2.address.0;
                            }
                        }
                        map.address.0
                    };

                    let mut object_files = object_files_clone.lock().expect("lock object_files");
                    let main_exec = mappings.is_empty();

                    mappings.push(ExecutableMapping {
                        executable_id,
                        build_id: Some(build_id.clone()),
                        kind: MappingType::FileBacked,
                        start_addr: map.address.0,
                        end_addr: map.address.1,
                        offset: map.offset,
                        load_address: load_address(),
                        main_exec,
                        unmapped: false,
                    });

                    match object_files.entry(executable_id) {
                        Entry::Vacant(entry) => match object_file.elf_load() {
                            Ok(elf_load) => {
                                entry.insert(ObjectFileInfo {
                                    path: abs_path,
                                    file,
                                    load_offset: elf_load.offset,
                                    load_vaddr: elf_load.vaddr,
                                    is_dyn: object_file.is_dynamic(),
                                    references: 1,
                                });
                            }
                            Err(e) => {
                                warn!("elf_load() failed with {:?}", e);
                            }
                        },
                        Entry::Occupied(mut entry) => {
                            entry.get_mut().references += 1;
                        }
                    }
                }
                procfs::process::MMapPath::Anonymous => {
                    mappings.push(ExecutableMapping {
                        executable_id: 0, // Placeholder for JIT.
                        build_id: None,
                        kind: MappingType::Anonymous,
                        start_addr: map.address.0,
                        end_addr: map.address.1,
                        offset: map.offset,
                        load_address: 0,
                        main_exec: false,
                        unmapped: false,
                    });
                }
                procfs::process::MMapPath::Vsyscall
                | procfs::process::MMapPath::Vdso
                | procfs::process::MMapPath::Vsys(_)
                | procfs::process::MMapPath::Vvar => {
                    mappings.push(ExecutableMapping {
                        executable_id: 0, // Placeholder for vDSO.
                        build_id: None,
                        kind: MappingType::Vdso,
                        start_addr: map.address.0,
                        end_addr: map.address.1,
                        offset: map.offset,
                        load_address: 0,
                        main_exec: false,
                        unmapped: false,
                    });
                }
                _ => {}
            }
        }

        mappings.sort_by_key(|k| k.start_addr.cmp(&k.start_addr));
        let proc_info = ProcessInfo {
            status: ProcessStatus::Running,
            mappings: ExecutableMappings(mappings),
        };
        self.procs
            .clone()
            .lock()
            .expect("lock")
            .insert(pid, proc_info);

        Ok(())
    }

    fn handle_event(sender: &Arc<Sender<Event>>, data: &[u8]) {
        let event = plain::from_bytes(data).expect("handle event serde");
        sender.send(*event).expect("handle event send");
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
        for i in get_online_cpus().expect("get online CPUs") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_raw_aggregated_sample() {
        let addrs = [0; 127];

        // User stack but no kernel stack
        let mut ustack = addrs;
        ustack[0] = 0xffff;
        ustack[1] = 0xdeadbeef;

        let ustack_data = Some(native_stack_t {
            addresses: ustack,
            len: 2,
        });

        let sample = RawAggregatedSample {
            pid: 1234,
            tid: 1235,
            ustack: ustack_data,
            kstack: None,
            count: 1,
        };
        insta::assert_yaml_snapshot!(format!("{}", sample), @r###"
        ---
        "RawAggregatedSample { pid: 1234, tid: 1235, ustack: \"[  0: 0x000000000000ffff,  1: 0x00000000deadbeef]\", kstack: \"[NONE]\", count: 1 }"
        "###);

        // No user or kernel stacks
        let sample = RawAggregatedSample {
            pid: 1234,
            tid: 1235,
            ustack: None,
            kstack: None,
            count: 1,
        };
        insta::assert_yaml_snapshot!(format!("{}", sample), @r###"
        ---
        "RawAggregatedSample { pid: 1234, tid: 1235, ustack: \"[NONE]\", kstack: \"[NONE]\", count: 1 }"
        "###);

        // user and kernel stacks
        let mut ustack = addrs;
        let ureplace: &[u64] = &[
            0x007f7c91c82314,
            0x007f7c91c4ff93,
            0x007f7c91c5d8ae,
            0x007f7c91c4d2c3,
            0x007f7c91c45400,
            0x007f7c91c10933,
            0x007f7c91c38153,
            0x007f7c91c331d9,
            0x007f7c91dfa501,
            0x007f7c91c16b05,
            0x007f7c91e22038,
            0x007f7c91e23fc6,
        ];
        ustack[..ureplace.len()].copy_from_slice(ureplace);

        let mut kstack = addrs;
        let kreplace: &[u64] = &[
            0xffffffff8749ae51,
            0xffffffffc04c4804,
            0xffffffff874ddfd0,
            0xffffffff874e0843,
            0xffffffff874e0b8a,
            0xffffffff8727f600,
            0xffffffff8727f8a7,
            0xffffffff87e0116e,
        ];
        kstack[..kreplace.len()].copy_from_slice(kreplace);

        let ustack_data = Some(native_stack_t {
            addresses: ustack,
            len: ureplace.len() as u64,
        });
        let kstack_data = Some(native_stack_t {
            addresses: kstack,
            len: kreplace.len() as u64,
        });

        let sample = RawAggregatedSample {
            pid: 128821,
            tid: 128822,
            ustack: ustack_data,
            kstack: kstack_data,
            count: 42,
        };
        insta::assert_yaml_snapshot!(format!("{}", sample), @r###"
        ---
        "RawAggregatedSample { pid: 128821, tid: 128822, ustack: \"[  0: 0x00007f7c91c82314,  1: 0x00007f7c91c4ff93,  2: 0x00007f7c91c5d8ae,  3: 0x00007f7c91c4d2c3,  4: 0x00007f7c91c45400,  5: 0x00007f7c91c10933,  6: 0x00007f7c91c38153,  7: 0x00007f7c91c331d9,  8: 0x00007f7c91dfa501,  9: 0x00007f7c91c16b05, 10: 0x00007f7c91e22038, 11: 0x00007f7c91e23fc6]\", kstack: \"[  0: 0xffffffff8749ae51,  1: 0xffffffffc04c4804,  2: 0xffffffff874ddfd0,  3: 0xffffffff874e0843,  4: 0xffffffff874e0b8a,  5: 0xffffffff8727f600,  6: 0xffffffff8727f8a7,  7: 0xffffffff87e0116e]\", count: 42 }"
        "###);
    }

    #[test]
    fn display_symbolized_aggregated_sample() {
        let ustack_data: Vec<_> = ["ufunc3", "ufunc2", "ufunc1"]
            .into_iter()
            .map(|s| Frame {
                virtual_address: 0x0,
                file_offset: None,
                name: s.to_string(),
                inline: false,
            })
            .collect();
        let kstack_data: Vec<_> = ["kfunc2", "kfunc1"]
            .into_iter()
            .map(|s| Frame {
                virtual_address: 0x0,
                file_offset: None,
                name: s.to_string(),
                inline: false,
            })
            .collect();

        let sample = SymbolizedAggregatedSample {
            pid: 1234567,
            tid: 1234568,
            ustack: ustack_data,
            kstack: kstack_data.clone(),
            count: 128,
        };
        insta::assert_yaml_snapshot!(format!("{}", sample), @r###"
        ---
        "SymbolizedAggregatedSample { pid: 1234567, tid: 1234568, ustack: \"[  0: ufunc3,  1: ufunc2,  2: ufunc1]\", kstack: \"[  0: kfunc2,  1: kfunc1]\", count: 128 }"
        "###);

        let ustack_data = vec![];

        let sample = SymbolizedAggregatedSample {
            pid: 98765,
            tid: 98766,
            ustack: ustack_data,
            kstack: kstack_data.clone(),
            count: 1001,
        };
        insta::assert_yaml_snapshot!(format!("{}", sample), @r###"
        ---
        "SymbolizedAggregatedSample { pid: 98765, tid: 98766, ustack: \"[NONE]\", kstack: \"[  0: kfunc2,  1: kfunc1]\", count: 1001 }"
        "###);
    }

    /// This tests ensures that cloning an `ObjectFileInfo` succeeds to
    /// open the file even if it's been deleted. This works because we
    /// always keep at least one open file descriptor to prevent the kernel
    /// from freeing the resource, effectively removing the file from the
    /// file system.
    #[test]
    fn test_object_file_clone() {
        use std::fs::remove_file;
        use std::io::Read;

        let named_tmpfile = tempfile::NamedTempFile::new().unwrap();
        let file_path = named_tmpfile.path();
        let file = File::open(file_path).unwrap();

        let object_file_info = ObjectFileInfo {
            file,
            path: file_path.to_path_buf(),
            load_offset: 0,
            load_vaddr: 0,
            is_dyn: false,
            references: 1,
        };

        remove_file(file_path).unwrap();

        let mut object_file_info_copy = object_file_info.clone();
        let mut buf = String::new();
        // This would fail without the procfs hack.
        object_file_info_copy.file.read_to_string(&mut buf).unwrap();
    }
}
