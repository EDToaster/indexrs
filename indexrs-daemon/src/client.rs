use std::path::{Path, PathBuf};
use std::time::Duration;

use indexrs_core::error::IndexError;
use tokio::net::UnixStream;

/// Maximum time to wait for a spawned daemon to become ready.
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between connection attempts when waiting for daemon startup.
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Return the Unix socket path for a given repo root.
pub fn socket_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".indexrs").join("sock")
}

/// Try to connect to a running daemon. Returns None if no daemon is running.
pub async fn try_connect(repo_root: &Path) -> Option<UnixStream> {
    let path = socket_path(repo_root);
    UnixStream::connect(&path).await.ok()
}

/// Spawn a daemon as a detached background process.
///
/// `daemon_bin` is the path to the binary that accepts `daemon-start --repo <path>`.
pub fn spawn_daemon_process(daemon_bin: &Path, repo_root: &Path) -> Result<(), IndexError> {
    std::process::Command::new(daemon_bin)
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
///
/// `daemon_bin` is the path to the binary that accepts `daemon-start --repo <path>`.
pub async fn ensure_daemon(daemon_bin: &Path, repo_root: &Path) -> Result<UnixStream, IndexError> {
    // Fast path: daemon already running.
    if let Some(stream) = try_connect(repo_root).await {
        return Ok(stream);
    }

    // Spawn a new daemon process.
    spawn_daemon_process(daemon_bin, repo_root)?;

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
