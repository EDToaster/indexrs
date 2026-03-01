# Wire Up Daemon Search/Files Handlers Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make the daemon's `Search` and `Files` request handlers execute real queries against the loaded index and stream results back as `DaemonResponse::Line` messages.

**Architecture:** The daemon already has the socket infrastructure, request/response types, and a `SegmentManager`. The stub at `daemon.rs:143-158` currently returns `Done { total: 0 }` for both Search and Files. We replace that stub with calls to the existing `search_cmd::run_search` and `files::run_files` functions, capturing their output into a `Vec<u8>` buffer, splitting into lines, and sending each as a `DaemonResponse::Line`. A final `DaemonResponse::Done` reports total lines and elapsed time.

**Tech Stack:** Rust, tokio, serde_json, indexrs-core (SegmentManager, SearchOptions, MatchPattern)

---

### Task 1: Extract `handle_search` function

**Files:**
- Modify: `indexrs-cli/src/daemon.rs:143-158`
- Test: `indexrs-cli/src/daemon.rs` (existing `test_daemon_ping_pong` pattern)

**Step 1: Write the failing test**

Add a test that starts a daemon with indexed content, sends a Search request, and expects `Line` + `Done` responses with actual results.

```rust
#[tokio::test]
async fn test_daemon_search_returns_results() {
    use indexrs_core::segment::InputFile;

    let dir = tempfile::tempdir().unwrap();
    let indexrs_dir = dir.path().join(".indexrs");
    std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p indexrs-cli -- test_daemon_search_returns_results --nocapture`
Expected: FAIL — the daemon returns `Done { total: 0 }` with no `Line` responses, so `lines` is empty and the assert fails.

**Step 3: Implement `handle_search`**

Replace the `DaemonRequest::Search { .. }` arm in `handle_connection` with a real implementation. Add a helper function `handle_search` that:

1. Resolves the `DaemonRequest::Search` fields into a `MatchPattern` via `search_cmd::resolve_match_pattern`
2. Builds a `SearchCmdOptions`
3. Calls `search_cmd::run_search` writing into a `Vec<u8>` buffer (with color disabled)
4. Splits the buffer into lines, sends each as `DaemonResponse::Line`
5. Sends `DaemonResponse::Done` with the line count and elapsed time

```rust
use std::time::Instant;

use crate::color::ColorConfig;
use crate::files::{self, FilesFilter};
use crate::output::StreamingWriter;
use crate::search_cmd::{self, SearchCmdOptions};
use crate::args::SortOrder;

/// Execute a Search request against the loaded index.
fn handle_search_request(
    manager: &SegmentManager,
    query: String,
    regex: bool,
    case_sensitive: bool,
    ignore_case: bool,
    limit: usize,
    context_lines: usize,
    language: Option<String>,
    path_glob: Option<String>,
) -> (Vec<String>, Duration) {
    let start = Instant::now();
    let snapshot = manager.snapshot();
    let color = ColorConfig::new(false); // No ANSI in daemon responses

    let pattern = search_cmd::resolve_match_pattern(
        &query,
        regex,
        case_sensitive,
        ignore_case,
        false, // smart_case — daemon uses explicit flags
    );
    let opts = SearchCmdOptions {
        pattern,
        context_lines,
        limit,
        language,
        path_glob,
        stats: false,
    };

    let mut buf = Vec::new();
    {
        let mut writer = StreamingWriter::new(&mut buf);
        let _ = search_cmd::run_search(&snapshot, &opts, &color, &mut writer);
    }

    let output = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    (lines, start.elapsed())
}
```

Then update the `DaemonRequest::Search` match arm in `handle_connection`:

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
} => {
    let (lines, elapsed) = handle_search_request(
        manager,
        query,
        regex,
        case_sensitive,
        ignore_case,
        limit,
        context_lines,
        language,
        path_glob,
    );

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
    })
    .unwrap();
    writer
        .write_all(format!("{resp}\n").as_bytes())
        .await
        .map_err(IndexError::Io)?;
}
```

Also update the imports at the top of `daemon.rs` — add `Instant` to the existing `use std::time::Duration` and add the crate-local imports for `search_cmd`, `files`, `color`, `output`, and `args`.

**Step 4: Run test to verify it passes**

Run: `cargo test -p indexrs-cli -- test_daemon_search_returns_results --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add indexrs-cli/src/daemon.rs
git commit -m "feat(daemon): wire up Search handler to real index queries"
```

---

### Task 2: Extract `handle_files` function

**Files:**
- Modify: `indexrs-cli/src/daemon.rs`

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn test_daemon_files_returns_results() {
    use indexrs_core::segment::InputFile;

    let dir = tempfile::tempdir().unwrap();
    let indexrs_dir = dir.path().join(".indexrs");
    std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p indexrs-cli -- test_daemon_files_returns_results --nocapture`
Expected: FAIL — same stub returns `Done { total: 0 }`.

**Step 3: Implement `handle_files_request`**

Add the helper function:

```rust
/// Execute a Files request against the loaded index.
fn handle_files_request(
    manager: &SegmentManager,
    language: Option<String>,
    path_glob: Option<String>,
    sort: String,
    limit: Option<usize>,
) -> (Vec<String>, Duration) {
    let start = Instant::now();
    let snapshot = manager.snapshot();
    let color = ColorConfig::new(false);

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
        let _ = files::run_files(&snapshot, &filter, &color, &mut writer);
    }

    let output = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    (lines, start.elapsed())
}
```

Update the `DaemonRequest::Files` match arm (split it out from the combined `Search | Files` arm):

```rust
DaemonRequest::Files {
    language,
    path_glob,
    sort,
    limit,
} => {
    let (lines, elapsed) = handle_files_request(
        manager,
        language,
        path_glob,
        sort,
        limit,
    );

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
    })
    .unwrap();
    writer
        .write_all(format!("{resp}\n").as_bytes())
        .await
        .map_err(IndexError::Io)?;
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p indexrs-cli -- test_daemon_files_returns_results --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add indexrs-cli/src/daemon.rs
git commit -m "feat(daemon): wire up Files handler to real index queries"
```

---

### Task 3: Add error handling for daemon request handlers

**Files:**
- Modify: `indexrs-cli/src/daemon.rs`

**Step 1: Write the failing test**

Test that an invalid regex in a Search request returns a `DaemonResponse::Error` instead of crashing.

```rust
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p indexrs-cli -- test_daemon_search_invalid_regex_returns_error --nocapture`
Expected: FAIL — the handler panics or returns something other than `DaemonResponse::Error`.

**Step 3: Wrap handler calls with error catching**

Update `handle_search_request` and `handle_files_request` to return `Result` and propagate errors:

```rust
fn handle_search_request(
    manager: &SegmentManager,
    query: String,
    regex: bool,
    case_sensitive: bool,
    ignore_case: bool,
    limit: usize,
    context_lines: usize,
    language: Option<String>,
    path_glob: Option<String>,
) -> Result<(Vec<String>, Duration), String> {
    let start = Instant::now();
    let snapshot = manager.snapshot();
    let color = ColorConfig::new(false);

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

    let mut buf = Vec::new();
    {
        let mut writer = StreamingWriter::new(&mut buf);
        search_cmd::run_search(&snapshot, &opts, &color, &mut writer)
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

fn handle_files_request(
    manager: &SegmentManager,
    language: Option<String>,
    path_glob: Option<String>,
    sort: String,
    limit: Option<usize>,
) -> Result<(Vec<String>, Duration), String> {
    let start = Instant::now();
    let snapshot = manager.snapshot();
    let color = ColorConfig::new(false);

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
        files::run_files(&snapshot, &filter, &color, &mut writer)
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

Update both match arms in `handle_connection` to handle the `Result`:

```rust
DaemonRequest::Search { query, regex, case_sensitive, ignore_case, limit, context_lines, language, path_glob } => {
    match handle_search_request(manager, query, regex, case_sensitive, ignore_case, limit, context_lines, language, path_glob) {
        Ok((lines, elapsed)) => {
            for line_content in &lines {
                let resp = serde_json::to_string(&DaemonResponse::Line {
                    content: line_content.clone(),
                }).unwrap();
                writer.write_all(format!("{resp}\n").as_bytes()).await.map_err(IndexError::Io)?;
            }
            let resp = serde_json::to_string(&DaemonResponse::Done {
                total: lines.len(),
                duration_ms: elapsed.as_millis() as u64,
            }).unwrap();
            writer.write_all(format!("{resp}\n").as_bytes()).await.map_err(IndexError::Io)?;
        }
        Err(msg) => {
            let resp = serde_json::to_string(&DaemonResponse::Error { message: msg }).unwrap();
            writer.write_all(format!("{resp}\n").as_bytes()).await.map_err(IndexError::Io)?;
        }
    }
}
```

Apply the same pattern for the `DaemonRequest::Files` arm.

**Step 4: Run all daemon tests**

Run: `cargo test -p indexrs-cli -- test_daemon --nocapture`
Expected: All 3 new tests + existing `test_daemon_ping_pong` + `test_try_connect_no_daemon` pass.

**Step 5: Commit**

```bash
git add indexrs-cli/src/daemon.rs
git commit -m "feat(daemon): add error handling for search/files handlers"
```

---

### Task 4: Remove `#[allow(unused)]` from daemon module

**Files:**
- Modify: `indexrs-cli/src/main.rs:3`

**Step 1: Remove the annotation**

Change line 3 of `main.rs` from:

```rust
#[allow(unused)]
mod daemon;
```

to:

```rust
mod daemon;
```

**Step 2: Run clippy to check for warnings**

Run: `cargo clippy -p indexrs-cli -- -D warnings`
Expected: PASS (no unused warnings since daemon now imports `search_cmd`, `files`, `color`, `output`, `args` which are all used).

**Step 3: Run the full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

**Step 4: Run fmt**

Run: `cargo fmt --all`

**Step 5: Commit**

```bash
git add indexrs-cli/src/main.rs
git commit -m "chore: remove allow(unused) from daemon module"
```
