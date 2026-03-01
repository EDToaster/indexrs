# Streaming Results via Channel Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a streaming search API that sends `FileMatch` results through a `std::sync::mpsc` channel as they're found, instead of collecting everything into a `Vec` before returning. This enables the CLI/fzf integration to display results incrementally. Support cancellation so search stops when the consumer disconnects (e.g., fzf exits or user refines query).

**Architecture:** The new `search_segments_streaming()` function mirrors the existing `search_segments_with_pattern_and_options()` logic but sends each `FileMatch` through an `mpsc::Sender<FileMatch>` immediately after verification and scoring, rather than accumulating in a `HashMap`. Since streaming results are consumed in arrival order (not sorted by score), deduplication uses the same newest-segment-wins rule but via a `HashSet<PathBuf>` of already-sent paths. The consumer (CLI) handles display. Cancellation is achieved by checking `sender.send()` return value -- if the receiver is dropped, the search loop exits early.

**Tech Stack:** Rust, `std::sync::mpsc`. No new external dependencies needed.

**Key Design Decisions:**
1. **`std::sync::mpsc` over `crossbeam` or `tokio::mpsc`**: The search runs synchronously on a single thread; `std::sync::mpsc` is sufficient and avoids new dependencies. The CLI consumer reads from the `Receiver` on the main thread.
2. **No score-based sorting in streaming mode**: Streaming inherently sacrifices global ordering for latency. Results arrive in segment-order, file-id-order. The CLI's fzf mode doesn't need sorting (fzf does its own ranking).
3. **Deduplication approach**: Process segments in reverse order (newest first). Track sent paths in a `HashSet`. Skip files whose path was already sent by a newer segment. This preserves the "newest wins" invariant without needing a merge step.
4. **Cancellation via channel disconnect**: When `sender.send()` returns `Err(SendError)`, the receiver was dropped (e.g., fzf exited). The search loop breaks immediately.

---

### Task 1: Add `search_segments_streaming` function to `multi_search.rs`

**Files:**
- Modify: `indexrs-core/src/multi_search.rs`

**Step 1: Write the failing test**

Add to the `tests` module in `multi_search.rs`:

```rust
#[test]
fn test_search_segments_streaming_basic() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".indexrs/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![
            InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "lib.rs".to_string(),
                content: b"pub fn lib() { println!(\"world\"); }\n".to_vec(),
                mtime: 0,
            },
        ],
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);
    let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
    let options = SearchOptions::default();

    let (tx, rx) = std::sync::mpsc::channel();
    let result = search_segments_streaming(&snapshot, &pattern, &options, tx);
    assert!(result.is_ok());

    let matches: Vec<FileMatch> = rx.into_iter().collect();
    assert_eq!(matches.len(), 2);
    let paths: Vec<String> = matches.iter().map(|m| m.path.to_string_lossy().to_string()).collect();
    assert!(paths.contains(&"main.rs".to_string()));
    assert!(paths.contains(&"lib.rs".to_string()));
}
```

**Step 2: Write the production code**

Add the `search_segments_streaming` function to `multi_search.rs`. The function:
1. Takes `snapshot: &SegmentList`, `pattern: &MatchPattern`, `options: &SearchOptions`, `sender: mpsc::Sender<FileMatch>`
2. Returns `Result<(), IndexError>`
3. Iterates segments in reverse order (newest first for dedup)
4. Maintains a `HashSet<PathBuf>` of sent paths for dedup
5. For each candidate file, calls the existing `search_single_segment_with_pattern` helper (or reuses its internal logic inline) to produce `FileMatch`es
6. Sends each `FileMatch` via the channel immediately
7. If `sender.send()` fails (receiver dropped), returns `Ok(())` immediately (cancellation)
8. Respects `options.max_results` by counting sent results

```rust
/// Stream search results through a channel as they're found.
///
/// Unlike [`search_segments_with_pattern_and_options()`], this function sends
/// each `FileMatch` through the channel immediately after verification,
/// enabling consumers to display results incrementally.
///
/// Results arrive in segment-order (newest segment first), not sorted by
/// relevance score. The caller is responsible for any post-hoc ordering.
///
/// Cancellation: if the receiving end of the channel is dropped (e.g., fzf
/// exits), the search loop terminates early and returns `Ok(())`.
///
/// Deduplication: segments are processed newest-first. If a file path was
/// already sent from a newer segment, it is skipped in older segments.
pub fn search_segments_streaming(
    snapshot: &SegmentList,
    pattern: &MatchPattern,
    options: &SearchOptions,
    sender: std::sync::mpsc::Sender<FileMatch>,
) -> Result<(), IndexError> {
    if snapshot.is_empty() {
        return Ok(());
    }

    let mut sent_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut sent_count: usize = 0;

    // Process segments in reverse order (newest first) for dedup correctness
    for segment in snapshot.iter().rev() {
        let tombstones = segment.load_tombstones()?;
        let file_matches = search_single_segment_with_pattern(
            segment,
            pattern,
            &tombstones,
            options.context_lines,
            None, // no per-segment limit; we limit globally via sent_count
        )?;

        for fm in file_matches {
            // Dedup: skip if already sent from a newer segment
            if sent_paths.contains(&fm.path) {
                continue;
            }

            sent_paths.insert(fm.path.clone());

            // Send the match; if receiver dropped, stop searching
            if sender.send(fm).is_err() {
                return Ok(());
            }

            sent_count += 1;
            if let Some(max) = options.max_results {
                if sent_count >= max {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}
```

**Step 3: Verify**

Run `cargo test -p indexrs-core -- test_search_segments_streaming_basic` to confirm the test passes.

---

### Task 2: Add streaming cancellation test

**Files:**
- Modify: `indexrs-core/src/multi_search.rs`

**Step 1: Write the test**

Add a test that verifies early cancellation when the receiver is dropped:

```rust
#[test]
fn test_search_segments_streaming_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".indexrs/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // Build a segment with many matching files
    let files: Vec<InputFile> = (0..10)
        .map(|i| InputFile {
            path: format!("file_{i}.rs"),
            content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
            mtime: 0,
        })
        .collect();

    let seg = build_segment(&base_dir, SegmentId(0), files);
    let snapshot: SegmentList = Arc::new(vec![seg]);
    let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
    let options = SearchOptions::default();

    let (tx, rx) = std::sync::mpsc::channel();

    // Receive one result then drop the receiver
    let handle = std::thread::spawn(move || {
        let first = rx.recv().unwrap();
        drop(rx); // drop receiver to signal cancellation
        first
    });

    let result = search_segments_streaming(&snapshot, &pattern, &options, tx);
    assert!(result.is_ok()); // should not error on cancellation

    let first_match = handle.join().unwrap();
    assert!(!first_match.lines.is_empty());
}
```

**Step 2: Verify**

Run `cargo test -p indexrs-core -- test_search_segments_streaming_cancellation`.

---

### Task 3: Add streaming deduplication test

**Files:**
- Modify: `indexrs-core/src/multi_search.rs`

**Step 1: Write the test**

Add a test that verifies dedup across segments (newest wins):

```rust
#[test]
fn test_search_segments_streaming_dedup_newest_wins() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".indexrs/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // Segment 0: old version of main.rs
    let seg0 = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() { println!(\"old version\"); }\n".to_vec(),
            mtime: 100,
        }],
    );

    // Segment 1: new version of main.rs
    let seg1 = build_segment(
        &base_dir,
        SegmentId(1),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() { println!(\"new version\"); }\n".to_vec(),
            mtime: 200,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
    let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
    let options = SearchOptions::default();

    let (tx, rx) = std::sync::mpsc::channel();
    search_segments_streaming(&snapshot, &pattern, &options, tx).unwrap();

    let matches: Vec<FileMatch> = rx.into_iter().collect();
    // Should only have one result (deduped)
    assert_eq!(matches.len(), 1);
    // Should be from the newer segment
    assert!(matches[0].lines[0].content.contains("new version"));
}
```

**Step 2: Verify**

Run `cargo test -p indexrs-core -- test_search_segments_streaming_dedup`.

---

### Task 4: Add streaming max_results test

**Files:**
- Modify: `indexrs-core/src/multi_search.rs`

**Step 1: Write the test**

```rust
#[test]
fn test_search_segments_streaming_max_results() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".indexrs/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let files: Vec<InputFile> = (0..10)
        .map(|i| InputFile {
            path: format!("file_{i}.rs"),
            content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
            mtime: 0,
        })
        .collect();

    let seg = build_segment(&base_dir, SegmentId(0), files);
    let snapshot: SegmentList = Arc::new(vec![seg]);
    let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
    let options = SearchOptions {
        context_lines: 0,
        max_results: Some(3),
    };

    let (tx, rx) = std::sync::mpsc::channel();
    search_segments_streaming(&snapshot, &pattern, &options, tx).unwrap();

    let matches: Vec<FileMatch> = rx.into_iter().collect();
    assert_eq!(matches.len(), 3);
}
```

**Step 2: Verify**

Run `cargo test -p indexrs-core -- test_search_segments_streaming_max_results`.

---

### Task 5: Export `search_segments_streaming` from `lib.rs`

**Files:**
- Modify: `indexrs-core/src/lib.rs`

**Step 1: Update the public API**

Add `search_segments_streaming` to the `pub use multi_search::` block:

```rust
pub use multi_search::{
    search_segments, search_segments_streaming, search_segments_with_options,
    search_segments_with_pattern, search_segments_with_pattern_and_options,
};
```

**Step 2: Verify**

Run `cargo check --workspace` to confirm compilation.

---

### Task 6: Wire streaming search into CLI `search_cmd.rs`

**Files:**
- Modify: `indexrs-cli/src/search_cmd.rs`

**Step 1: Add `run_search_streaming` function**

Add a new function that uses the streaming API instead of the batch API:

```rust
/// Run the search command in streaming mode: results are displayed as they're found.
///
/// Uses `search_segments_streaming` to send results through a channel,
/// formatting and writing each one as it arrives. This gives the user
/// immediate feedback, which is critical for fzf integration.
pub fn run_search_streaming<W: std::io::Write>(
    snapshot: &SegmentList,
    opts: &SearchCmdOptions,
    color: &ColorConfig,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
    let search_opts = SearchOptions {
        context_lines: opts.context_lines,
        max_results: Some(opts.limit),
    };

    let glob_matcher: Option<GlobMatcher> = opts
        .path_glob
        .as_ref()
        .map(|g| Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;

    let (tx, rx) = std::sync::mpsc::channel();
    let pattern = opts.pattern.clone();

    // Run the search on a background thread so we can consume results on this thread
    let snapshot_clone = Arc::clone(snapshot);
    let search_handle = std::thread::spawn(move || {
        indexrs_core::multi_search::search_segments_streaming(
            &snapshot_clone,
            &pattern,
            &search_opts,
            tx,
        )
    });

    let mut has_results = false;
    for file_match in rx {
        let path_str = file_match.path.to_string_lossy();

        // Path filter
        if let Some(ref matcher) = glob_matcher {
            if !matcher.is_match(path_str.as_ref()) {
                continue;
            }
        }

        // Language filter
        if let Some(ref lang) = opts.language {
            if !file_match.language.to_string().eq_ignore_ascii_case(lang) {
                continue;
            }
        }

        has_results = true;
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

            if writer.write_line(&line).is_err() {
                // SIGPIPE or broken pipe -- drop rx to cancel search
                drop(rx);
                let _ = search_handle.join();
                return Ok(ExitCode::Success);
            }
        }
    }
    let _ = writer.finish();

    // Check for search errors
    match search_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(IndexError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "search thread panicked",
            )));
        }
    }

    Ok(if has_results {
        ExitCode::Success
    } else {
        ExitCode::NoResults
    })
}
```

**Step 2: Add the required import**

Add `use std::sync::Arc;` to the imports.

**Step 3: Add a test**

```rust
#[test]
fn test_search_streaming_vimgrep_format() {
    let dir = tempfile::tempdir().unwrap();
    let manager = build_test_index(dir.path());
    let snapshot = manager.snapshot();

    let mut buf = Vec::new();
    let color = ColorConfig::new(false);

    let opts = SearchCmdOptions {
        pattern: MatchPattern::LiteralCaseInsensitive("println".to_string()),
        context_lines: 0,
        limit: 1000,
        language: None,
        path_glob: None,
        stats: false,
    };

    let exit = {
        let mut writer = StreamingWriter::new(&mut buf);
        run_search_streaming(&snapshot, &opts, &color, &mut writer).unwrap()
    };
    let output = String::from_utf8(buf).unwrap();

    assert!(output.contains("src/main.rs:2:"));
    assert!(output.contains("println"));
    assert!(matches!(exit, ExitCode::Success));
}
```

**Step 4: Verify**

Run `cargo test -p indexrs-cli -- test_search_streaming_vimgrep_format`.

---

### Task 7: Wire streaming search into daemon handler

**Files:**
- Modify: `indexrs-cli/src/daemon.rs`

**Step 1: Update `handle_search_request` to use streaming**

Replace the batch search in `handle_search_request` with the streaming approach. The daemon already streams results as `DaemonResponse::Line` messages, so we just need to format results as they arrive instead of collecting them first.

```rust
fn handle_search_request(
    manager: &SegmentManager,
    opts: &SearchCmdOptions,
    color: bool,
) -> Result<(Vec<String>, Duration), String> {
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
```

**Step 2: Verify**

Run `cargo test -p indexrs-cli -- test_daemon_search_returns_results`.

---

### Task 8: Final validation

**Step 1:** Run the full test suite:
```bash
cargo test --workspace
```

**Step 2:** Run clippy:
```bash
cargo clippy --workspace -- -D warnings
```

**Step 3:** Run format check:
```bash
cargo fmt --all -- --check
```

**Step 4:** Auto-format if needed:
```bash
cargo fmt --all
```
