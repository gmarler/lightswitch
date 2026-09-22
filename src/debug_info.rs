use std::fs::File;
use std::io::BufReader;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::anyhow;
use lru::LruCache;
use parking_lot::Mutex;
use reqwest::StatusCode;
use tracing::{debug, instrument};

use lightswitch_object::BuildId;
use lightswitch_object::ExecutableId;

use crate::NAME_AND_VERSION;

/// Handles with debug information.
///
/// This currently experimental, not feature-complete and not used yet during
/// symbolization. The end goal would be to keep track of every debug info
/// that's either present locally or remotely (depending on configuration),
/// while minimizing the number of open FDs, file copies, and race condition
/// windows.
pub trait DebugInfoManager {
    fn add_if_not_present(
        &self,
        name: &str,
        build_id: &BuildId,
        debug_info: &Path,
    ) -> anyhow::Result<()>;
    fn debug_info_path(&self) -> Option<PathBuf>;
}

pub struct DebugInfoBackendNull {}
impl DebugInfoManager for DebugInfoBackendNull {
    fn add_if_not_present(
        &self,
        _name: &str,
        _build_id: &BuildId,
        _debug_info: &Path,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn debug_info_path(&self) -> Option<PathBuf> {
        None
    }
}

#[derive(Debug)]
pub struct DebugInfoBackendFilesystem {
    pub path: PathBuf,
}
impl DebugInfoManager for DebugInfoBackendFilesystem {
    #[instrument]
    fn add_if_not_present(
        &self,
        _name: &str,
        build_id: &BuildId,
        debug_info: &Path,
    ) -> anyhow::Result<()> {
        // try to find, else extract
        if self.find_in_fs(build_id) {
            return Ok(());
        }

        self.add_to_fs(build_id, debug_info)
    }

    fn debug_info_path(&self) -> Option<PathBuf> {
        todo!()
    }
}

impl DebugInfoBackendFilesystem {
    fn find_in_fs(&self, build_id: &BuildId) -> bool {
        self.path.join(build_id.to_string()).exists()
    }

    fn add_to_fs(&self, build_id: &BuildId, debug_info: &Path) -> anyhow::Result<()> {
        // TODO: add support for other methods beyond copying. For example
        // hardlinks could be used and only fall back to copying if the src
        // and dst filesystems differ.
        let mut reader = BufReader::new(File::open(debug_info)?);
        let mut writer = std::fs::File::create(self.path.join(build_id.to_string()))?;
        std::io::copy(&mut reader, &mut writer)?;
        Ok(())
    }
}

/// What the backend told us about a build ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendQuery {
    /// The backend has it. Nothing to do.
    Present,
    /// The backend definitely does not have it. Upload.
    Absent,
    /// We do not know: the backend is shedding load, broken, or unreachable.
    /// Do *not* upload — a multi-megabyte POST chasing every failed query is
    /// how a slow backend can turn into a dead one.
    Unavailable,
}

/// Map the response status of a debuginfo query onto what it tells us.
///
/// Only 404 is taken as a definitive "absent". A 401/403 would reject the
/// upload too, and an unrecognised status is not evidence of anything, so both
/// back off rather than uploading.
///
/// A `Retry-After` header, where the backend sends one, is deliberately
/// ignored: nothing here acts on a delay yet, so parsing it would be dead code.
fn classify(status: StatusCode) -> BackendQuery {
    if status.is_success() {
        BackendQuery::Present
    } else if status == StatusCode::NOT_FOUND {
        BackendQuery::Absent
    } else {
        BackendQuery::Unavailable
    }
}

/// The max number of build IDs to remember.
///
/// Using `NonZeroUsize` because that is what `LruCache::new` and
/// `LruCache::sparse` take.
/// TODO: Make configurable via the cli
const DEBUG_INFO_CACHE_ENTRIES: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// How long to wait before asking again, indexed by the number of attempts
/// that have already failed.
///
/// The last entry repeats for every failure after it. A backend down for 15
/// minutes is a backend the whole fleet is already looking for.
const BACKOFF_SECS: [u32; 6] = [30, 60, 120, 240, 480, 900];
/// Width of the jitter, in seconds. 0-15 is enough to break up a fleet that
/// would otherwise retry in lockstep.
const BACKOFF_JITTER_MASK: u32 = 0xF;

/// What we know about a build ID.
///
/// Deliberately `Copy` and 8 bytes wide. The deadline is a `u32` count of
/// seconds since [`DebugInfoBackendRemote::started_at`] rather than an
/// `Instant`, which is 16 bytes on Linux and would double the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DebugInfoStatus {
    /// The backend has it. Never ask again.
    Known,
    /// The last attempt failed. Do not ask again before `retry_at_secs`.
    Backoff { retry_at_secs: u32, attempts: u8 },
}

/// How long to wait after `attempts` consecutive failures, plus `jitter_secs`.
///
/// Taken out of the cache so it can be checked without one.
fn next_backoff(attempts: u8, jitter_secs: u32) -> u32 {
    let step = usize::from(attempts).min(BACKOFF_SECS.len() - 1);
    BACKOFF_SECS[step] + jitter_secs
}

/// A per-process seed for the jitter.
///
/// XOR'ing the pid with the current nanoseconds gives every agent a different
/// seed, so hosts across the fleet do not all retry at the same moment. A seed
/// from the build ID alone would be the same everywhere and wouldn't give us an
/// even spread.
fn jitter_seed() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since_epoch| since_epoch.subsec_nanos())
        .unwrap_or(0);
    std::process::id() ^ nanos
}

#[derive(Debug)]
pub struct DebugInfoBackendRemote {
    pub token: Option<String>,
    pub server_url: String,
    pub query_client: reqwest::blocking::Client,
    pub upload_client: reqwest::blocking::Client,
    /// Build IDs the backend is known to hold, so that the 2nd and later
    /// processes mapping the same executable cost nothing.
    ///
    /// Keyed by [`ExecutableId`] rather than [`BuildId`] to keep each entry
    /// small: it's a bare `u64`, whereas a `BuildId` is an enum plus a
    /// separately allocated `Vec<u8>`. It also helps that it's what the
    /// profiler already uses to track object files.
    ///
    /// A failed attempt is recorded too, as a deadline before which we will
    /// not ask again. Without that, a backend that is down turns every mapped
    /// executable back into a request, which is the load we are trying to
    /// avoid at exactly the moment it hurts most.
    ///
    /// Built with `sparse` rather than `new`: so that we have as few entries as
    /// possible.
    known: Mutex<LruCache<ExecutableId, DebugInfoStatus>>,
    /// Reference point for the `u32` deadlines in [`DebugInfoStatus`].
    started_at: Instant,
    /// Per-process seed for the retry jitter. See [`jitter_seed`].
    jitter_seed: u32,
}

impl DebugInfoBackendRemote {
    pub fn new(
        token: Option<String>,
        server_url: String,
        query_timeout: Duration,
        upload_timeout: Duration,
    ) -> anyhow::Result<Self> {
        Ok(DebugInfoBackendRemote {
            token,
            server_url,
            query_client: reqwest::blocking::Client::builder()
                .timeout(query_timeout)
                .user_agent(NAME_AND_VERSION)
                .build()?,
            upload_client: reqwest::blocking::Client::builder()
                .timeout(upload_timeout)
                .user_agent(NAME_AND_VERSION)
                .build()?,
            known: Mutex::new(LruCache::sparse(DEBUG_INFO_CACHE_ENTRIES)),
            started_at: Instant::now(),
            jitter_seed: jitter_seed(),
        })
    }

    /// Seconds since this backend was built.
    fn now_secs(&self) -> u32 {
        self.started_at.elapsed().as_secs() as u32
    }

    /// The jitter for one build ID: stable for a given id on a given agent,
    /// different on the next agent. No RNG and no extra state.
    fn jitter_for(&self, executable_id: ExecutableId) -> u32 {
        (executable_id.0 as u32 ^ self.jitter_seed) & BACKOFF_JITTER_MASK
    }

    /// What we last recorded about this build ID.
    ///
    /// The lock is held for the lookup alone, never across the request that may
    /// follow. A hit promotes the entry, so executables that keep being used
    /// are the last to be evicted.
    fn status(&self, executable_id: ExecutableId) -> Option<DebugInfoStatus> {
        self.known.lock().get(&executable_id).copied()
    }

    /// Why we should not ask the backend about this build ID.
    ///
    /// Returns a string rather than a bool so the debug log can say which of
    /// the two reasons applied.
    fn suppression_reason(&self, executable_id: ExecutableId) -> Option<&'static str> {
        match self.status(executable_id)? {
            DebugInfoStatus::Known => Some("the backend already has it"),
            DebugInfoStatus::Backoff { retry_at_secs, .. } => {
                (self.now_secs() < retry_at_secs).then_some("the last attempt failed recently")
            }
        }
    }

    fn mark_known(&self, executable_id: ExecutableId) {
        self.known.lock().put(executable_id, DebugInfoStatus::Known);
    }

    /// Record a failed attempt, and return how many had already failed.
    ///
    /// The deadline and the jitter are computed before the lock is taken.
    fn mark_failed(&self, executable_id: ExecutableId) -> u8 {
        let now_secs = self.now_secs();
        let jitter_secs = self.jitter_for(executable_id);

        let mut known = self.known.lock();
        let attempts = match known.get(&executable_id) {
            Some(DebugInfoStatus::Backoff { attempts, .. }) => *attempts,
            _ => 0,
        };
        known.put(
            executable_id,
            DebugInfoStatus::Backoff {
                retry_at_secs: now_secs + next_backoff(attempts, jitter_secs),
                attempts: attempts.saturating_add(1),
            },
        );
        attempts
    }

    /// Note a failed attempt.
    fn record_failure(&self, executable_id: ExecutableId, build_id: &BuildId, what: &str) {
        let attempts = self.mark_failed(executable_id);
        debug!(
            "debuginfo backend {} for {}, {} consecutive failures",
            what,
            build_id,
            attempts + 1
        );
    }
}

impl DebugInfoManager for DebugInfoBackendRemote {
    // `self` is skipped: it holds the bearer token, which has no business in a
    // span, and a `Mutex` whose `Debug` impl try-locks the cache on every call.
    #[instrument(level = "debug", skip(self))]
    fn add_if_not_present(
        &self,
        name: &str,
        build_id: &BuildId,
        debug_info: &Path,
    ) -> anyhow::Result<()> {
        let executable_id = build_id.id()?;

        if let Some(reason) = self.suppression_reason(executable_id) {
            debug!("not asking about {}: {}", build_id, reason);
            return Ok(());
        }

        match self.find_in_backend(build_id) {
            BackendQuery::Present => {
                self.mark_known(executable_id);
                Ok(())
            }
            BackendQuery::Unavailable => {
                // Not an answer, so we record a deadline rather than a
                // result. We ask again later, not once per process that
                // maps this executable.
                self.record_failure(executable_id, build_id, "did not answer");
                Ok(())
            }
            // TODO: do this in another thread.
            BackendQuery::Absent => match self.upload_to_backend(name, build_id, debug_info) {
                // Only mark it known when the upload worked. Otherwise the
                // backend still lacks the debug information, and we would
                // never try again.
                Ok(()) => {
                    self.mark_known(executable_id);
                    Ok(())
                }
                Err(e) => {
                    self.record_failure(executable_id, build_id, "rejected the upload");
                    Err(e)
                }
            },
        }
    }

    fn debug_info_path(&self) -> Option<PathBuf> {
        None
    }
}

impl DebugInfoBackendRemote {
    /// Whether the backend knows about some debug information.
    ///
    /// A transport error or a timeout is reported as
    /// [`BackendQuery::Unavailable`] rather than an error: not being able
    /// to ask is not conclusive evidence that the backend lacks the build ID.
    #[instrument(level = "debug")]
    fn find_in_backend(&self, build_id: &BuildId) -> BackendQuery {
        let response = self
            .query_client
            .get(format!(
                "{}/debuginfo/{}",
                self.server_url.clone(),
                build_id
            ))
            .send();

        let response = match response {
            Ok(response) => response,
            Err(e) => {
                debug!("debuginfo query for {} failed: {}", build_id, e);
                return BackendQuery::Unavailable;
            }
        };

        classify(response.status())
    }

    /// Send the debug information to the backend.
    #[instrument]
    fn upload_to_backend(
        &self,
        name: &str,
        build_id: &BuildId,
        debug_info: &Path,
    ) -> anyhow::Result<()> {
        let mut request = self
            .upload_client
            .post(format!(
                "{}/debuginfo/new/{}/{}",
                self.server_url.clone(),
                name,
                build_id
            ))
            .body(File::open(debug_info)?);

        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }

        let response = request.send()?;

        if !response.status().is_success() {
            return Err(anyhow!("debuginfo upload failed with {:?}", response));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rstest::rstest;

    use BackendQuery::Unavailable;

    #[rstest]
    #[case(StatusCode::OK, BackendQuery::Present)]
    #[case(StatusCode::NO_CONTENT, BackendQuery::Present)]
    #[case(StatusCode::NOT_FOUND, BackendQuery::Absent)]
    // Not evidence of absence: uploading would fail too, or tell us nothing.
    #[case(StatusCode::BAD_REQUEST, Unavailable)]
    #[case(StatusCode::UNAUTHORIZED, Unavailable)]
    #[case(StatusCode::FORBIDDEN, Unavailable)]
    #[case(StatusCode::TOO_MANY_REQUESTS, Unavailable)]
    #[case(StatusCode::INTERNAL_SERVER_ERROR, Unavailable)]
    #[case(StatusCode::BAD_GATEWAY, Unavailable)]
    #[case(StatusCode::SERVICE_UNAVAILABLE, Unavailable)]
    fn classify_maps_status_to_query(#[case] status: StatusCode, #[case] expected: BackendQuery) {
        assert_eq!(classify(status), expected);
    }

    /// The whole point of the tri-state: a status we cannot interpret must not
    /// be treated as "the backend does not have it", because only `Absent`
    /// uploads.
    #[rstest]
    #[case(StatusCode::TOO_MANY_REQUESTS)]
    #[case(StatusCode::INTERNAL_SERVER_ERROR)]
    #[case(StatusCode::SERVICE_UNAVAILABLE)]
    fn an_unanswerable_query_is_never_absent(#[case] status: StatusCode) {
        assert_ne!(classify(status), BackendQuery::Absent);
    }

    #[rstest]
    #[case(0, 0, 30)]
    #[case(1, 0, 60)]
    // Every failure past the end of the table waits the same, longest, time.
    #[case(5, 0, 900)]
    #[case(u8::MAX, 0, 900)]
    // The jitter is added after the cap, so it still spreads out a fleet that
    // has all reached the longest wait.
    #[case(0, 7, 37)]
    #[case(u8::MAX, 15, 915)]
    fn the_backoff_doubles_then_stops(
        #[case] attempts: u8,
        #[case] jitter_secs: u32,
        #[case] expected: u32,
    ) {
        assert_eq!(next_backoff(attempts, jitter_secs), expected);
    }

    mod cache {
        use super::*;

        use std::io::Read;
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::thread;

        /// A backend that answers everything with one status and records the
        /// request line it saw.
        ///
        /// Every reply says `connection: close`, so reqwest cannot reuse a
        /// connection. One request arrives per connection, so the recorded
        /// lines are an exact count of the requests.
        fn canned_server(status: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let address = listener.local_addr().expect("local_addr");
            let seen = Arc::new(Mutex::new(Vec::new()));

            let recorder = seen.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut buffer = [0u8; 1024];
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    if let Some(line) = request.lines().next() {
                        recorder.lock().push(line.to_string());
                    }
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                }
            });

            (format!("http://{address}"), seen)
        }

        fn backend(server_url: String) -> DebugInfoBackendRemote {
            DebugInfoBackendRemote::new(
                None,
                server_url,
                Duration::from_secs(5),
                Duration::from_secs(5),
            )
            .expect("build backend")
        }

        fn build_id(byte: u8) -> BuildId {
            BuildId::gnu_from_bytes(&[byte; 20]).expect("build id")
        }

        #[test]
        fn a_present_build_id_is_asked_about_once() {
            let (url, seen) = canned_server("200 OK");
            let backend = backend(url);
            let build_id = build_id(0xab);

            for _ in 0..3 {
                backend
                    .add_if_not_present("test", &build_id, Path::new("/nonexistent"))
                    .expect("should not error");
            }

            let seen = seen.lock();
            assert_eq!(seen.len(), 1, "expected one request, saw {seen:?}");
            assert!(seen[0].starts_with("GET "), "saw {:?}", seen[0]);
        }

        #[test]
        fn an_unanswered_query_waits_before_asking_again() {
            // A 503 is not an answer, so it is never recorded as one. It does
            // start a backoff: the calls that follow within the wait are
            // skipped, which is the difference between asking again later and
            // asking again immediately.
            let (url, seen) = canned_server("503 Service Unavailable");
            let backend = backend(url);
            let build_id = build_id(0xcd);

            for _ in 0..3 {
                backend
                    .add_if_not_present("test", &build_id, Path::new("/nonexistent"))
                    .expect("unavailable is not an error");
            }

            let seen = seen.lock();
            assert_eq!(seen.len(), 1, "expected one request, saw {seen:?}");
            assert!(
                seen.iter().all(|line| line.starts_with("GET ")),
                "a query the backend could not answer must never be followed by \
                 an upload, saw {seen:?}"
            );
        }

        #[test]
        fn an_expired_backoff_allows_one_more_attempt() {
            // The backoff has to end, or an unreachable backend would never be
            // asked again for the life of the agent.
            let (url, seen) = canned_server("503 Service Unavailable");
            let backend = backend(url);
            let build_id = build_id(0x11);
            let executable_id = build_id.id().expect("executable id");

            backend
                .add_if_not_present("test", &build_id, Path::new("/nonexistent"))
                .expect("unavailable is not an error");
            assert_eq!(seen.lock().len(), 1);

            // Put the deadline in the past so the test need not wait 30
            // seconds for it: zero is before anything `now_secs` can return.
            //
            // Leave the attempt count at 1. The next failure should then wait
            // 60 seconds, the second step. If it waits 30 the schedule
            // restarted, which is the bug this test looks for.
            backend.known.lock().put(
                executable_id,
                DebugInfoStatus::Backoff {
                    retry_at_secs: 0,
                    attempts: 1,
                },
            );

            // The deadline has passed, so this call must reach the backend.
            backend
                .add_if_not_present("test", &build_id, Path::new("/nonexistent"))
                .expect("unavailable is not an error");
            assert_eq!(
                seen.lock().len(),
                2,
                "an expired backoff must allow another attempt"
            );

            // It failed again, so the schedule must advance a step, not
            // start over.
            match backend.status(executable_id) {
                Some(DebugInfoStatus::Backoff {
                    retry_at_secs,
                    attempts,
                }) => {
                    assert_eq!(attempts, 2, "both failures must be counted");
                    assert!(
                        retry_at_secs >= next_backoff(1, 0),
                        "the wait must have advanced to the second step, got {retry_at_secs}"
                    );
                }
                other => panic!("expected a backoff, got {other:?}"),
            }
        }

        #[test]
        fn a_failed_upload_is_recorded_as_a_failure() {
            // 404 routes to the upload, which cannot succeed because the path
            // does not exist. The backend still lacks this build ID, so it must
            // be recorded as a failure to retry, never as present.
            let (url, _seen) = canned_server("404 Not Found");
            let backend = backend(url);
            let build_id = build_id(0xef);
            let executable_id = build_id.id().expect("executable id");

            assert!(
                backend
                    .add_if_not_present("test", &build_id, Path::new("/nonexistent"))
                    .is_err(),
                "the upload must fail, so there is a failure to record"
            );
            assert!(
                matches!(
                    backend.status(executable_id),
                    Some(DebugInfoStatus::Backoff { attempts: 1, .. })
                ),
                "a failed upload must be recorded as a first failure, got {:?}",
                backend.status(executable_id)
            );
        }

        #[test]
        fn a_full_cache_evicts_the_least_recently_used() {
            let backend = backend("http://127.0.0.1:1".to_string());

            let capacity = DEBUG_INFO_CACHE_ENTRIES.get() as u64;
            let first = ExecutableId(0);
            for i in 0..capacity {
                backend.mark_known(ExecutableId(i));
            }
            assert!(
                backend.status(first).is_some(),
                "nothing must be evicted at exactly capacity"
            );

            // One past capacity evicts the least recently used. The lookup
            // above promoted the first entry, so the second is the one that
            // will be evicted
            backend.mark_known(ExecutableId(capacity));
            assert!(
                backend.status(first).is_some(),
                "the earlier lookup promoted this, so it must survive"
            );
            assert!(
                backend.status(ExecutableId(1)).is_none(),
                "the least recently used entry must be evicted"
            );
        }
    }
}
