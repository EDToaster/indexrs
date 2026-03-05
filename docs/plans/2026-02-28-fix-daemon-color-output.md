# Fix Daemon Color Output Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Restore ANSI color output for `search` and `files` commands that now route through the daemon.

**Architecture:** Add a `color` boolean to `DaemonRequest::Search` and `DaemonRequest::Files` so the client can forward its tty-based color preference. The daemon handlers pass this through to `ColorConfig::new(color)` instead of hardcoding `false`. This is the minimal fix — the daemon already does all the formatting, it just needs to know whether to emit ANSI codes.

**Tech Stack:** Rust, serde (JSON protocol), nu-ansi-term (existing color dependency)

**Why client-sends-preference, not client-side coloring:** The daemon returns pre-formatted text lines (vimgrep `path:line:col:content` for search, plain paths for files). Applying color client-side would require parsing these lines back apart, duplicating formatting logic, and making the protocol fragile. Since the daemon already calls `ColorConfig::format_search_line()` and `ColorConfig::format_file_path()`, simply passing the client's color flag through the protocol is the 2-line fix.

---

### Task 1: Add `color` field to DaemonRequest variants

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs:31-49` (DaemonRequest enum)

**Step 1: Write the failing test**

Add a serialization roundtrip test that includes the `color` field.

```rust
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
    };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
    match parsed {
        DaemonRequest::Files { color, .. } => assert!(color),
        _ => panic!("expected Files"),
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-cli -- test_request_serialize_search_with_color`
Expected: FAIL — `color` field doesn't exist on the enum variants yet.

**Step 3: Add `color` field to both variants**

In `daemon.rs`, add `color: bool` to both `DaemonRequest::Search` and `DaemonRequest::Files`:

```rust
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
        color: bool,
    },
    Files {
        language: Option<String>,
        path_glob: Option<String>,
        sort: String,
        limit: Option<usize>,
        color: bool,
    },
    Ping,
    Shutdown,
}
```

Then fix all existing code that constructs these variants — add `color: false` to every existing call site that isn't the two new tests:
- `daemon.rs` existing tests: `test_request_serialize_search`, `test_request_serialize_files`, `test_daemon_files_returns_results`, `test_daemon_search_invalid_regex_returns_error`, `test_daemon_search_returns_results`, `test_run_via_daemon_search`
- `main.rs`: the two `DaemonRequest` constructions (will be updated in Task 3)

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferret-indexer-cli -- test_request_serialize`
Expected: PASS — all serialization tests pass including the two new ones.

**Step 5: Commit**

```bash
git add ferret-indexer-cli/src/daemon.rs
git commit -m "feat(daemon): add color field to Search and Files requests"
```

---

### Task 2: Use `color` field in daemon handlers

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs:120-187` (handle_search_request, handle_files_request)

**Step 1: Write the failing test**

Add an integration test that sends a Search request with `color: true` and asserts the response contains ANSI escape codes.

```rust
#[tokio::test]
async fn test_daemon_search_with_color() {
    use ferret_indexer_core::segment::InputFile;

    let dir = tempfile::tempdir().unwrap();
    let ferret_dir = dir.path().join(".ferret_index");
    std::fs::create_dir_all(ferret_dir.join("segments")).unwrap();

    let manager = ferret_indexer_core::SegmentManager::new(&ferret_dir).unwrap();
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

    // Search with color enabled.
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
            DaemonResponse::Line { content } => lines.push(content),
            DaemonResponse::Done { .. } => break,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    assert!(!lines.is_empty());
    // Color-enabled output should contain ANSI escape codes.
    assert!(
        lines.iter().any(|l| l.contains("\x1b[")),
        "expected ANSI color codes in output, got: {:?}",
        lines
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

Run: `cargo test -p ferret-indexer-cli -- test_daemon_search_with_color`
Expected: FAIL — handlers still hardcode `ColorConfig::new(false)`.

**Step 3: Thread `color` through daemon handlers**

In `handle_search_request`, accept the color flag and use it:

```rust
fn handle_search_request(
    manager: &SegmentManager,
    opts: &SearchCmdOptions,
    color: bool,
) -> Result<(Vec<String>, Duration), String> {
    // ... existing regex validation ...
    let start = Instant::now();
    let snapshot = manager.snapshot();
    let color = ColorConfig::new(color);
    // ... rest unchanged ...
}
```

In `handle_files_request`, accept the color flag:

```rust
fn handle_files_request(
    manager: &SegmentManager,
    language: Option<String>,
    path_glob: Option<String>,
    sort: String,
    limit: Option<usize>,
    color: bool,
) -> Result<(Vec<String>, Duration), String> {
    let start = Instant::now();
    let snapshot = manager.snapshot();
    let color = ColorConfig::new(color);
    // ... rest unchanged ...
}
```

In `handle_connection`, pass the `color` field from the request to the handlers:

```rust
// In the Search arm, after building opts:
match handle_search_request(manager, &opts, color) {

// In the Files arm:
match handle_files_request(manager, language, path_glob, sort, limit, color) {
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferret-indexer-cli -- daemon`
Expected: PASS — all daemon tests pass, including the new color test.

**Step 5: Commit**

```bash
git add ferret-indexer-cli/src/daemon.rs
git commit -m "fix(daemon): pass client color preference to search/files handlers"
```

---

### Task 3: Pass color preference from CLI to daemon requests

**Files:**
- Modify: `ferret-indexer-cli/src/main.rs:73-112` (Search and Files request construction)

**Step 1: Update Search request construction**

In `main.rs`, the `run()` function receives `color: &ColorConfig`. Pass `color.enabled` into the daemon request:

```rust
let request = daemon::DaemonRequest::Search {
    query,
    regex,
    case_sensitive: eff_case_sensitive,
    ignore_case: eff_ignore_case,
    limit,
    context_lines: context.unwrap_or(0),
    language,
    path_glob: path,
    color: color.enabled,
};
```

**Step 2: Update Files request construction**

```rust
let request = daemon::DaemonRequest::Files {
    language,
    path_glob: path,
    sort: sort_str.to_string(),
    limit,
    color: color.enabled,
};
```

**Step 3: Run the full test suite**

Run: `cargo test -p ferret-indexer-cli`
Expected: PASS — all tests pass.

Run: `cargo clippy -p ferret-indexer-cli -- -D warnings`
Expected: No warnings.

**Step 4: Commit**

```bash
git add ferret-indexer-cli/src/main.rs
git commit -m "fix(cli): forward color preference to daemon for search and files"
```

---

### Task 4: Verify end-to-end

**Step 1: Build and smoke test**

```bash
cargo build -p ferret-indexer-cli

# Kill any running daemon
rm -f .ferret_index/sock

# Search with color (terminal)
cargo run -p ferret-indexer-cli -- search "trigram"
# Should show colored output (magenta paths, green line numbers, red matches)

# Search without color
cargo run -p ferret-indexer-cli -- --color never search "trigram"
# Should show plain text

# Files with color
cargo run -p ferret-indexer-cli -- files
# Should show colored paths (dim dirs, bold filename, cyan extension)

# Files piped (auto-detects no tty)
cargo run -p ferret-indexer-cli -- files | head -5
# Should show plain text (no ANSI codes)
```

**Step 2: Run full workspace checks**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```
