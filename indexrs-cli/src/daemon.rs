use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

use indexrs_core::SegmentManager;
use indexrs_core::error::IndexError;

/// Idle timeout before daemon self-terminates.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

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
    },
    Files {
        language: Option<String>,
        path_glob: Option<String>,
        sort: String,
        limit: Option<usize>,
    },
    Ping,
    Shutdown,
}

/// Response from daemon to CLI client, one JSON line per message.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonResponse {
    /// A single output line (file path or search match).
    Line { content: String },
    /// End of results with summary.
    Done { total: usize, duration_ms: u64 },
    /// Error message.
    Error { message: String },
    /// Ping response.
    Pong,
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

    loop {
        match timeout(IDLE_TIMEOUT, listener.accept()).await {
            Ok(Ok((stream, _))) => {
                let mgr = manager.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &mgr).await {
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

/// Handle a single client connection.
///
/// Reads newline-delimited JSON requests from the client and writes
/// newline-delimited JSON responses back.
async fn handle_connection(stream: UnixStream, manager: &SegmentManager) -> Result<(), IndexError> {
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
            DaemonRequest::Search { .. } | DaemonRequest::Files { .. } => {
                // Execute the command using the pre-loaded index.
                // Serialize results as Line responses, then Done.
                // Implementation delegates to run_search/run_files with
                // a Vec<u8> writer, then sends each line as a DaemonResponse::Line.
                let _snapshot = manager.snapshot();
                // TODO: full implementation — for now, return Done with zero results.
                let resp = serde_json::to_string(&DaemonResponse::Done {
                    total: 0,
                    duration_ms: 0,
                })
                .unwrap();
                writer
                    .write_all(format!("{resp}\n").as_bytes())
                    .await
                    .map_err(IndexError::Io)?;
            }
        }

        line.clear();
    }

    Ok(())
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
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("rust"));
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
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonResponse::Done { total, duration_ms } => {
                assert_eq!(total, 42);
                assert_eq!(duration_ms, 123);
            }
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
}
