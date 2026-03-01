# On-Demand Daemon Startup Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** CLI commands (`search`, `files`) connect to a running daemon or auto-start one, routing queries through the Unix socket instead of loading the index directly each time.

**Architecture:** The daemon process is the same `indexrs` binary invoked with a hidden `daemon-start` subcommand. When the CLI handles `search` or `files`, it tries `try_connect()` first. If no daemon is running, it spawns one via `std::process::Command` (detached background process), polls until the socket is ready, then sends the request as JSON-over-newline and streams `DaemonResponse::Line` content to stdout. The daemon keeps the `SegmentManager` loaded in memory across requests, eliminating per-query index load overhead. Non-daemon commands (`preview`, `status`, `symbols`, `reindex`) are unchanged.

**Tech Stack:** Rust, tokio, clap (hidden subcommand), serde_json, Unix domain sockets

---

### Task 1: Add hidden `daemon-start` subcommand and make `run()` async

**Files:**
- Modify: `indexrs-cli/src/args.rs`
- Modify: `indexrs-cli/src/main.rs`

**Step 1: Write the failing test**

No unit test needed for this step — the test is: `cargo build -p indexrs-cli` must compile, and the hidden subcommand must not appear in `--help`.

**Step 2: Add `DaemonStart` variant to `Command` enum**

In `indexrs-cli/src/args.rs`, add a hidden variant at the end of the `Command` enum:

```rust
    /// Internal: run as daemon process (hidden from help)
    #[command(name = "daemon-start", hide = true)]
    DaemonStart,
```

**Step 3: Make `run()` async and wire up `DaemonStart`**

In `indexrs-cli/src/main.rs`:

1. Change `fn run(cli: Cli, color: &ColorConfig) -> Result<ExitCode, indexrs_core::IndexError>` to `async fn run(...)`.
2. Update the call site in `main()`: change `match run(cli, &color)` to `match run(cli, &color).await`.
3. Add the `DaemonStart` match arm:

```rust
Command::DaemonStart => {
    let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
    daemon::start_daemon(&repo_root).await?;
    Ok(ExitCode::Success)
}
```

**Step 4: Verify it builds and existing tests pass**

Run: `cargo build -p indexrs-cli`
Run: `cargo test -p indexrs-cli`
Expected: All pass. No behavior change for existing commands.

**Step 5: Verify hidden from help**

Run: `cargo run -p indexrs-cli -- --help`
Expected: `daemon-start` does NOT appear in the subcommand list.

**Step 6: Commit**

```bash
git add indexrs-cli/src/args.rs indexrs-cli/src/main.rs
git commit -m "feat(cli): add hidden daemon-start subcommand, make run() async"
```

---

### Task 2: Add `spawn_daemon` and `ensure_daemon` functions

**Files:**
- Modify: `indexrs-cli/src/daemon.rs`

**Step 1: Write the failing test**

Add an integration test that verifies `ensure_daemon` returns a working connection, even when no daemon is running (it should spawn one).

```rust
#[tokio::test]
async fn test_ensure_daemon_spawns_and_connects() {
    use indexrs_core::segment::InputFile;

    let dir = tempfile::tempdir().unwrap();
    let indexrs_dir = dir.path().join(".indexrs");
    std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

    // Build a minimal index so the daemon can load it.
    let manager = indexrs_core::SegmentManager::new(&indexrs_dir).unwrap();
    manager
        .index_files(vec![InputFile {
            path: "test.rs".to_string(),
            content: b"fn test() {}\n".to_vec(),
            mtime: 100,
        }])
        .unwrap();
    drop(manager);

    // No daemon running yet — ensure_daemon should spawn one.
    let stream = ensure_daemon(dir.path()).await.expect("should connect");

    // Verify connection works by sending Ping.
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

    // Shutdown the daemon.
    let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
    writer
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p indexrs-cli -- test_ensure_daemon_spawns_and_connects --nocapture`
Expected: FAIL — `ensure_daemon` doesn't exist yet.

**Step 3: Implement `spawn_daemon` and `ensure_daemon`**

Add these two functions to `daemon.rs`:

```rust
/// Maximum time to wait for a spawned daemon to become ready.
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between connection attempts when waiting for daemon startup.
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Spawn a daemon as a detached background process.
///
/// Runs the current binary with `daemon-start --repo <path>` and detaches
/// stdin/stdout/stderr so it survives after the CLI process exits.
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
///
/// Returns a connected `UnixStream` to the daemon's socket.
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
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p indexrs-cli -- test_ensure_daemon_spawns_and_connects --nocapture`
Expected: PASS

**Important note:** This test spawns a real daemon process. It requires the `indexrs` binary to be built first. If the test fails because the binary isn't found, run `cargo build -p indexrs-cli` first, or restructure the test to start the daemon in-process (like the existing ping/pong test). If in-process is preferred, use `tokio::spawn(start_daemon(...))` instead of `spawn_daemon_process`:

```rust
// Alternative: in-process test (doesn't test spawn_daemon_process, but tests ensure_daemon logic)
#[tokio::test]
async fn test_ensure_daemon_connects_to_running() {
    // Start daemon in-process first, then call ensure_daemon
    // ... same as existing ping/pong test pattern ...
    let stream = ensure_daemon(&repo_root).await.expect("should connect");
    // ... verify with Ping/Pong ...
}
```

Write **both** tests: one that tests `ensure_daemon` connecting to an already-running daemon (in-process), and if the binary-spawn test is flaky in CI, gate it behind `#[ignore]`.

**Step 5: Run all tests**

Run: `cargo test -p indexrs-cli`
Expected: All pass.

**Step 6: Commit**

```bash
git add indexrs-cli/src/daemon.rs
git commit -m "feat(daemon): add spawn_daemon and ensure_daemon for on-demand startup"
```

---

### Task 3: Add `run_via_daemon` client function

**Files:**
- Modify: `indexrs-cli/src/daemon.rs`

**Step 1: Write the failing test**

Test that `run_via_daemon` sends a Search request and captures the output correctly.

```rust
#[tokio::test]
async fn test_run_via_daemon_search() {
    use indexrs_core::segment::InputFile;

    let dir = tempfile::tempdir().unwrap();
    let indexrs_dir = dir.path().join(".indexrs");
    std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

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
    assert!(output.contains("println"), "output should contain search results");

    // Shutdown.
    let stream = try_connect(&repo_root).await.unwrap();
    let (_, mut writer) = stream.into_split();
    let req = serde_json::to_string(&DaemonRequest::Shutdown).unwrap();
    writer
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), daemon_handle).await;
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p indexrs-cli -- test_run_via_daemon_search --nocapture`
Expected: FAIL — `run_via_daemon` doesn't exist yet.

**Step 3: Implement `run_via_daemon`**

```rust
use crate::output::{ExitCode, StreamingWriter};

/// Send a request to the daemon and stream results to the writer.
///
/// Connects to the daemon (spawning one if needed), sends the request,
/// reads response lines, and writes each `Line` content to the writer.
/// Returns the appropriate exit code based on the `Done` or `Error` response.
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
            DaemonResponse::Done { total, .. } => {
                let _ = writer.finish();
                return Ok(if total == 0 {
                    ExitCode::NoResults
                } else {
                    ExitCode::Success
                });
            }
            DaemonResponse::Error { message } => {
                return Err(IndexError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    message,
                )));
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
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p indexrs-cli -- test_run_via_daemon_search --nocapture`
Expected: PASS

**Step 5: Run all tests**

Run: `cargo test -p indexrs-cli`
Expected: All pass.

**Step 6: Commit**

```bash
git add indexrs-cli/src/daemon.rs
git commit -m "feat(daemon): add run_via_daemon client function"
```

---

### Task 4: Wire Search and Files commands through daemon

**Files:**
- Modify: `indexrs-cli/src/main.rs`

**Step 1: Update the `Search` match arm**

Replace the direct index-loading path with a daemon request. In the `Command::Search` arm of `run()`:

```rust
Command::Search {
    query,
    regex,
    case_sensitive,
    ignore_case,
    smart_case,
    language,
    path,
    limit,
    context,
    stats,
} => {
    let repo_root = repo::find_repo_root(cli.repo.as_deref())?;

    // smart_case resolution: if query has uppercase, treat as case-sensitive
    let effective_ignore_case = if !case_sensitive && !ignore_case && (smart_case || true) {
        !query.chars().any(|c| c.is_uppercase())
    } else {
        ignore_case
    };
    let effective_case_sensitive = if !case_sensitive && !ignore_case && (smart_case || true) {
        query.chars().any(|c| c.is_uppercase())
    } else {
        case_sensitive
    };

    let request = daemon::DaemonRequest::Search {
        query,
        regex,
        case_sensitive: effective_case_sensitive,
        ignore_case: effective_ignore_case,
        limit,
        context_lines: context.unwrap_or(0),
        language,
        path_glob: path,
    };

    let stdout = std::io::stdout();
    let mut writer = StreamingWriter::new(stdout.lock());
    let exit = daemon::run_via_daemon(&repo_root, request, &mut writer).await?;

    if stats {
        // Note: stats timing is reported by the daemon in Done.duration_ms,
        // but the current protocol doesn't surface it to the client.
        // For now, stats through the daemon path is a no-op.
    }

    Ok(exit)
}
```

**Step 2: Update the `Files` match arm**

```rust
Command::Files {
    query: _,
    language,
    path,
    limit,
    sort,
} => {
    let repo_root = repo::find_repo_root(cli.repo.as_deref())?;

    let sort_str = match sort {
        args::SortOrder::Path => "path",
        args::SortOrder::Modified => "modified",
        args::SortOrder::Size => "size",
    };

    let request = daemon::DaemonRequest::Files {
        language,
        path_glob: path,
        sort: sort_str.to_string(),
        limit,
    };

    let stdout = std::io::stdout();
    let mut writer = StreamingWriter::new(stdout.lock());
    daemon::run_via_daemon(&repo_root, request, &mut writer).await
}
```

**Step 3: Remove unused imports**

After replacing the Search/Files arms, the following imports in `main.rs` are no longer needed:
- Remove `use crate::search_cmd` if only used in the Search arm
- Remove `use crate::files` if only used in the Files arm

Check with `cargo clippy -p indexrs-cli -- -D warnings` to identify any dead imports.

**Note:** Keep the `repo` module import — it's still used by other commands (`Preview`, `Status`, `Reindex`). Keep `search_cmd` and `files` modules declared (they're used by `daemon.rs`).

**Step 4: Verify all tests pass**

Run: `cargo test -p indexrs-cli`
Run: `cargo clippy -p indexrs-cli -- -D warnings`
Expected: All pass, no warnings.

**Step 5: Commit**

```bash
git add indexrs-cli/src/main.rs
git commit -m "feat(cli): route search and files commands through daemon"
```

---

### Task 5: Remove `#![allow(dead_code)]` and clean up

**Files:**
- Modify: `indexrs-cli/src/daemon.rs`

**Step 1: Remove the attribute**

Remove `#![allow(dead_code)]` from line 1 of `daemon.rs`.

**Step 2: Run clippy**

Run: `cargo clippy -p indexrs-cli -- -D warnings`
Expected: PASS — all daemon functions are now used (either by `main.rs` calling `run_via_daemon`/`start_daemon`, or internally by the daemon module).

If there are dead code warnings for items only used in tests (e.g., `socket_path` if it's only used in tests and `ensure_daemon`), add `#[cfg(test)]` or `pub(crate)` as appropriate rather than a blanket `allow`.

**Step 3: Run full test suite and fmt**

Run: `cargo test --workspace`
Run: `cargo fmt --all`
Expected: All pass.

**Step 4: Commit**

```bash
git add indexrs-cli/src/daemon.rs
git commit -m "chore: remove allow(dead_code) from daemon module"
```
