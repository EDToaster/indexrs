use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

use indexrs_core::HybridDetector;
use indexrs_core::SegmentManager;
use indexrs_core::checkpoint::{Checkpoint, write_checkpoint};
use indexrs_core::error::IndexError;
use indexrs_core::git_diff::GitChangeDetector;
use indexrs_core::search::MatchPattern;

pub use indexrs_daemon::{DaemonRequest, DaemonResponse};

use crate::args::SortOrder;
use crate::color::ColorConfig;
use crate::files::{self, FilesFilter};
use crate::output::{ExitCode, StreamingWriter};
use crate::paths::PathRewriter;
use crate::search_cmd;
use crate::wire;

/// Idle timeout before daemon self-terminates.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

/// Return the Unix socket path for a given repo root.
pub fn socket_path(repo_root: &Path) -> PathBuf {
    indexrs_daemon::socket_path(repo_root)
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

/// Format a FileMatch into vimgrep-style output lines and send each as a
/// TLV binary frame through the channel.
fn format_and_send_file_match(
    file_match: &indexrs_core::search::FileMatch,
    color: &ColorConfig,
    path_rewriter: &PathRewriter,
    glob_matcher: &Option<globset::GlobMatcher>,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) -> bool {
    let raw_path = file_match.path.to_string_lossy();

    // Path filter
    if let Some(matcher) = glob_matcher
        && !matcher.is_match(raw_path.as_ref())
    {
        return true; // filtered out, keep going
    }

    let path_str = path_rewriter.rewrite(&raw_path);

    for line_match in &file_match.lines {
        let col = line_match
            .ranges
            .first()
            .map(|(start, _)| start + 1)
            .unwrap_or(1);

        let line = color.format_search_line(
            &path_str,
            line_match.line_number,
            col,
            &line_match.content,
            &line_match.ranges,
        );

        if tx.send(crate::wire::encode_line_frame(&line)).is_err() {
            return false; // receiver dropped, stop
        }
    }

    true // keep going
}

/// Execute a query-language search against the loaded index.
#[allow(dead_code)]
fn handle_query_search_request(
    manager: &SegmentManager,
    query_str: &str,
    limit: usize,
    context_lines: usize,
    color: bool,
    path_rewriter: &PathRewriter,
) -> Result<(Vec<String>, Duration), String> {
    let start = Instant::now();
    let snapshot = manager.snapshot();
    let color = ColorConfig::new(color);

    let mut buf = Vec::new();
    {
        let mut writer = StreamingWriter::new(&mut buf);
        search_cmd::run_query_search(
            &snapshot,
            query_str,
            context_lines,
            limit,
            &color,
            path_rewriter,
            &mut writer,
        )
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
    path_rewriter: &PathRewriter,
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
        files::run_files(&snapshot, &filter, &color, path_rewriter, &mut writer)
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

/// Handle a single client connection.
///
/// Reads newline-delimited JSON requests from the client and writes
/// TLV binary-framed responses back.
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
                wire::write_response(
                    &mut writer,
                    &DaemonResponse::Error {
                        message: format!("invalid request: {e}"),
                    },
                )
                .await
                .map_err(IndexError::Io)?;
                line.clear();
                continue;
            }
        };

        match request {
            DaemonRequest::Ping => {
                wire::write_response(&mut writer, &DaemonResponse::Pong)
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
                cwd,
            } => {
                let stale = !caught_up.load(Ordering::Relaxed);
                let path_rewriter = match cwd {
                    Some(ref cwd_str) => PathRewriter::new(repo_root, Path::new(cwd_str)),
                    None => PathRewriter::identity(),
                };
                let pattern = search_cmd::resolve_match_pattern(
                    &query,
                    regex,
                    case_sensitive,
                    ignore_case,
                    false,
                );

                // Validate regex before starting the search.
                if let MatchPattern::Regex(ref pat) = pattern
                    && let Err(e) = regex::Regex::new(pat)
                {
                    wire::write_response(
                        &mut writer,
                        &DaemonResponse::Error {
                            message: format!("invalid regex: {e}"),
                        },
                    )
                    .await
                    .map_err(IndexError::Io)?;
                    line.clear();
                    continue;
                }

                let query = match search_cmd::flags_to_query(&pattern, language.as_deref()) {
                    Ok(q) => q,
                    Err(e) => {
                        wire::write_response(
                            &mut writer,
                            &DaemonResponse::Error {
                                message: e.to_string(),
                            },
                        )
                        .await
                        .map_err(IndexError::Io)?;
                        line.clear();
                        continue;
                    }
                };

                let color_config = ColorConfig::new(color);
                let start = Instant::now();
                let snapshot = manager.snapshot();
                let search_opts = indexrs_core::search::SearchOptions {
                    context_lines,
                    max_results: Some(limit),
                };

                let glob_matcher: Option<globset::GlobMatcher> = path_glob
                    .as_ref()
                    .and_then(|g| globset::Glob::new(g).ok().map(|g| g.compile_matcher()));

                let (search_tx, search_rx) = std::sync::mpsc::channel();

                // Spawn blocking search thread using query-based streaming.
                let search_handle = tokio::task::spawn_blocking(move || {
                    indexrs_core::multi_search::search_segments_with_query_streaming(
                        &snapshot,
                        &query,
                        &search_opts,
                        search_tx,
                    )
                });

                // Bridge: blocking mpsc -> tokio mpsc -> async socket writer.
                let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

                let bridge_handle = tokio::task::spawn_blocking({
                    let async_tx = async_tx.clone();
                    move || {
                        for file_match in search_rx {
                            if !format_and_send_file_match(
                                &file_match,
                                &color_config,
                                &path_rewriter,
                                &glob_matcher,
                                &async_tx,
                            ) {
                                break; // receiver dropped
                            }
                        }
                    }
                });

                // Drop our copy of async_tx so the channel closes when bridge finishes.
                drop(async_tx);

                // Stream responses to the client as they arrive.
                let mut total: usize = 0;
                while let Some(frame) = async_rx.recv().await {
                    total += 1;
                    if writer.write_all(&frame).await.is_err() {
                        break; // client disconnected
                    }
                }

                // Wait for both tasks to finish.
                let search_result = search_handle.await;
                let _ = bridge_handle.await;

                // Check for search errors (match the Reindex pattern).
                match search_result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        wire::write_response(
                            &mut writer,
                            &DaemonResponse::Error {
                                message: e.to_string(),
                            },
                        )
                        .await
                        .map_err(IndexError::Io)?;
                        line.clear();
                        continue;
                    }
                    Err(e) => {
                        wire::write_response(
                            &mut writer,
                            &DaemonResponse::Error {
                                message: format!("search task panicked: {e}"),
                            },
                        )
                        .await
                        .map_err(IndexError::Io)?;
                        line.clear();
                        continue;
                    }
                }

                let elapsed = start.elapsed();
                wire::write_response(
                    &mut writer,
                    &DaemonResponse::Done {
                        total,
                        duration_ms: elapsed.as_millis() as u64,
                        stale,
                    },
                )
                .await
                .map_err(IndexError::Io)?;
            }
            DaemonRequest::QuerySearch {
                query,
                limit,
                context_lines,
                color,
                cwd,
            } => {
                let stale = !caught_up.load(Ordering::Relaxed);
                let path_rewriter = match cwd {
                    Some(ref cwd_str) => PathRewriter::new(repo_root, Path::new(cwd_str)),
                    None => PathRewriter::identity(),
                };

                let parsed_query = match indexrs_core::query::parse_query(&query) {
                    Ok(q) => q,
                    Err(e) => {
                        wire::write_response(
                            &mut writer,
                            &DaemonResponse::Error {
                                message: e.to_string(),
                            },
                        )
                        .await
                        .map_err(IndexError::Io)?;
                        line.clear();
                        continue;
                    }
                };

                let color_config = ColorConfig::new(color);
                let start = Instant::now();
                let snapshot = manager.snapshot();
                let search_opts = indexrs_core::search::SearchOptions {
                    context_lines,
                    max_results: Some(limit),
                };

                let (search_tx, search_rx) = std::sync::mpsc::channel();
                let search_handle = tokio::task::spawn_blocking(move || {
                    indexrs_core::multi_search::search_segments_with_query_streaming(
                        &snapshot,
                        &parsed_query,
                        &search_opts,
                        search_tx,
                    )
                });

                // Bridge: blocking mpsc -> tokio mpsc -> async socket writer
                let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

                let bridge_handle = tokio::task::spawn_blocking({
                    let async_tx = async_tx.clone();
                    move || {
                        for file_match in search_rx {
                            if !format_and_send_file_match(
                                &file_match,
                                &color_config,
                                &path_rewriter,
                                &None,
                                &async_tx,
                            ) {
                                break;
                            }
                        }
                    }
                });

                drop(async_tx);

                let mut total: usize = 0;
                while let Some(frame) = async_rx.recv().await {
                    total += 1;
                    if writer.write_all(&frame).await.is_err() {
                        break;
                    }
                }

                let _ = search_handle.await;
                let _ = bridge_handle.await;

                let elapsed = start.elapsed();
                wire::write_response(
                    &mut writer,
                    &DaemonResponse::Done {
                        total,
                        duration_ms: elapsed.as_millis() as u64,
                        stale,
                    },
                )
                .await
                .map_err(IndexError::Io)?;
            }
            DaemonRequest::Files {
                language,
                path_glob,
                sort,
                limit,
                color,
                cwd,
            } => {
                let stale = !caught_up.load(Ordering::Relaxed);
                let path_rewriter = match cwd {
                    Some(ref cwd_str) => PathRewriter::new(repo_root, Path::new(cwd_str)),
                    None => PathRewriter::identity(),
                };
                match handle_files_request(
                    manager,
                    language,
                    path_glob,
                    sort,
                    limit,
                    color,
                    &path_rewriter,
                ) {
                    Ok((lines, elapsed)) => {
                        for line_content in &lines {
                            wire::write_response(
                                &mut writer,
                                &DaemonResponse::Line {
                                    content: line_content.clone(),
                                },
                            )
                            .await
                            .map_err(IndexError::Io)?;
                        }

                        wire::write_response(
                            &mut writer,
                            &DaemonResponse::Done {
                                total: lines.len(),
                                duration_ms: elapsed.as_millis() as u64,
                                stale,
                            },
                        )
                        .await
                        .map_err(IndexError::Io)?;
                    }
                    Err(msg) => {
                        wire::write_response(&mut writer, &DaemonResponse::Error { message: msg })
                            .await
                            .map_err(IndexError::Io)?;
                    }
                }
            }
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
                    wire::write_response(&mut writer, &DaemonResponse::Progress { message: msg })
                        .await
                        .map_err(IndexError::Io)?;
                }

                // Task finished (tx dropped). Get the result.
                match handle.await {
                    Ok(Ok(changes)) => {
                        let elapsed = start.elapsed();
                        wire::write_response(
                            &mut writer,
                            &DaemonResponse::Done {
                                total: changes.len(),
                                duration_ms: elapsed.as_millis() as u64,
                                stale: false,
                            },
                        )
                        .await
                        .map_err(IndexError::Io)?;
                    }
                    Ok(Err(e)) => {
                        wire::write_response(
                            &mut writer,
                            &DaemonResponse::Error {
                                message: e.to_string(),
                            },
                        )
                        .await
                        .map_err(IndexError::Io)?;
                    }
                    Err(e) => {
                        wire::write_response(
                            &mut writer,
                            &DaemonResponse::Error {
                                message: format!("reindex task panicked: {e}"),
                            },
                        )
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

/// Connect to a running daemon, or spawn one and wait for it to be ready.
pub async fn ensure_daemon(repo_root: &Path) -> Result<UnixStream, IndexError> {
    let exe = std::env::current_exe().map_err(IndexError::Io)?;
    indexrs_daemon::client::ensure_daemon(&exe, repo_root).await
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

    // Read responses (TLV binary frames).
    loop {
        let resp = match wire::read_response(&mut reader).await {
            Ok(resp) => resp,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(IndexError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "daemon disconnected without sending Done",
                )));
            }
            Err(e) => return Err(IndexError::Io(e)),
        };

        match resp {
            DaemonResponse::Line { content } => {
                if writer.write_line(&content).is_err() {
                    // SIGPIPE — exit silently with whatever we have.
                    let _ = writer.finish();
                    return Ok(ExitCode::Success);
                }
            }
            DaemonResponse::Done { total, stale, .. } => {
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
                eprintln!("{message}");
            }
            DaemonResponse::Pong => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexrs_daemon::try_connect;

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
            cwd: None,
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
            cwd: None,
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
            cwd: None,
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
            cwd: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonRequest::Files { color, .. } => assert!(color),
            _ => panic!("expected Files"),
        }
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

        let resp = crate::wire::read_response(&mut reader).await.unwrap();
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
            cwd: None,
        })
        .unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        let mut lines = Vec::new();
        loop {
            let resp = crate::wire::read_response(&mut reader).await.unwrap();
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
            cwd: None,
        })
        .unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        let resp = crate::wire::read_response(&mut reader).await.unwrap();
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
            cwd: None,
        })
        .unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        // Read responses: expect at least one Line, then Done.
        let mut lines = Vec::new();
        loop {
            let resp = crate::wire::read_response(&mut reader).await.unwrap();
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
            cwd: None,
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
            cwd: None,
        })
        .unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        let mut lines = Vec::new();
        loop {
            let resp = crate::wire::read_response(&mut reader).await.unwrap();
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

    #[test]
    fn test_query_search_request_serialization() {
        let req = DaemonRequest::QuerySearch {
            query: "language:rust println OR eprintln".to_string(),
            limit: 100,
            context_lines: 0,
            color: false,
            cwd: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonRequest::QuerySearch { query, limit, .. } => {
                assert_eq!(query, "language:rust println OR eprintln");
                assert_eq!(limit, 100);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_handle_query_search_request() {
        use indexrs_core::segment::InputFile;

        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

        let manager = indexrs_core::SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "src/lib.rs".to_string(),
                    content: b"pub fn greet() -> &'static str {\n    \"hello\"\n}\n".to_vec(),
                    mtime: 200,
                },
                InputFile {
                    path: "app.py".to_string(),
                    content: b"def main():\n    print(\"hello\")\n    println = 1\n".to_vec(),
                    mtime: 300,
                },
            ])
            .unwrap();

        let rewriter = crate::paths::PathRewriter::identity();

        // 1. Simple literal: "println" should match main.rs (contains println! macro)
        let (lines, _dur) =
            handle_query_search_request(&manager, "println", 100, 0, false, &rewriter).unwrap();
        assert!(
            lines.iter().any(|l| l.contains("main.rs")),
            "simple literal 'println' should match main.rs, got: {lines:?}"
        );

        // 2. Language filter: "language:rust main" should only match .rs files
        let (lines, _dur) =
            handle_query_search_request(&manager, "language:rust main", 100, 0, false, &rewriter)
                .unwrap();
        assert!(
            !lines.is_empty(),
            "language:rust main should produce results"
        );
        assert!(
            lines.iter().all(|l| !l.contains("app.py")),
            "language:rust should exclude app.py, got: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains(".rs")),
            "language:rust main should match a .rs file, got: {lines:?}"
        );

        // 3. Implicit AND: "println main" should only match files containing BOTH terms
        let (lines, _dur) =
            handle_query_search_request(&manager, "println main", 100, 0, false, &rewriter)
                .unwrap();
        // main.rs has both "println" and "main", lib.rs has neither "println" nor "main"
        // app.py has "main" and "println" (as variable), so it can match too
        for line in &lines {
            // Every matched file must contain both terms — we just check we got results
            // and that lib.rs (which has neither println nor main) does NOT appear
            assert!(
                !line.contains("lib.rs"),
                "implicit AND 'println main' should not match lib.rs (no println), got: {lines:?}"
            );
        }
        assert!(
            !lines.is_empty(),
            "implicit AND 'println main' should have results"
        );

        // 4. OR: "println OR greet" should match both main.rs and lib.rs
        let (lines, _dur) =
            handle_query_search_request(&manager, "println OR greet", 100, 0, false, &rewriter)
                .unwrap();
        assert!(
            lines.iter().any(|l| l.contains("main.rs")),
            "OR query should match main.rs (has println), got: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("lib.rs")),
            "OR query should match lib.rs (has greet), got: {lines:?}"
        );

        // 5. NOT: "main NOT println" — main.rs has both so excluded; app.py has "main"
        //    and "println" (as variable name), so it depends on verification.
        //    lib.rs has neither "main" nor "println" so it won't match.
        let (lines, _dur) =
            handle_query_search_request(&manager, "main NOT println", 100, 0, false, &rewriter)
                .unwrap();
        assert!(
            lines.iter().all(|l| !l.contains("main.rs")),
            "NOT query should exclude main.rs (has both main and println), got: {lines:?}"
        );
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

        let resp = crate::wire::read_response(&mut reader).await.unwrap();
        assert!(matches!(resp, DaemonResponse::Pong));

        // Shutdown.
        let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
    }

    #[tokio::test]
    async fn test_daemon_search_streams_results() {
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
        std::fs::write(
            dir.path().join("src/lib.rs"),
            b"pub fn greet() {\n    println!(\"hi there\");\n}\n",
        )
        .unwrap();

        let manager = indexrs_core::SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {\n    println!(\"hello world\");\n}\n".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "src/lib.rs".to_string(),
                    content: b"pub fn greet() {\n    println!(\"hi there\");\n}\n".to_vec(),
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
            cwd: None,
        })
        .unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();

        let mut lines = Vec::new();
        let mut got_done = false;
        loop {
            let resp = crate::wire::read_response(&mut reader).await.unwrap();
            match resp {
                DaemonResponse::Line { content } => {
                    assert!(!got_done, "should not receive Line after Done");
                    lines.push(content);
                }
                DaemonResponse::Done { total, .. } => {
                    assert_eq!(total, lines.len());
                    got_done = true;
                    break;
                }
                other => panic!("unexpected response: {other:?}"),
            }
        }

        assert!(got_done, "should receive Done");
        assert!(
            lines.len() >= 2,
            "should have results from both files, got {}",
            lines.len()
        );
        assert!(
            lines.iter().any(|l| l.contains("println")),
            "results should contain 'println'"
        );

        // Shutdown.
        let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
    }
}
