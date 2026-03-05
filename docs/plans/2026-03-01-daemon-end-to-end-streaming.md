# Daemon End-to-End Search Streaming Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make the daemon stream search results to the CLI client as they're found, rather than buffering all results before sending any.

**Architecture:** Replace `handle_search_request` (which collects all formatted lines into a `Vec<String>`) with a new `handle_search_streaming` that runs `search_segments_streaming` on a blocking thread, receives `FileMatch`es via an `mpsc` channel, formats each one into vimgrep lines, and writes `DaemonResponse::Line` JSON messages to the socket immediately. A `tokio::sync::mpsc` channel bridges the blocking search thread to the async socket writer. The `Done` response is sent after the search completes with the final count and timing.

**Tech Stack:** Rust, tokio (async socket writes), std::sync::mpsc (core search streaming), tokio::sync::mpsc (bridge to async), serde_json (response encoding)

---

### Task 1: Add `handle_search_streaming` function

This is the core change. We replace the batch `handle_search_request` with a new function that streams results over the socket as they arrive.

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs:250-280` (replace `handle_search_request`)

**Step 1: Write the failing test**

Add a new test that verifies streaming behavior by checking that results arrive before the `Done` message, reusing the existing daemon test pattern.

Add to the bottom of the `#[cfg(test)] mod tests` block in `ferret-indexer-cli/src/daemon.rs`:

```rust
#[tokio::test]
async fn test_daemon_search_streams_results() {
    use ferret_indexer_core::segment::InputFile;

    let dir = tempfile::tempdir().unwrap();
    let ferret_dir = dir.path().join(".ferret_index");
    std::fs::create_dir_all(ferret_dir.join("segments")).unwrap();

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

    let manager = ferret_indexer_core::SegmentManager::new(&ferret_dir).unwrap();
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

    // Read responses: should get Line messages followed by Done.
    let mut lines = Vec::new();
    let mut got_done = false;
    loop {
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await.unwrap();
        let resp: DaemonResponse = serde_json::from_str(response_line.trim()).unwrap();
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
```

**Step 2: Run test to verify it passes with current code**

This test should pass with the existing batch implementation too (it tests the protocol contract, not internal streaming). Run it to establish the baseline:

Run: `cargo test -p ferret-indexer-cli test_daemon_search_streams_results -- --nocapture`
Expected: PASS

**Step 3: Write `handle_search_streaming` to replace `handle_search_request`**

Replace `handle_search_request` (lines 250-280) with a new function that writes `DaemonResponse` JSON lines directly to a `tokio::sync::mpsc::UnboundedSender<String>` as results arrive, instead of collecting into a `Vec<String>`.

The key insight: `search_segments_streaming` runs on a blocking thread and sends `FileMatch`es via `std::sync::mpsc`. We need a `tokio::sync::mpsc` to bridge to the async socket writer. The pattern is identical to how `DaemonRequest::Reindex` already streams progress messages (see `daemon.rs:497-513`).

Replace the entire `handle_search_request` function and its call site in `handle_connection` with:

```rust
/// Format a FileMatch into vimgrep-style output lines and send each as a
/// DaemonResponse::Line JSON string through the channel.
fn format_and_send_file_match(
    file_match: &ferret_indexer_core::search::FileMatch,
    color: &ColorConfig,
    path_rewriter: &PathRewriter,
    glob_matcher: &Option<globset::GlobMatcher>,
    language_filter: &Option<String>,
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
) -> bool {
    let raw_path = file_match.path.to_string_lossy();

    // Path filter
    if let Some(ref matcher) = glob_matcher {
        if !matcher.is_match(raw_path.as_ref()) {
            return true; // filtered out, keep going
        }
    }

    // Language filter
    if let Some(ref lang) = language_filter {
        if !file_match.language.to_string().eq_ignore_ascii_case(lang) {
            return true; // filtered out, keep going
        }
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

        let resp = serde_json::to_string(&DaemonResponse::Line { content: line }).unwrap();
        if tx.send(resp).is_err() {
            return false; // receiver dropped, stop
        }
    }

    true // keep going
}
```

Then replace the `DaemonRequest::Search` arm in `handle_connection` (lines 369-435) with:

```rust
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
    if let MatchPattern::Regex(ref pat) = pattern {
        if let Err(e) = regex::Regex::new(pat) {
            let resp = serde_json::to_string(&DaemonResponse::Error {
                message: format!("invalid regex: {e}"),
            })
            .unwrap();
            writer
                .write_all(format!("{resp}\n").as_bytes())
                .await
                .map_err(IndexError::Io)?;
            line.clear();
            continue;
        }
    }

    let color_config = ColorConfig::new(color);
    let start = Instant::now();
    let snapshot = manager.snapshot();
    let search_opts = ferret_indexer_core::search::SearchOptions {
        context_lines,
        max_results: Some(limit),
    };

    let glob_matcher: Option<globset::GlobMatcher> = path_glob
        .as_ref()
        .and_then(|g| globset::Glob::new(g).ok().map(|g| g.compile_matcher()));

    let (search_tx, search_rx) = std::sync::mpsc::channel();
    let pattern_clone = pattern.clone();

    // Spawn blocking search thread (same pattern as run_search_streaming).
    let search_handle = tokio::task::spawn_blocking(move || {
        ferret_indexer_core::multi_search::search_segments_streaming(
            &snapshot,
            &pattern_clone,
            &search_opts,
            search_tx,
        )
    });

    // Bridge: blocking mpsc -> tokio mpsc -> async socket writer.
    let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let bridge_handle = tokio::task::spawn_blocking({
        let async_tx = async_tx.clone();
        let language_filter = language.clone();
        move || {
            let mut line_count: usize = 0;
            for file_match in search_rx {
                if !format_and_send_file_match(
                    &file_match,
                    &color_config,
                    &path_rewriter,
                    &glob_matcher,
                    &language_filter,
                    &async_tx,
                ) {
                    break; // receiver dropped
                }
                line_count += file_match.lines.len();
            }
            line_count
        }
    });

    // Drop our copy of async_tx so the channel closes when bridge finishes.
    drop(async_tx);

    // Stream responses to the client as they arrive.
    let mut total: usize = 0;
    while let Some(resp_json) = async_rx.recv().await {
        total += 1;
        if writer
            .write_all(format!("{resp_json}\n").as_bytes())
            .await
            .is_err()
        {
            break; // client disconnected
        }
    }

    // Wait for both tasks to finish.
    let _ = search_handle.await;
    let _ = bridge_handle.await;

    let elapsed = start.elapsed();
    let resp = serde_json::to_string(&DaemonResponse::Done {
        total,
        duration_ms: elapsed.as_millis() as u64,
        stale,
    })
    .unwrap();
    writer
        .write_all(format!("{resp}\n").as_bytes())
        .await
        .map_err(IndexError::Io)?;
}
```

**Step 4: Add required imports**

Add `globset` to the imports at the top of `daemon.rs`. The `use` block needs:

```rust
use ferret_indexer_core::search::MatchPattern;  // already present
```

Verify `globset` is already a dependency of `ferret-indexer-cli` (it is, used by `search_cmd.rs`).

**Step 5: Remove dead code**

Delete the old `handle_search_request` function (the one that collected into `Vec<String>`). It's fully replaced. Also remove the `search_cmd` import if it's only used for `run_search_streaming` (check if `resolve_match_pattern` still uses it — it does, so keep `use crate::search_cmd`).

**Step 6: Run the test suite**

Run: `cargo test -p ferret-indexer-cli -- --nocapture`
Expected: All tests pass, including `test_daemon_search_streams_results`, `test_daemon_search_returns_results`, `test_daemon_search_invalid_regex_returns_error`, `test_run_via_daemon_search`, and `test_daemon_search_with_color`.

**Step 7: Run clippy and fmt**

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: Clean

**Step 8: Commit**

```bash
git add ferret-indexer-cli/src/daemon.rs
git commit -m "feat: stream search results end-to-end through daemon

Replace the batch handle_search_request (which buffered all results into
a Vec<String> before sending any) with inline streaming in the Search
arm of handle_connection. Results now flow:

  core search_segments_streaming (blocking thread)
    -> std::sync::mpsc
    -> bridge thread (formatting + filtering)
    -> tokio::sync::mpsc
    -> async socket writer

This dramatically reduces time-to-first-result for CLI users and
enables proper early termination when piped to head/fzf."
```

---

### Task 2: Clean up unused imports and dead code

After Task 1, `handle_search_request` is gone. Verify nothing else references it and clean up any orphaned imports.

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs` (top-level imports)

**Step 1: Check for dead imports**

Run: `cargo clippy --workspace -- -D warnings`

If clippy reports unused imports (e.g., `StreamingWriter` may no longer be needed in daemon.rs if only `handle_files_request` uses it — but `handle_files_request` still uses `StreamingWriter` via `files::run_files`, so it stays), fix them.

**Step 2: Fix any warnings**

Address any clippy warnings. The `search_cmd` import should still be needed for `resolve_match_pattern`. The `StreamingWriter` import is still needed for `handle_files_request`.

**Step 3: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass

**Step 4: Commit if changes were needed**

```bash
git add ferret-indexer-cli/src/daemon.rs
git commit -m "chore: clean up unused imports after streaming refactor"
```

---

### Task 3: Verify end-to-end behavior with the demo

**Step 1: Build in release mode**

Run: `cargo build --release`
Expected: Clean build

**Step 2: Index a directory and search via daemon**

Run the CLI against the ferret repo itself to verify streaming works in practice:

```bash
# Index the repo
cargo run --release -- init

# Search — results should appear incrementally
cargo run --release -- search "fn main"

# Search with pipe — should terminate early
cargo run --release -- search "fn" | head -5
```

Expected: Results appear immediately (not after a delay), and `head -5` terminates quickly.

**Step 3: No commit needed — this is manual verification**
