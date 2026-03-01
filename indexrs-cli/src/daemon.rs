use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

use indexrs_core::HybridDetector;
use indexrs_core::SegmentManager;
use indexrs_core::checkpoint::{Checkpoint, write_checkpoint};
use indexrs_core::error::IndexError;
use indexrs_core::git_diff::GitChangeDetector;
use indexrs_core::search::MatchPattern;

use crate::args::SortOrder;
use crate::color::ColorConfig;
use crate::files::{self, FilesFilter};
use crate::output::{ExitCode, StreamingWriter};
use crate::search_cmd::{self, SearchCmdOptions};

/// Idle timeout before daemon self-terminates.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

/// Maximum time to wait for a spawned daemon to become ready.
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between connection attempts when waiting for daemon startup.
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Request from CLI client to daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRequest {
    Search {
        query: String,
        regex: bool,
        case_sensitive: bool,
        ignore_case: bool,
        limit: usize,
        context_lines: usize,
        language: Option<String>,
        path_glob: Option<String>,
        color: bool,
    },
    Files {
        language: Option<String>,
        path_glob: Option<String>,
        sort: String,
        limit: Option<usize>,
        color: bool,
    },
    Ping,
    Shutdown,
    Reindex,
}

/// Response from daemon to CLI client, one JSON line per message.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonResponse {
    /// A single output line (file path or search match).
    Line { content: String },
    /// End of results with summary.
    Done {
        total: usize,
        duration_ms: u64,
        stale: bool,
    },
    /// Error message.
    Error { message: String },
    /// Ping response.
    Pong,
    /// Progress update (e.g. during reindex).
    Progress { message: String },
}

/// Return the Unix socket path for a given repo root.
pub fn socket_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".indexrs").join("sock")
}

/// Try to connect to a running daemon. Returns None if no daemon is running.
pub async fn try_connect(repo_root: &Path) -> Option<UnixStream> {
    let path = socket_path(repo_root);
    UnixStream::connect(&path).await.ok()
}

/// Run the HybridDetector event loop, applying changes to the index.
///
/// Blocks the calling thread until the detector's channel disconnects
/// (which happens when the detector is dropped). Periodically checks
/// `reindex_flag` and triggers a manual reindex when it is set.
fn run_live_indexing(
    repo_root: &Path,
    indexrs_dir: &Path,
    manager: &std::sync::Arc<SegmentManager>,
    reindex_flag: &AtomicBool,
) -> Result<(), IndexError> {
    let mut detector = HybridDetector::new(repo_root.to_path_buf())?;
    let rx = detector.start()?;

    loop {
        // Check for external reindex requests between batches.
        if reindex_flag.swap(false, Ordering::SeqCst) {
            tracing::info!("external reindex request received");
            detector.reindex();
        }

        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(batch) => {
                if batch.is_empty() {
                    continue;
                }
                tracing::debug!(event_count = batch.len(), "applying live change batch");

                if let Err(e) = manager.apply_changes(repo_root, &batch) {
                    tracing::warn!(error = %e, "failed to apply live changes");
                    continue;
                }

                // Update checkpoint.
                let git = GitChangeDetector::new(repo_root.to_path_buf());
                let git_commit = git.get_head_sha().ok();
                let snapshot = manager.snapshot();
                let file_count: u64 = snapshot.iter().map(|s| s.entry_count() as u64).sum();
                let cp = Checkpoint::new(git_commit, file_count);
                if let Err(e) = write_checkpoint(indexrs_dir, &cp) {
                    tracing::warn!(error = %e, "failed to update checkpoint");
                }

                // Check if compaction needed.
                if manager.should_compact() {
                    tracing::info!("compaction triggered by live changes");
                    drop(manager.compact_background());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    detector.stop();
    Ok(())
}

/// Start a daemon process listening on a Unix domain socket.
///
/// The daemon loads the index from `repo_root/.indexrs/`, listens on the Unix
/// socket, and serves requests until it has been idle for [`IDLE_TIMEOUT`].
pub async fn start_daemon(repo_root: &Path) -> Result<(), IndexError> {
    let sock_path = socket_path(repo_root);

    // Ensure the parent directory exists.
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Remove stale socket file.
    let _ = std::fs::remove_file(&sock_path);

    let listener = UnixListener::bind(&sock_path).map_err(IndexError::Io)?;

    let indexrs_dir = repo_root.join(".indexrs");
    let manager = std::sync::Arc::new(SegmentManager::new(&indexrs_dir)?);
    let caught_up = std::sync::Arc::new(AtomicBool::new(false));
    let reindex_flag = std::sync::Arc::new(AtomicBool::new(false));

    // Spawn background catch-up + live indexing task.
    {
        let mgr = manager.clone();
        let cu = caught_up.clone();
        let rf = reindex_flag.clone();
        let repo = repo_root.to_path_buf();
        let idir = indexrs_dir.clone();
        tokio::spawn(async move {
            // Phase 1: catch-up.
            match tokio::task::spawn_blocking({
                let repo = repo.clone();
                let idir = idir.clone();
                let mgr = mgr.clone();
                move || indexrs_core::run_catchup(&repo, &idir, &mgr)
            })
            .await
            {
                Ok(Ok(changes)) => {
                    if !changes.is_empty() {
                        tracing::info!(
                            change_count = changes.len(),
                            "daemon catch-up applied changes"
                        );
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "daemon catch-up failed");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "daemon catch-up task panicked");
                }
            }
            cu.store(true, Ordering::SeqCst);
            tracing::info!("daemon catch-up complete, starting live watcher");

            // Phase 2: start HybridDetector for live changes.
            // Use std::thread::spawn (not spawn_blocking) so the blocking
            // event loop does not prevent the tokio runtime from shutting
            // down during tests or graceful termination.
            std::thread::spawn({
                let repo = repo.clone();
                let idir = idir.clone();
                let mgr = mgr.clone();
                let rf = rf.clone();
                move || match run_live_indexing(&repo, &idir, &mgr, &rf) {
                    Ok(()) => tracing::debug!("live indexing stopped"),
                    Err(e) => tracing::warn!(error = %e, "live indexing failed"),
                }
            });
        });
    }

    loop {
        match timeout(IDLE_TIMEOUT, listener.accept()).await {
            Ok(Ok((stream, _))) => {
                let mgr = manager.clone();
                let cu = caught_up.clone();
                let repo = repo_root.to_path_buf();
                let idir = indexrs_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &mgr, &cu, &repo, &idir).await {
                        eprintln!("daemon: connection error: {e}");
                    }
                });
            }
            Ok(Err(e)) => {
                eprintln!("daemon: accept error: {e}");
            }
            Err(_) => {
                // Idle timeout — shut down.
                let _ = std::fs::remove_file(&sock_path);
                return Ok(());
            }
        }
    }
}

/// Execute a search against the loaded index and return the result lines with elapsed time.
fn handle_search_request(
    manager: &SegmentManager,
    opts: &SearchCmdOptions,
    color: bool,
) -> Result<(Vec<String>, Duration), String> {
    // Validate regex patterns before searching (the core silently ignores invalid regex).
    if let MatchPattern::Regex(ref pat) = opts.pattern {
        regex::Regex::new(pat).map_err(|e| format!("invalid regex: {e}"))?;
    }

    let start = Instant::now();
    let snapshot = manager.snapshot();
    let color = ColorConfig::new(color);

    let mut buf = Vec::new();
    {
        let mut writer = StreamingWriter::new(&mut buf);
        search_cmd::run_search_streaming(&snapshot, opts, &color, &mut writer)
            .map_err(|e| e.to_string())?;
    }

    let output = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    Ok((lines, start.elapsed()))
}

/// Execute a Files request against the loaded index.
fn handle_files_request(
    manager: &SegmentManager,
    language: Option<String>,
    path_glob: Option<String>,
    sort: String,
    limit: Option<usize>,
    color: bool,
) -> Result<(Vec<String>, Duration), String> {
    let start = Instant::now();
    let snapshot = manager.snapshot();
    let color = ColorConfig::new(color);

    let sort_order = match sort.as_str() {
        "modified" => SortOrder::Modified,
        "size" => SortOrder::Size,
        _ => SortOrder::Path,
    };

    let filter = FilesFilter {
        language,
        path_glob,
        sort: sort_order,
        limit,
    };

    let mut buf = Vec::new();
    {
        let mut writer = StreamingWriter::new(&mut buf);
        files::run_files(&snapshot, &filter, &color, &mut writer).map_err(|e| e.to_string())?;
    }

    let output = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    Ok((lines, start.elapsed()))
}

/// Handle a single client connection.
///
/// Reads newline-delimited JSON requests from the client and writes
/// newline-delimited JSON responses back.
async fn handle_connection(
    stream: UnixStream,
    manager: &std::sync::Arc<SegmentManager>,
    caught_up: &AtomicBool,
    repo_root: &Path,
    indexrs_dir: &Path,
) -> Result<(), IndexError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await.map_err(IndexError::Io)? > 0 {
        let request: DaemonRequest = match serde_json::from_str(line.trim()) {
            Ok(req) => req,
            Err(e) => {
                let resp = DaemonResponse::Error {
                    message: format!("invalid request: {e}"),
                };
                let json = serde_json::to_string(&resp).unwrap();
                writer
                    .write_all(format!("{json}\n").as_bytes())
                    .await
                    .map_err(IndexError::Io)?;
                line.clear();
                continue;
            }
        };

        match request {
            DaemonRequest::Ping => {
                let resp = serde_json::to_string(&DaemonResponse::Pong).unwrap();
                writer
                    .write_all(format!("{resp}\n").as_bytes())
                    .await
                    .map_err(IndexError::Io)?;
            }
            DaemonRequest::Shutdown => {
                return Ok(());
            }
            DaemonRequest::Search {
                query,
                regex,
                case_sensitive,
                ignore_case,
                limit,
                context_lines,
                language,
                path_glob,
                color,
            } => {
                let pattern = search_cmd::resolve_match_pattern(
                    &query,
                    regex,
                    case_sensitive,
                    ignore_case,
                    false,
                );
                let opts = SearchCmdOptions {
                    pattern,
                    context_lines,
                    limit,
                    language,
                    path_glob,
                    stats: false,
                };
                match handle_search_request(manager, &opts, color) {
                    Ok((lines, elapsed)) => {
                        for line in &lines {
                            let resp = serde_json::to_string(&DaemonResponse::Line {
                                content: line.clone(),
                            })
                            .unwrap();
                            writer
                                .write_all(format!("{resp}\n").as_bytes())
                                .await
                                .map_err(IndexError::Io)?;
                        }
                        let resp = serde_json::to_string(&DaemonResponse::Done {
                            total: lines.len(),
                            duration_ms: elapsed.as_millis() as u64,
                            stale: !caught_up.load(Ordering::Relaxed),
                        })
                        .unwrap();
                        writer
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .map_err(IndexError::Io)?;
                    }
                    Err(msg) => {
                        let resp =
                            serde_json::to_string(&DaemonResponse::Error { message: msg }).unwrap();
                        writer
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .map_err(IndexError::Io)?;
                    }
                }
            }
            DaemonRequest::Files {
                language,
                path_glob,
                sort,
                limit,
                color,
            } => match handle_files_request(manager, language, path_glob, sort, limit, color) {
                Ok((lines, elapsed)) => {
                    for line_content in &lines {
                        let resp = serde_json::to_string(&DaemonResponse::Line {
                            content: line_content.clone(),
                        })
                        .unwrap();
                        writer
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .map_err(IndexError::Io)?;
                    }

                    let resp = serde_json::to_string(&DaemonResponse::Done {
                        total: lines.len(),
                        duration_ms: elapsed.as_millis() as u64,
                        stale: !caught_up.load(Ordering::Relaxed),
                    })
                    .unwrap();
                    writer
                        .write_all(format!("{resp}\n").as_bytes())
                        .await
                        .map_err(IndexError::Io)?;
                }
                Err(msg) => {
                    let resp =
                        serde_json::to_string(&DaemonResponse::Error { message: msg }).unwrap();
                    writer
                        .write_all(format!("{resp}\n").as_bytes())
                        .await
                        .map_err(IndexError::Io)?;
                }
            },
            DaemonRequest::Reindex => {
                let start = Instant::now();
                let repo = repo_root.to_path_buf();
                let idir = indexrs_dir.to_path_buf();
                let mgr = manager.clone();

                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

                let handle = tokio::task::spawn_blocking(move || {
                    indexrs_core::run_catchup_with_progress(&repo, &idir, &mgr, |msg| {
                        let _ = tx.send(msg.to_string());
                    })
                });

                // Stream progress messages to client.
                while let Some(msg) = rx.recv().await {
                    let resp =
                        serde_json::to_string(&DaemonResponse::Progress { message: msg }).unwrap();
                    writer
                        .write_all(format!("{resp}\n").as_bytes())
                        .await
                        .map_err(IndexError::Io)?;
                }

                // Task finished (tx dropped). Get the result.
                match handle.await {
                    Ok(Ok(changes)) => {
                        let elapsed = start.elapsed();
                        let resp = serde_json::to_string(&DaemonResponse::Done {
                            total: changes.len(),
                            duration_ms: elapsed.as_millis() as u64,
                            stale: false,
                        })
                        .unwrap();
                        writer
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .map_err(IndexError::Io)?;
                    }
                    Ok(Err(e)) => {
                        let resp = serde_json::to_string(&DaemonResponse::Error {
                            message: e.to_string(),
                        })
                        .unwrap();
                        writer
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .map_err(IndexError::Io)?;
                    }
                    Err(e) => {
                        let resp = serde_json::to_string(&DaemonResponse::Error {
                            message: format!("reindex task panicked: {e}"),
                        })
                        .unwrap();
                        writer
                            .write_all(format!("{resp}\n").as_bytes())
                            .await
                            .map_err(IndexError::Io)?;
                    }
                }
            }
        }

        line.clear();
    }

    Ok(())
}

/// Spawn a daemon as a detached background process.
fn spawn_daemon_process(repo_root: &Path) -> Result<(), IndexError> {
    let exe = std::env::current_exe().map_err(IndexError::Io)?;
    std::process::Command::new(exe)
        .arg("daemon-start")
        .arg("--repo")
        .arg(repo_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(IndexError::Io)?;
    Ok(())
}

/// Connect to a running daemon, or spawn one and wait for it to be ready.
pub async fn ensure_daemon(repo_root: &Path) -> Result<UnixStream, IndexError> {
    // Fast path: daemon already running.
    if let Some(stream) = try_connect(repo_root).await {
        return Ok(stream);
    }

    // Spawn a new daemon process.
    spawn_daemon_process(repo_root)?;

    // Poll until the socket is ready or timeout.
    let deadline = tokio::time::Instant::now() + DAEMON_STARTUP_TIMEOUT;
    loop {
        tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
        if let Some(stream) = try_connect(repo_root).await {
            return Ok(stream);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(IndexError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "daemon did not start within timeout",
            )));
        }
    }
}

/// Send a request to the daemon and stream results to the writer.
pub async fn run_via_daemon<W: std::io::Write>(
    repo_root: &Path,
    request: DaemonRequest,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
    let stream = ensure_daemon(repo_root).await?;
    let (reader, mut sock_writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send request.
    let json = serde_json::to_string(&request)
        .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    sock_writer
        .write_all(format!("{json}\n").as_bytes())
        .await
        .map_err(IndexError::Io)?;

    // Read responses.
    let mut line = String::new();
    while reader.read_line(&mut line).await.map_err(IndexError::Io)? > 0 {
        let resp: DaemonResponse = serde_json::from_str(line.trim())
            .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;

        match resp {
            DaemonResponse::Line { content } => {
                if writer.write_line(&content).is_err() {
                    break; // SIGPIPE — exit silently
                }
            }
            DaemonResponse::Done { total, stale, .. } => {
                // Clear any progress line on TTY.
                let stderr = std::io::stderr();
                if std::io::IsTerminal::is_terminal(&stderr) {
                    use std::io::Write;
                    let _ = write!(stderr.lock(), "\r{:<80}\r", "");
                }
                let _ = writer.finish();
                if stale {
                    eprintln!("warning: index is updating, results may be incomplete");
                }
                return Ok(if total == 0 {
                    ExitCode::NoResults
                } else {
                    ExitCode::Success
                });
            }
            DaemonResponse::Error { message } => {
                return Err(IndexError::Io(std::io::Error::other(message)));
            }
            DaemonResponse::Progress { message } => {
                let stderr = std::io::stderr();
                let mut handle = stderr.lock();
                if std::io::IsTerminal::is_terminal(&handle) {
                    use std::io::Write;
                    let _ = write!(handle, "\r{message:<80}");
                    let _ = handle.flush();
                } else {
                    use std::io::Write;
                    let _ = writeln!(handle, "{message}");
                }
            }
            DaemonResponse::Pong => {}
        }

        line.clear();
    }

    // Unexpected disconnect.
    Err(IndexError::Io(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "daemon disconnected without sending Done",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialize_search() {
        let req = DaemonRequest::Search {
            query: "hello".to_string(),
            regex: false,
            case_sensitive: false,
            ignore_case: true,
            limit: 1000,
            context_lines: 0,
            language: None,
            path_glob: None,
            color: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("hello"));
    }

    #[test]
    fn test_request_serialize_files() {
        let req = DaemonRequest::Files {
            language: Some("rust".to_string()),
            path_glob: None,
            sort: "path".to_string(),
            limit: None,
            color: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("rust"));
    }

    #[test]
    fn test_request_serialize_search_with_color() {
        let req = DaemonRequest::Search {
            query: "hello".to_string(),
            regex: false,
            case_sensitive: false,
            ignore_case: true,
            limit: 1000,
            context_lines: 0,
            language: None,
            path_glob: None,
            color: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonRequest::Search { color, .. } => assert!(color),
            _ => panic!("expected Search"),
        }
    }

    #[test]
    fn test_request_serialize_files_with_color() {
        let req = DaemonRequest::Files {
            language: None,
            path_glob: None,
            sort: "path".to_string(),
            limit: None,
            color: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonRequest::Files { color, .. } => assert!(color),
            _ => panic!("expected Files"),
        }
    }

    #[test]
    fn test_response_roundtrip() {
        let resp = DaemonResponse::Line {
            content: "src/main.rs:10:5:hello".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, DaemonResponse::Line { .. }));
    }

    #[test]
    fn test_socket_path() {
        let root = PathBuf::from("/tmp/test-repo");
        let path = socket_path(&root);
        assert_eq!(path, PathBuf::from("/tmp/test-repo/.indexrs/sock"));
    }

    #[test]
    fn test_request_roundtrip_ping() {
        let req = DaemonRequest::Ping;
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, DaemonRequest::Ping));
    }

    #[test]
    fn test_request_roundtrip_reindex() {
        let req = DaemonRequest::Reindex;
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, DaemonRequest::Reindex));
    }

    #[test]
    fn test_request_roundtrip_shutdown() {
        let req = DaemonRequest::Shutdown;
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, DaemonRequest::Shutdown));
    }

    #[test]
    fn test_response_roundtrip_done() {
        let resp = DaemonResponse::Done {
            total: 42,
            duration_ms: 123,
            stale: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonResponse::Done {
                total,
                duration_ms,
                stale,
            } => {
                assert_eq!(total, 42);
                assert_eq!(duration_ms, 123);
                assert!(!stale);
            }
            _ => panic!("expected Done variant"),
        }
    }

    #[test]
    fn test_response_roundtrip_done_stale() {
        let resp = DaemonResponse::Done {
            total: 10,
            duration_ms: 50,
            stale: true,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonResponse::Done { stale, .. } => assert!(stale),
            _ => panic!("expected Done variant"),
        }
    }

    #[test]
    fn test_response_roundtrip_error() {
        let resp = DaemonResponse::Error {
            message: "something went wrong".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonResponse::Error { message } => {
                assert_eq!(message, "something went wrong");
            }
            _ => panic!("expected Error variant"),
        }
    }

    #[test]
    fn test_response_roundtrip_pong() {
        let resp = DaemonResponse::Pong;
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, DaemonResponse::Pong));
    }

    #[test]
    fn test_response_roundtrip_progress() {
        let resp = DaemonResponse::Progress {
            message: "Detecting changes...".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, DaemonResponse::Progress { message } if message == "Detecting changes...")
        );
    }

    #[tokio::test]
    async fn test_try_connect_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let result = try_connect(dir.path()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_daemon_ping_pong() {
        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

        let repo_root = dir.path().to_path_buf();
        let repo_root_clone = repo_root.clone();

        // Start daemon in background.
        let daemon_handle = tokio::spawn(async move {
            start_daemon(&repo_root_clone).await.unwrap();
        });

        // Give the daemon time to bind the socket.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Connect and send a Ping.
        let stream = try_connect(&repo_root).await.expect("should connect");
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let req = serde_json::to_string(&DaemonRequest::Ping).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();
        let resp: DaemonResponse = serde_json::from_str(response_line.trim()).unwrap();
        assert!(matches!(resp, DaemonResponse::Pong));

        // Send Shutdown to clean up.
        let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        // Wait for daemon to exit.
        let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
    }

    #[tokio::test]
    async fn test_daemon_files_returns_results() {
        use indexrs_core::segment::InputFile;

        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

        // Write source files to disk so background catch-up doesn't tombstone them.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), b"fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), b"pub fn hello() {}\n").unwrap();

        let manager = indexrs_core::SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {}\n".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "src/lib.rs".to_string(),
                    content: b"pub fn hello() {}\n".to_vec(),
                    mtime: 200,
                },
            ])
            .unwrap();
        drop(manager);

        let repo_root = dir.path().to_path_buf();
        let repo_root_clone = repo_root.clone();

        let daemon_handle = tokio::spawn(async move {
            start_daemon(&repo_root_clone).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let stream = try_connect(&repo_root).await.expect("should connect");
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Send a Files request.
        let req = serde_json::to_string(&DaemonRequest::Files {
            language: None,
            path_glob: None,
            sort: "path".to_string(),
            limit: None,
            color: false,
        })
        .unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        let mut lines = Vec::new();
        loop {
            let mut response_line = String::new();
            reader.read_line(&mut response_line).await.unwrap();
            let resp: DaemonResponse = serde_json::from_str(response_line.trim()).unwrap();
            match resp {
                DaemonResponse::Line { content } => {
                    lines.push(content);
                }
                DaemonResponse::Done { total, .. } => {
                    assert_eq!(total, lines.len());
                    break;
                }
                other => panic!("unexpected response: {other:?}"),
            }
        }

        assert_eq!(lines.len(), 2, "should list both indexed files");
        assert!(lines.iter().any(|l| l.contains("main.rs")));
        assert!(lines.iter().any(|l| l.contains("lib.rs")));

        // Shutdown.
        let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
    }

    #[tokio::test]
    async fn test_daemon_search_invalid_regex_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

        // Create a minimal valid index (empty is fine for error testing).
        let manager = indexrs_core::SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![indexrs_core::segment::InputFile {
                path: "test.rs".to_string(),
                content: b"fn test() {}\n".to_vec(),
                mtime: 100,
            }])
            .unwrap();
        drop(manager);

        let repo_root = dir.path().to_path_buf();
        let repo_root_clone = repo_root.clone();

        let daemon_handle = tokio::spawn(async move {
            start_daemon(&repo_root_clone).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let stream = try_connect(&repo_root).await.expect("should connect");
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Send a Search request with an invalid regex.
        let req = serde_json::to_string(&DaemonRequest::Search {
            query: "[invalid(".to_string(),
            regex: true,
            case_sensitive: false,
            ignore_case: false,
            limit: 100,
            context_lines: 0,
            language: None,
            path_glob: None,
            color: false,
        })
        .unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();
        let resp: DaemonResponse = serde_json::from_str(response_line.trim()).unwrap();
        assert!(
            matches!(resp, DaemonResponse::Error { .. }),
            "invalid regex should return Error, got {resp:?}"
        );

        // Shutdown.
        let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
    }

    #[tokio::test]
    async fn test_daemon_search_returns_results() {
        use indexrs_core::segment::InputFile;

        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

        // Write source files to disk so background catch-up doesn't tombstone them.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            b"fn main() {\n    println!(\"hello world\");\n}\n",
        )
        .unwrap();

        // Build an index with searchable content.
        let manager = indexrs_core::SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![InputFile {
                path: "src/main.rs".to_string(),
                content: b"fn main() {\n    println!(\"hello world\");\n}\n".to_vec(),
                mtime: 100,
            }])
            .unwrap();
        drop(manager);

        let repo_root = dir.path().to_path_buf();
        let repo_root_clone = repo_root.clone();

        let daemon_handle = tokio::spawn(async move {
            start_daemon(&repo_root_clone).await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        let stream = try_connect(&repo_root).await.expect("should connect");
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Send a Search request for "println".
        let req = serde_json::to_string(&DaemonRequest::Search {
            query: "println".to_string(),
            regex: false,
            case_sensitive: false,
            ignore_case: true,
            limit: 100,
            context_lines: 0,
            language: None,
            path_glob: None,
            color: false,
        })
        .unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        // Read responses: expect at least one Line, then Done.
        let mut lines = Vec::new();
        loop {
            let mut response_line = String::new();
            reader.read_line(&mut response_line).await.unwrap();
            let resp: DaemonResponse = serde_json::from_str(response_line.trim()).unwrap();
            match resp {
                DaemonResponse::Line { content } => {
                    lines.push(content);
                }
                DaemonResponse::Done { total, .. } => {
                    assert_eq!(total, lines.len());
                    break;
                }
                other => panic!("unexpected response: {other:?}"),
            }
        }

        assert!(!lines.is_empty(), "should have at least one result line");
        assert!(
            lines.iter().any(|l| l.contains("println")),
            "result should contain 'println'"
        );

        // Shutdown.
        let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
    }

    #[tokio::test]
    async fn test_run_via_daemon_search() {
        use indexrs_core::segment::InputFile;

        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

        // Write source files to disk so background catch-up doesn't tombstone them.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            b"fn main() {\n    println!(\"hello world\");\n}\n",
        )
        .unwrap();

        let manager = indexrs_core::SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![InputFile {
                path: "src/main.rs".to_string(),
                content: b"fn main() {\n    println!(\"hello world\");\n}\n".to_vec(),
                mtime: 100,
            }])
            .unwrap();
        drop(manager);

        let repo_root = dir.path().to_path_buf();
        let repo_root_clone = repo_root.clone();

        // Start daemon in-process.
        let daemon_handle = tokio::spawn(async move {
            start_daemon(&repo_root_clone).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Use run_via_daemon to send a search.
        let request = DaemonRequest::Search {
            query: "println".to_string(),
            regex: false,
            case_sensitive: false,
            ignore_case: true,
            limit: 100,
            context_lines: 0,
            language: None,
            path_glob: None,
            color: false,
        };

        let mut buf = Vec::new();
        let exit = {
            let mut writer = crate::output::StreamingWriter::new(&mut buf);
            run_via_daemon(&repo_root, request, &mut writer)
                .await
                .unwrap()
        };
        let output = String::from_utf8(buf).unwrap();

        assert!(matches!(exit, crate::output::ExitCode::Success));
        assert!(
            output.contains("println"),
            "output should contain search results"
        );

        // Shutdown daemon.
        let stream = try_connect(&repo_root).await.unwrap();
        let (_, mut writer) = stream.into_split();
        let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
    }

    #[tokio::test]
    async fn test_daemon_search_with_color() {
        use indexrs_core::segment::InputFile;

        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

        // Write source files to disk so background catch-up doesn't tombstone them.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            b"fn main() {\n    println!(\"hello world\");\n}\n",
        )
        .unwrap();

        let manager = indexrs_core::SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![InputFile {
                path: "src/main.rs".to_string(),
                content: b"fn main() {\n    println!(\"hello world\");\n}\n".to_vec(),
                mtime: 100,
            }])
            .unwrap();
        drop(manager);

        let repo_root = dir.path().to_path_buf();
        let repo_root_clone = repo_root.clone();

        let daemon_handle = tokio::spawn(async move {
            start_daemon(&repo_root_clone).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stream = try_connect(&repo_root).await.expect("should connect");
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let req = serde_json::to_string(&DaemonRequest::Search {
            query: "println".to_string(),
            regex: false,
            case_sensitive: false,
            ignore_case: true,
            limit: 100,
            context_lines: 0,
            language: None,
            path_glob: None,
            color: true,
        })
        .unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        let mut lines = Vec::new();
        loop {
            let mut response_line = String::new();
            reader.read_line(&mut response_line).await.unwrap();
            let resp: DaemonResponse = serde_json::from_str(response_line.trim()).unwrap();
            match resp {
                DaemonResponse::Line { content } => lines.push(content),
                DaemonResponse::Done { .. } => break,
                other => panic!("unexpected response: {other:?}"),
            }
        }

        assert!(!lines.is_empty());
        assert!(
            lines.iter().any(|l| l.contains("\x1b[")),
            "expected ANSI color codes in output, got: {:?}",
            lines
        );

        let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
    }

    #[tokio::test]
    async fn test_ensure_daemon_connects_to_running() {
        use indexrs_core::segment::InputFile;

        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

        let manager = indexrs_core::SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![InputFile {
                path: "test.rs".to_string(),
                content: b"fn test() {}\n".to_vec(),
                mtime: 100,
            }])
            .unwrap();
        drop(manager);

        let repo_root = dir.path().to_path_buf();
        let repo_root_clone = repo_root.clone();

        // Start daemon in-process first.
        let daemon_handle = tokio::spawn(async move {
            start_daemon(&repo_root_clone).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // ensure_daemon should find the running daemon.
        let stream = ensure_daemon(&repo_root).await.expect("should connect");

        // Verify with Ping/Pong.
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let req = serde_json::to_string(&DaemonRequest::Ping).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();
        let resp: DaemonResponse = serde_json::from_str(response_line.trim()).unwrap();
        assert!(matches!(resp, DaemonResponse::Pong));

        // Shutdown.
        let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
    }
}
