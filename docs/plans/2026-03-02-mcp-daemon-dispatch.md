# MCP Daemon Dispatch Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Route MCP `search_code`, `search_files`, and `reindex` through the existing CLI daemon for warm-index TTFB, while keeping `get_file` and `index_status` client-side.

**Architecture:** Extract the daemon client protocol (types, wire format, connection helpers) into a new `ferret-indexer-daemon` crate shared by both CLI and MCP. The MCP server connects to the daemon on startup via `ensure_daemon()`, sends requests over the Unix socket, collects `Line` response frames, and returns the plain-text output as MCP tool responses. The daemon's `color: false` plain-text formatting replaces the MCP's own search formatter for daemon-routed tools. `get_file` and `index_status` continue to read directly from the on-disk index via `IndexState`.

**Tech Stack:** Rust, tokio, serde/serde_json, Unix domain sockets (tokio::net::UnixStream)

---

### Task 1: Create `ferret-indexer-daemon` crate with shared types

**Files:**
- Create: `ferret-indexer-daemon/Cargo.toml`
- Create: `ferret-indexer-daemon/src/lib.rs`
- Create: `ferret-indexer-daemon/src/types.rs`
- Modify: `Cargo.toml` (workspace members)

**Step 1: Create the crate directory**

```bash
mkdir -p ferret-indexer-daemon/src
```

**Step 2: Write `ferret-indexer-daemon/Cargo.toml`**

```toml
[package]
name = "ferret-indexer-daemon"
version = "0.1.0"
edition = "2024"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["net", "io-util", "time", "process"] }
ferret-indexer-core = { path = "../ferret-indexer-core" }
```

**Step 3: Write `ferret-indexer-daemon/src/types.rs`**

Move `DaemonRequest` and `DaemonResponse` from `ferret-indexer-cli/src/daemon.rs:37-87`. Keep the same definitions exactly. `DaemonRequest` already derives `Serialize, Deserialize`. `DaemonResponse` currently only derives `Debug, PartialEq` (it uses TLV not serde) — keep it that way.

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
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
        cwd: Option<String>,
    },
    QuerySearch {
        query: String,
        limit: usize,
        context_lines: usize,
        color: bool,
        cwd: Option<String>,
    },
    Files {
        language: Option<String>,
        path_glob: Option<String>,
        sort: String,
        limit: Option<usize>,
        color: bool,
        cwd: Option<String>,
    },
    Ping,
    Shutdown,
    Reindex,
}

/// Response from daemon to CLI client, sent as TLV binary frames.
#[derive(Debug, PartialEq)]
pub enum DaemonResponse {
    Line { content: String },
    Done { total: usize, duration_ms: u64, stale: bool },
    Error { message: String },
    Pong,
    Progress { message: String },
}
```

**Step 4: Write `ferret-indexer-daemon/src/lib.rs`**

```rust
pub mod types;

pub use types::{DaemonRequest, DaemonResponse};
```

**Step 5: Add `ferret-indexer-daemon` to workspace**

In root `Cargo.toml`, add `"ferret-indexer-daemon"` to the workspace members list.

**Step 6: Run `cargo check -p ferret-indexer-daemon`**

Expected: compiles cleanly.

**Step 7: Commit**

```bash
git add ferret-indexer-daemon/ Cargo.toml
git commit -m "feat(daemon): create ferret-indexer-daemon crate with shared request/response types"
```

---

### Task 2: Move wire protocol into `ferret-indexer-daemon`

**Files:**
- Move: `ferret-indexer-cli/src/wire.rs` → `ferret-indexer-daemon/src/wire.rs`
- Modify: `ferret-indexer-daemon/src/lib.rs` (add `pub mod wire`)
- Modify: `ferret-indexer-cli/src/wire.rs` (replace with re-export or thin wrapper)
- Modify: `ferret-indexer-cli/src/daemon.rs` (update `wire::` imports)

**Step 1: Copy `ferret-indexer-cli/src/wire.rs` to `ferret-indexer-daemon/src/wire.rs`**

Copy the file as-is. Change the `use crate::daemon::DaemonResponse` import to `use crate::types::DaemonResponse`.

**Step 2: Export from `ferret-indexer-daemon/src/lib.rs`**

Add `pub mod wire;` to lib.rs.

**Step 3: Add `ferret-indexer-daemon` dependency to `ferret-indexer-cli/Cargo.toml`**

```toml
ferret-indexer-daemon = { path = "../ferret-indexer-daemon" }
```

**Step 4: Update `ferret-indexer-cli/src/wire.rs` to re-export**

Replace the entire file with:

```rust
pub use ferret_indexer_daemon::wire::*;
```

**Step 5: Update `ferret-indexer-cli/src/daemon.rs` imports**

Change `DaemonRequest` and `DaemonResponse` imports to come from `ferret_indexer_daemon` instead of being defined locally. Remove the local enum definitions. Add:

```rust
use ferret_indexer_daemon::{DaemonRequest, DaemonResponse};
```

**Step 6: Run `cargo test -p ferret-indexer-cli`**

Expected: all existing tests pass (wire roundtrip tests, daemon tests).

**Step 7: Run `cargo clippy --workspace -- -D warnings`**

Expected: clean.

**Step 8: Commit**

```bash
git add ferret-indexer-daemon/ ferret-indexer-cli/
git commit -m "refactor(daemon): move wire protocol and types into shared ferret-indexer-daemon crate"
```

---

### Task 3: Move daemon client helpers into `ferret-indexer-daemon`

**Files:**
- Create: `ferret-indexer-daemon/src/client.rs`
- Modify: `ferret-indexer-daemon/src/lib.rs`
- Modify: `ferret-indexer-cli/src/daemon.rs` (use shared client helpers)

**Step 1: Write `ferret-indexer-daemon/src/client.rs`**

Extract from `ferret-indexer-cli/src/daemon.rs`: `socket_path()`, `try_connect()`, `spawn_daemon_process()`, `ensure_daemon()`. Modify `spawn_daemon_process()` to accept the binary path as a parameter instead of using `current_exe()`, so the MCP can pass the `ferret` CLI binary path.

```rust
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use ferret_indexer_core::error::IndexError;

const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Return the Unix socket path for a given repo root.
pub fn socket_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".ferret_index").join("sock")
}

/// Try to connect to a running daemon. Returns None if no daemon is running.
pub async fn try_connect(repo_root: &Path) -> Option<UnixStream> {
    let path = socket_path(repo_root);
    UnixStream::connect(&path).await.ok()
}

/// Spawn a daemon as a detached background process.
///
/// `daemon_bin` is the path to the `ferret` CLI binary that has the
/// `daemon-start` subcommand.
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

/// Find the `ferret` CLI binary.
///
/// Checks:
/// 1. Sibling of the current executable (same directory)
/// 2. `INDEXRS_BIN` environment variable
/// 3. `ferret` in PATH
pub fn find_daemon_binary() -> Result<PathBuf, IndexError> {
    // 1. Sibling of current executable
    if let Ok(current) = std::env::current_exe() {
        let sibling = current.with_file_name("ferret");
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    // 2. INDEXRS_BIN env var
    if let Ok(bin) = std::env::var("INDEXRS_BIN") {
        let path = PathBuf::from(bin);
        if path.exists() {
            return Ok(path);
        }
    }

    // 3. Search PATH via `which`
    let output = std::process::Command::new("which")
        .arg("ferret")
        .output()
        .map_err(IndexError::Io)?;
    if output.status.success() {
        let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path_str.is_empty() {
            return Ok(PathBuf::from(path_str));
        }
    }

    Err(IndexError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "could not find 'ferret' binary (install it or set INDEXRS_BIN)",
    )))
}
```

**Step 2: Export from `ferret-indexer-daemon/src/lib.rs`**

```rust
pub mod client;
pub mod types;
pub mod wire;

pub use types::{DaemonRequest, DaemonResponse};
pub use client::{socket_path, try_connect, ensure_daemon, find_daemon_binary};
```

**Step 3: Update `ferret-indexer-cli/src/daemon.rs`**

Replace the local `socket_path`, `try_connect`, `spawn_daemon_process`, and `ensure_daemon` functions with calls to `ferret_indexer_daemon::client`. The CLI's `spawn_daemon_process` uses `current_exe()`, so the CLI's `ensure_daemon` wrapper should call:

```rust
pub async fn ensure_daemon(repo_root: &Path) -> Result<UnixStream, IndexError> {
    let bin = std::env::current_exe().map_err(IndexError::Io)?;
    ferret_indexer_daemon::client::ensure_daemon(&bin, repo_root).await
}
```

Keep this as a local thin wrapper so the CLI's existing call sites don't change.

**Step 4: Run `cargo test -p ferret-indexer-cli`**

Expected: all existing tests pass.

**Step 5: Run `cargo test -p ferret-indexer-daemon`**

Expected: compiles, any wire tests pass.

**Step 6: Run `cargo clippy --workspace -- -D warnings`**

Expected: clean.

**Step 7: Commit**

```bash
git add ferret-indexer-daemon/ ferret-indexer-cli/
git commit -m "refactor(daemon): move client helpers (ensure_daemon, socket_path) into shared crate"
```

---

### Task 4: Add `ferret-indexer-daemon` dependency to MCP and create daemon client module

**Files:**
- Modify: `ferret-mcp/Cargo.toml`
- Create: `ferret-mcp/src/daemon_client.rs`
- Modify: `ferret-mcp/src/main.rs` (add module declaration)

**Step 1: Add dependency to `ferret-mcp/Cargo.toml`**

```toml
ferret-indexer-daemon = { path = "../ferret-indexer-daemon" }
```

**Step 2: Write the failing test for `daemon_client.rs`**

Create `ferret-mcp/src/daemon_client.rs` with a test that validates the client can build and send a request:

```rust
//! Daemon client for MCP tool dispatch.
//!
//! Handles connecting to the ferret daemon and sending requests for
//! search_code, search_files, and reindex. Returns collected plain-text
//! response lines for the MCP server to wrap in CallToolResult.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use ferret_indexer_daemon::{DaemonRequest, DaemonResponse};

/// Daemon connection state for the MCP server.
///
/// Lazily connects on first use. Reconnects if the connection drops.
pub struct DaemonClient {
    repo_root: PathBuf,
    conn: Mutex<Option<(BufReader<tokio::net::unix::OwnedReadHalf>, tokio::net::unix::OwnedWriteHalf)>>,
}

/// Result of a daemon request: collected lines + metadata.
pub struct DaemonResult {
    /// All Line content strings joined with newlines.
    pub text: String,
    /// Total count from Done frame.
    pub total: usize,
    /// Whether the index was stale at query time.
    pub stale: bool,
    /// Duration in ms reported by daemon.
    pub duration_ms: u64,
}

impl DaemonClient {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            conn: Mutex::new(None),
        }
    }

    /// Send a request and collect all Line responses until Done.
    pub async fn request(&self, req: DaemonRequest) -> Result<DaemonResult, String> {
        let mut guard = self.conn.lock().await;

        // Ensure we have a connection (lazy connect).
        if guard.is_none() {
            let stream = self.connect().await?;
            let (read, write) = stream.into_split();
            *guard = Some((BufReader::new(read), write));
        }

        let (reader, writer) = guard.as_mut().unwrap();

        // Send the request as a JSON line.
        let json = serde_json::to_string(&req)
            .map_err(|e| format!("failed to serialize request: {e}"))?;
        writer
            .write_all(format!("{json}\n").as_bytes())
            .await
            .map_err(|e| {
                // Connection dropped — clear it so next call reconnects.
                format!("daemon write error: {e}")
            })?;

        // Read responses until Done or Error.
        let mut lines = Vec::new();
        loop {
            let resp = ferret_indexer_daemon::wire::read_response(reader)
                .await
                .map_err(|e| format!("daemon read error: {e}"))?;

            match resp {
                DaemonResponse::Line { content } => {
                    lines.push(content);
                }
                DaemonResponse::Done { total, duration_ms, stale } => {
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
                DaemonResponse::Progress { message } => {
                    tracing::info!("daemon: {message}");
                }
                DaemonResponse::Pong => {}
            }
        }
    }

    /// Connect to daemon, spawning it if needed.
    async fn connect(&self) -> Result<UnixStream, String> {
        let daemon_bin = ferret_indexer_daemon::find_daemon_binary()
            .map_err(|e| format!("cannot find ferret binary: {e}"))?;
        ferret_indexer_daemon::ensure_daemon(&daemon_bin, &self.repo_root)
            .await
            .map_err(|e| format!("failed to connect to daemon: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_client_creation() {
        let client = DaemonClient::new(PathBuf::from("/tmp/test-repo"));
        assert_eq!(client.repo_root, PathBuf::from("/tmp/test-repo"));
    }
}
```

**Step 3: Add module to `ferret-mcp/src/main.rs`**

Add `pub mod daemon_client;` to the module declarations.

**Step 4: Run `cargo check -p ferret-mcp`**

Expected: compiles cleanly.

**Step 5: Commit**

```bash
git add ferret-mcp/
git commit -m "feat(mcp): add daemon client module for dispatching to ferret daemon"
```

---

### Task 5: Wire `search_code` to dispatch through daemon

**Files:**
- Modify: `ferret-mcp/src/server.rs` (update `FerretServer` and `search_code`)
- Modify: `ferret-mcp/src/main.rs` (create `DaemonClient` and pass to server)

**Step 1: Add `DaemonClient` to `FerretServer`**

Add a field to `FerretServer`:

```rust
pub struct FerretServer {
    pub index_state: Arc<IndexState>,
    pub root_path: Option<PathBuf>,
    start_time: Instant,
    daemon: Option<Arc<DaemonClient>>,
}
```

Update `new()` to accept `daemon: Option<Arc<DaemonClient>>` and store it. Update existing `build_test_server` helper in tests to pass `None` for daemon.

**Step 2: Rewrite `search_code` to dispatch via daemon**

The new `search_code` method:
1. Validates parameters (context_lines, max_results) — keep existing validation
2. Builds the query string using `build_query_string()` — keep existing helper
3. If daemon is available: sends `DaemonRequest::QuerySearch` with `color: false`, collects response, wraps in `CallToolResult`
4. If daemon is not available (None): falls back to current direct-index search

```rust
async fn search_code(
    &self,
    #[tool(aggr)] params: SearchCodeParams,
) -> Result<CallToolResult, rmcp::Error> {
    // Parameter validation (unchanged)
    let context_lines = params.context_lines.unwrap_or(2);
    if context_lines > 10 {
        return Ok(errors::invalid_parameter(
            "context_lines",
            &format!("must be between 0 and 10, got {context_lines}"),
        ));
    }
    let max_results = params.max_results.unwrap_or(20);
    if !(1..=100).contains(&max_results) {
        return Ok(errors::invalid_parameter(
            "max_results",
            &format!("must be between 1 and 100, got {max_results}"),
        ));
    }
    let case_sensitive = params.case_sensitive.unwrap_or(false);

    // Build query string with filters
    let query_string = build_query_string(
        &params.query,
        params.path.as_deref(),
        params.language.as_deref(),
        case_sensitive,
    );

    // Dispatch via daemon if available
    if let Some(ref daemon) = self.daemon {
        let req = DaemonRequest::QuerySearch {
            query: query_string,
            limit: max_results,
            context_lines: context_lines as usize,
            color: false,
            cwd: None,
        };

        match daemon.request(req).await {
            Ok(result) => {
                if result.total == 0 {
                    return Ok(errors::no_results(
                        &params.query,
                        &[],
                    ));
                }
                let mut text = String::new();
                if result.stale {
                    text.push_str("Warning: Index is updating, results may be incomplete.\n\n");
                }
                text.push_str(&result.text);
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(e) => {
                Ok(errors::invalid_query(&format!("Daemon error: {e}")))
            }
        }
    } else {
        // Fallback: direct index search (existing code)
        self.search_code_direct(&params, &query_string, context_lines, max_results).await
    }
}
```

Extract the current direct-search body into a private `search_code_direct()` method so the fallback path is clean.

**Step 3: Update `main.rs` to create `DaemonClient`**

In `main.rs`, after finding `repo_root`, create the daemon client:

```rust
let daemon = Arc::new(DaemonClient::new(repo_root.clone()));
let server = FerretServer::new(index_state, Some(repo_root), Some(daemon));
```

**Step 4: Run `cargo check -p ferret-mcp`**

Expected: compiles cleanly.

**Step 5: Run `cargo test -p ferret-mcp`**

Expected: existing tests pass (they use `daemon: None` so they hit the fallback path).

**Step 6: Run `cargo clippy --workspace -- -D warnings`**

Expected: clean.

**Step 7: Commit**

```bash
git add ferret-mcp/
git commit -m "feat(mcp): route search_code through daemon for warm-index TTFB"
```

---

### Task 6: Wire `search_files` to dispatch through daemon

**Files:**
- Modify: `ferret-mcp/src/server.rs` (`search_files` method)

**Step 1: Rewrite `search_files` with daemon dispatch**

Same pattern as search_code. Send `DaemonRequest::Files` with `color: false`, collect lines, return as text. Fall back to direct search if no daemon.

```rust
async fn search_files(
    &self,
    #[tool(aggr)] params: SearchFilesParams,
) -> Result<CallToolResult, rmcp::Error> {
    let max_results = params.max_results.unwrap_or(30).min(200);
    if max_results == 0 {
        return Ok(errors::invalid_parameter(
            "max_results",
            "must be between 1 and 200",
        ));
    }

    if let Some(ref daemon) = self.daemon {
        let req = DaemonRequest::Files {
            language: params.language.clone(),
            path_glob: Some(params.query.clone()),
            sort: "path".to_string(),
            limit: Some(max_results),
            color: false,
            cwd: None,
        };

        match daemon.request(req).await {
            Ok(result) => {
                if result.total == 0 {
                    let text = format!("No files found matching \"{}\".", params.query);
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }
                Ok(CallToolResult::success(vec![Content::text(result.text)]))
            }
            Err(e) => {
                Ok(errors::invalid_parameter("query", &format!("Daemon error: {e}")))
            }
        }
    } else {
        self.search_files_direct(params).await
    }
}
```

Extract the current direct body into `search_files_direct()`.

**Step 2: Run `cargo test -p ferret-mcp`**

Expected: existing tests pass (fallback path).

**Step 3: Run `cargo clippy --workspace -- -D warnings`**

Expected: clean.

**Step 4: Commit**

```bash
git add ferret-mcp/src/server.rs
git commit -m "feat(mcp): route search_files through daemon"
```

---

### Task 7: Wire `reindex` to dispatch through daemon

**Files:**
- Modify: `ferret-mcp/src/server.rs` (`reindex` method)

**Step 1: Update `reindex` to send daemon request**

```rust
async fn reindex(
    &self,
    #[tool(aggr)] params: ReindexParams,
) -> Result<CallToolResult, rmcp::Error> {
    if let Some(ref daemon) = self.daemon {
        match daemon.request(DaemonRequest::Reindex).await {
            Ok(result) => {
                Ok(CallToolResult::success(vec![Content::text(result.text)]))
            }
            Err(e) => {
                Ok(CallToolResult::error(vec![Content::text(
                    format!("Reindex failed: {e}"),
                )]))
            }
        }
    } else {
        let repo_label = params.repo.as_deref().unwrap_or("default repository");
        let full = params.full.unwrap_or(false);
        let mode = if full { "full" } else { "incremental" };
        let output = format!(
            "Reindex requested for {repo_label} ({mode})\n\
             \n\
             Reindexing is not yet available (no daemon connection).\n\
             To reindex, use the CLI: ferret reindex"
        );
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}
```

**Step 2: Run `cargo test -p ferret-mcp`**

Expected: all tests pass.

**Step 3: Run `cargo clippy --workspace -- -D warnings`**

Expected: clean.

**Step 4: Commit**

```bash
git add ferret-mcp/src/server.rs
git commit -m "feat(mcp): route reindex through daemon"
```

---

### Task 8: Remove `IndexState` startup load from MCP main when daemon is available

**Files:**
- Modify: `ferret-mcp/src/main.rs`

**Step 1: Simplify startup**

The MCP server no longer needs to load segments from disk for search — only for `get_file` and `index_status` which remain client-side. Keep the `recover_segments` + `IndexState` for those tools, but it's no longer critical for startup latency. The daemon handles the hot path.

No change needed to the recovery code — it still serves `get_file` and `index_status`. But update the startup log to indicate daemon mode:

```rust
eprintln!("daemon mode: search tools will dispatch to ferret daemon");
```

**Step 2: Run `cargo test -p ferret-mcp`**

Expected: all tests pass.

**Step 3: Commit**

```bash
git add ferret-mcp/src/main.rs
git commit -m "feat(mcp): log daemon dispatch mode on startup"
```

---

### Task 9: Handle daemon connection errors gracefully with fallback

**Files:**
- Modify: `ferret-mcp/src/daemon_client.rs`
- Modify: `ferret-mcp/src/server.rs`

**Step 1: Add reconnection logic to `DaemonClient`**

If a request fails due to a broken connection, clear the cached connection and retry once:

```rust
pub async fn request(&self, req: DaemonRequest) -> Result<DaemonResult, String> {
    match self.request_inner(&req).await {
        Ok(result) => Ok(result),
        Err(e) => {
            // Connection might be stale — clear and retry once.
            tracing::warn!("daemon request failed, retrying: {e}");
            {
                let mut guard = self.conn.lock().await;
                *guard = None;
            }
            self.request_inner(&req).await
        }
    }
}
```

Move the current `request` body into `request_inner`.

**Step 2: Add test for reconnection behavior**

Write a unit test that verifies the client clears its connection on error.

**Step 3: Run `cargo test -p ferret-mcp`**

Expected: all tests pass.

**Step 4: Run `cargo clippy --workspace -- -D warnings`**

Expected: clean.

**Step 5: Commit**

```bash
git add ferret-mcp/
git commit -m "fix(mcp): retry daemon request once on connection failure"
```

---

### Task 10: Full integration test and CI verification

**Files:**
- No new files

**Step 1: Run full workspace tests**

```bash
cargo test --workspace
```

Expected: all tests pass across all crates.

**Step 2: Run CI checks**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

Expected: clean.

**Step 3: Commit any fixups if needed**

---

## Notes

- **`get_file` and `index_status` remain unchanged** — they read directly from `IndexState` loaded at MCP startup. This is fine because they don't benefit from warm-index TTFB (they're reading metadata/content, not searching posting lists).
- **`ping` remains local** — it just returns the server version string.
- **Daemon output format**: The daemon sends pre-formatted plain text (with `color: false`). The MCP loses its `## path (Language, N matches)` / `L42:*` formatting for search results, replacing it with the daemon's `path:line:col: content` format. This is acceptable — both formats are LLM-readable, and the TTFB win matters more.
- **Fallback path**: When `daemon: None` (e.g., in tests or when the CLI binary isn't installed), all tools fall back to their current direct-index implementation. This means existing tests continue to work without modification.
- **Connection lifecycle**: The MCP server is long-lived (runs as long as the MCP client is connected). The daemon has a 5-minute idle timeout. The `DaemonClient` reconnects lazily, so if the daemon times out between tool calls, the next call will re-spawn it via `ensure_daemon()`.
