use std::fs::File;
use std::io::BufReader;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::anyhow;
use lru::LruCache;
use parking_lot::Mutex;
use reqwest::StatusCode;
use tracing::{debug, instrument, warn};

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
    /// Only positive answers are recorded (until the next PR). A query the
    /// backend could not answer, or an upload that failed does nothing, so
    /// it is retried on the next process that maps the executable.
    ///
    /// Built with `sparse` rather than `new`: so that we have as few entries as
    /// possible.
    known: Mutex<LruCache<ExecutableId, ()>>,
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
        })
    }

    /// Has the backend already told us it holds this build ID?
    ///
    /// The lock is held for the lookup alone, never across the request that may
    /// follow. A hit promotes the entry, so executables that keep being used
    /// are the last to be evicted.
    fn is_known(&self, executable_id: ExecutableId) -> bool {
        self.known.lock().get(&executable_id).is_some()
    }

    fn mark_known(&self, executable_id: ExecutableId) {
        self.known.lock().put(executable_id, ());
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

        if self.is_known(executable_id) {
            debug!("debuginfo for {} is already on the backend", build_id);
            return Ok(());
        }

        match self.find_in_backend(build_id) {
            BackendQuery::Present => {
                self.mark_known(executable_id);
                Ok(())
            }
            BackendQuery::Unavailable => {
                // Deliberately not recorded: an unanswered query is not an
                // answer of any kind, so the next process that maps this
                // executable asks the backend again.
                warn!(
                    "debuginfo backend did not answer for {}, skipping upload",
                    build_id
                );
                Ok(())
            }
            // TODO: do this in another thread.
            BackendQuery::Absent => {
                self.upload_to_backend(name, build_id, debug_info)?;
                // Only after the upload succeeded. A failed upload leaves the
                // backend without the debug information, so recording it as
                // present would prevent every later attempt.
                self.mark_known(executable_id);
                Ok(())
            }
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
        fn an_unanswered_query_is_not_cached() {
            // The backend never said whether it has this build ID, so nothing
            // has been learned and every call must ask again. Caching here
            // would make a transient 503 look like a permanent answer.
            let (url, seen) = canned_server("503 Service Unavailable");
            let backend = backend(url);
            let build_id = build_id(0xcd);

            for _ in 0..3 {
                backend
                    .add_if_not_present("test", &build_id, Path::new("/nonexistent"))
                    .expect("unavailable is not an error");
            }

            let seen = seen.lock();
            assert_eq!(seen.len(), 3, "expected three requests, saw {seen:?}");
            assert!(
                seen.iter().all(|line| line.starts_with("GET ")),
                "a query the backend could not answer must never be followed by \
                 an upload, saw {seen:?}"
            );
        }

        #[test]
        fn a_failed_upload_is_not_cached() {
            // 404 routes to the upload, which cannot succeed because the path
            // does not exist. The backend still lacks this build ID, so it must
            // not be remembered as if it had it.
            let (url, _seen) = canned_server("404 Not Found");
            let backend = backend(url);
            let build_id = build_id(0xef);
            let executable_id = build_id.id().expect("executable id");

            assert!(
                backend
                    .add_if_not_present("test", &build_id, Path::new("/nonexistent"))
                    .is_err(),
                "opening a missing debuginfo file should fail"
            );
            assert!(
                !backend.is_known(executable_id),
                "a failed upload must not be recorded as present"
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
            assert!(backend.is_known(first), "should still fit at capacity");

            // One past capacity evicts the least recently used. The lookup
            // above promoted the first entry, so the second is the one to go.
            backend.mark_known(ExecutableId(capacity));
            assert!(backend.is_known(first), "promoted by the last lookup");
            assert!(
                !backend.is_known(ExecutableId(1)),
                "evicted, so it has to be asked about again"
            );
        }
    }
}
