# Skip Startup Catchup on Reindex Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** When `ferret reindex` spawns a new daemon, skip the automatic startup catchup so only the explicit reindex runs — eliminating redundant work.

**Architecture:** Thread a `--skip-catchup` flag from `spawn_daemon_process` through to `start_daemon`. When the flag is set, the daemon skips Phase 1 catchup and immediately marks `caught_up = true`, proceeding directly to the live watcher. The reindex CLI path passes this flag; all other daemon-spawning paths (search, files, status, etc.) continue to catchup normally.

**Tech Stack:** Rust, clap, tokio

---

### Task 1: Add `--skip-catchup` flag to `DaemonStart` CLI variant

**Files:**
- Modify: `ferret-indexer-cli/src/args.rs:197-198`

**Step 1: Add the flag to the `DaemonStart` variant**

In `args.rs`, change the `DaemonStart` variant from a unit variant to a struct variant with a `skip_catchup` field:

```rust
    /// Internal: run as daemon process (hidden from help)
    #[command(name = "daemon-start", hide = true)]
    DaemonStart {
        /// Skip startup catch-up (used when reindex will run immediately after)
        #[arg(long)]
        skip_catchup: bool,
    },
```

**Step 2: Update `main.rs` to pass the flag through**

In `main.rs:280-284`, update the match arm to destructure and pass `skip_catchup`:

```rust
        Command::DaemonStart { skip_catchup } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            daemon::start_daemon(&repo_root, skip_catchup).await?;
            Ok(ExitCode::Success)
        }
```

**Step 3: Run `cargo check -p ferret-indexer-cli`**

Expected: Compile error — `start_daemon` doesn't accept the new parameter yet. That's fine, we fix it in Task 2.

**Step 4: Commit**

```bash
git add ferret-indexer-cli/src/args.rs ferret-indexer-cli/src/main.rs
git commit -m "feat(cli): add --skip-catchup flag to daemon-start command"
```

---

### Task 2: Update `start_daemon` to accept and honor `skip_catchup`

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs:96` (function signature)
- Modify: `ferret-indexer-cli/src/daemon.rs:114-163` (catchup block)

**Step 1: Change the `start_daemon` signature**

At `daemon.rs:96`, add the parameter:

```rust
pub async fn start_daemon(repo_root: &Path, skip_catchup: bool) -> Result<(), IndexError> {
```

**Step 2: Conditionally skip catchup**

Replace the background catchup+watcher spawn block (`daemon.rs:114-164`) with a conditional. When `skip_catchup` is true, immediately mark `caught_up = true` and only start the live watcher (no catchup phase):

```rust
    // Spawn background catch-up + live indexing task.
    {
        let mgr = manager.clone();
        let cu = caught_up.clone();
        let rf = reindex_flag.clone();
        let repo = repo_root.to_path_buf();
        let idir = ferret_dir.clone();
        tokio::spawn(async move {
            if !skip_catchup {
                // Phase 1: catch-up.
                match tokio::task::spawn_blocking({
                    let repo = repo.clone();
                    let idir = idir.clone();
                    let mgr = mgr.clone();
                    move || ferret_indexer_core::run_catchup(&repo, &idir, &mgr)
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
            } else {
                tracing::info!("daemon skipping catch-up (--skip-catchup)");
            }
            cu.store(true, Ordering::SeqCst);
            tracing::info!("daemon catch-up complete, starting live watcher");

            // Phase 2: start HybridDetector for live changes.
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
```

**Step 3: Run `cargo check -p ferret-indexer-cli`**

Expected: Compiles cleanly.

**Step 4: Run `cargo clippy --workspace -- -D warnings`**

Expected: No warnings.

**Step 5: Commit**

```bash
git add ferret-indexer-cli/src/daemon.rs
git commit -m "feat(daemon): honor --skip-catchup flag to skip startup catch-up"
```

---

### Task 3: Thread `skip_catchup` through `spawn_daemon_process` and `ensure_daemon`

**Files:**
- Modify: `ferret-indexer-daemon/src/client.rs:28-70` (add `skip_catchup` param to `spawn_daemon_process` and `ensure_daemon`)
- Modify: `ferret-indexer-cli/src/daemon.rs:1401-1404` (CLI's `ensure_daemon` wrapper)
- Modify: `ferret-indexer-cli/src/reindex_display.rs:18` (pass `true`)

**Step 1: Update `spawn_daemon_process` to accept and pass `--skip-catchup`**

In `ferret-indexer-daemon/src/client.rs:31`, add the parameter and conditionally append the flag:

```rust
pub fn spawn_daemon_process(
    daemon_bin: &Path,
    repo_root: &Path,
    skip_catchup: bool,
) -> Result<(), IndexError> {
    let mut cmd = std::process::Command::new(daemon_bin);
    cmd.arg("daemon-start").arg("--repo").arg(repo_root);
    if skip_catchup {
        cmd.arg("--skip-catchup");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(IndexError::Io)?;
    Ok(())
}
```

**Step 2: Update `ensure_daemon` in `client.rs` to accept and pass `skip_catchup`**

In `ferret-indexer-daemon/src/client.rs:47`:

```rust
pub async fn ensure_daemon(
    daemon_bin: &Path,
    repo_root: &Path,
    skip_catchup: bool,
) -> Result<UnixStream, IndexError> {
    // Fast path: daemon already running.
    if let Some(stream) = try_connect(repo_root).await {
        return Ok(stream);
    }

    // Spawn a new daemon process.
    spawn_daemon_process(daemon_bin, repo_root, skip_catchup)?;

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

**Step 3: Update the CLI's `ensure_daemon` wrapper in `daemon.rs`**

At `ferret-indexer-cli/src/daemon.rs:1401-1405`, add the parameter:

```rust
pub async fn ensure_daemon(
    repo_root: &Path,
    skip_catchup: bool,
) -> Result<UnixStream, IndexError> {
    let exe = std::env::current_exe().map_err(IndexError::Io)?;
    ferret_indexer_daemon::client::ensure_daemon(&exe, repo_root, skip_catchup).await
}
```

**Step 4: Fix all callers of `ensure_daemon` to pass `false`**

Search for all calls to `ensure_daemon(` in `ferret-indexer-cli/src/` and add `, false` (except the reindex caller which gets `true`). These are the non-reindex paths (search, files, symbols, status, etc.).

Run: `rg 'ensure_daemon\(' ferret-indexer-cli/src/` to find them all.

For each call like `ensure_daemon(&repo_root).await?`, change to `ensure_daemon(&repo_root, false).await?`.

**The one exception:** In `reindex_display.rs:18`, change to:

```rust
    let stream = ensure_daemon(repo_root, true).await?;
```

**Step 5: Run `cargo check --workspace`**

Expected: Compiles cleanly.

**Step 6: Run `cargo clippy --workspace -- -D warnings`**

Expected: No warnings.

**Step 7: Run `cargo test --workspace`**

Expected: All tests pass.

**Step 8: Commit**

```bash
git add ferret-indexer-daemon/src/client.rs ferret-indexer-cli/src/daemon.rs ferret-indexer-cli/src/reindex_display.rs
git commit -m "feat(reindex): skip daemon catch-up when reindex is the caller

When 'ferret reindex' spawns a new daemon, it passes --skip-catchup so
the daemon skips Phase 1 catch-up. The explicit Reindex request then
does the only catch-up, eliminating redundant work."
```
