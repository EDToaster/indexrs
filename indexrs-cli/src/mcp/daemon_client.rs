//! Daemon client for dispatching MCP tool requests through the indexrs daemon.
//!
//! [`DaemonClient`] lazily connects to (or spawns) the daemon process and
//! sends requests over the Unix socket using JSON lines + TLV binary framing.
//! It retries once on connection failure (clears the cached connection and
//! reconnects).

use std::path::PathBuf;

use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use indexrs_daemon::ensure_daemon;
use indexrs_daemon::types::{DaemonRequest, DaemonResponse};
use indexrs_daemon::wire::read_response;

/// Result of a successful daemon request.
#[derive(Debug)]
pub struct DaemonResult {
    /// All `Line` contents joined with `"\n"`.
    pub text: String,
    /// Total result count from the `Done` frame.
    pub total: usize,
    /// Whether the index was stale at query time.
    pub stale: bool,
    /// Duration in milliseconds from the `Done` frame.
    pub duration_ms: u64,
}

/// Client that dispatches requests to the indexrs daemon via Unix socket.
///
/// Lazily connects on first use and retries once on connection failure.
pub struct DaemonClient {
    repo_root: PathBuf,
    conn: Mutex<Option<UnixStream>>,
}

impl DaemonClient {
    /// Create a new daemon client for the given repository root.
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            conn: Mutex::new(None),
        }
    }

    /// Send a request to the daemon and collect the response.
    ///
    /// On the first call this lazily connects to (or spawns) the daemon.
    /// If the request fails due to a connection error, the cached connection
    /// is cleared and the request is retried once.
    pub async fn request(&self, req: DaemonRequest) -> Result<DaemonResult, String> {
        match self.request_inner(&req).await {
            Ok(result) => Ok(result),
            Err(first_err) => {
                tracing::warn!("daemon request failed, retrying: {first_err}");
                // Clear stale connection and retry once.
                {
                    let mut guard = self.conn.lock().await;
                    *guard = None;
                }
                self.request_inner(&req)
                    .await
                    .map_err(|e| format!("daemon request failed after retry: {e}"))
            }
        }
    }

    /// Inner implementation: connect if needed, send request, read response frames.
    async fn request_inner(&self, req: &DaemonRequest) -> Result<DaemonResult, String> {
        let mut guard = self.conn.lock().await;

        // Lazily establish connection.
        if guard.is_none() {
            let daemon_bin = std::env::current_exe()
                .map_err(|e| format!("cannot determine current executable: {e}"))?;
            let stream = ensure_daemon(&daemon_bin, &self.repo_root)
                .await
                .map_err(|e| format!("failed to connect to daemon: {e}"))?;
            *guard = Some(stream);
        }

        let stream = guard.as_mut().unwrap();

        // Send request as a JSON line.
        let mut json =
            serde_json::to_string(req).map_err(|e| format!("failed to serialize request: {e}"))?;
        json.push('\n');

        let mut writer = BufWriter::new(&mut *stream);
        writer
            .write_all(json.as_bytes())
            .await
            .map_err(|e| format!("failed to send request: {e}"))?;
        writer
            .flush()
            .await
            .map_err(|e| format!("failed to flush request: {e}"))?;

        // Read TLV response frames until Done or Error.
        let mut lines: Vec<String> = Vec::new();

        loop {
            let frame = read_response(&mut *stream)
                .await
                .map_err(|e| format!("failed to read response: {e}"))?;

            match frame {
                DaemonResponse::Line { content } => {
                    lines.push(content);
                }
                DaemonResponse::Progress { message } => {
                    tracing::info!("daemon: {message}");
                }
                DaemonResponse::Done {
                    total,
                    duration_ms,
                    stale,
                } => {
                    return Ok(DaemonResult {
                        text: lines.join("\n"),
                        total,
                        stale,
                        duration_ms,
                    });
                }
                DaemonResponse::Error { message } => {
                    return Err(message);
                }
                DaemonResponse::Pong => {
                    return Ok(DaemonResult {
                        text: "pong".to_string(),
                        total: 0,
                        stale: false,
                        duration_ms: 0,
                    });
                }
            }
        }
    }
}
