# End-to-End Integration Tests Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Full pipeline integration tests covering index → search → incremental update → CLI output → MCP response.

**Architecture:** Two integration test files: `ferret-core/tests/integration.rs` (library-level pipeline tests using `SegmentManager` + `search_segments` directly) and `ferret-cli/tests/cli_integration.rs` (binary-level tests using `std::process::Command`). Both use a shared set of small fixture files copied to `tempdir()` per test.

**Tech Stack:** Rust integration tests, `tempfile` crate, `std::process::Command`, `assert_cmd` not needed (raw Command is sufficient).

---

### Task 1: Create fixture files

**Files:**
- Create: `ferret-core/tests/fixtures/repo/src/main.rs`
- Create: `ferret-core/tests/fixtures/repo/src/lib.rs`
- Create: `ferret-core/tests/fixtures/repo/src/utils.py`
- Create: `ferret-core/tests/fixtures/repo/README.md`
- Create: `ferret-core/tests/fixtures/repo/data/config.toml`

**Step 1: Create the fixture files**

`ferret-core/tests/fixtures/repo/src/main.rs`:
```rust
use std::collections::HashMap;

struct Config {
    name: String,
    values: HashMap<String, i32>,
}

fn main() {
    let config = Config {
        name: "ferret".to_string(),
        values: HashMap::new(),
    };
    println!("Running {}", config.name);
}

fn helper_function(x: i32) -> i32 {
    x * 2 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_helper() {
        assert_eq!(helper_function(3), 7);
    }
}
```

`ferret-core/tests/fixtures/repo/src/lib.rs`:
```rust
pub mod utils;

pub trait Searchable {
    fn search(&self, query: &str) -> Vec<String>;
}

pub fn search_all(items: &[&dyn Searchable], query: &str) -> Vec<String> {
    items.iter().flat_map(|item| item.search(query)).collect()
}

pub struct Index {
    entries: Vec<String>,
}

impl Searchable for Index {
    fn search(&self, query: &str) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.contains(query))
            .cloned()
            .collect()
    }
}
```

`ferret-core/tests/fixtures/repo/src/utils.py`:
```python
"""Utility functions for data processing."""

class DataProcessor:
    def __init__(self, name):
        self.name = name
        self.results = []

    def process(self, data):
        return [x * 2 for x in data]

def helper(items):
    """Filter and transform items."""
    return [str(item).upper() for item in items if item]

def main():
    proc = DataProcessor("default")
    result = proc.process([1, 2, 3])
    print(result)

if __name__ == "__main__":
    main()
```

`ferret-core/tests/fixtures/repo/README.md`:
```markdown
# Test Repository

This is a test repository for ferret integration tests.

It contains sample source files in multiple languages.
```

`ferret-core/tests/fixtures/repo/data/config.toml`:
```toml
[project]
name = "ferret-test"
version = "0.1.0"

[settings]
max_results = 100
enable_cache = true
```

**Step 2: Verify fixtures exist**

Run: `find ferret-core/tests/fixtures/repo -type f | sort`
Expected: 5 files listed.

**Step 3: Commit**

```bash
git add ferret-core/tests/fixtures/
git commit -m "test: add fixture files for e2e integration tests (HHC-66)"
```

---

### Task 2: Core integration test — index and basic search

**Files:**
- Create: `ferret-core/tests/integration.rs`

**Step 1: Write the test file with index + literal search tests**

`ferret-core/tests/integration.rs`:
```rust
//! End-to-end integration tests for the ferret indexing and search pipeline.
//!
//! These tests use small fixture files to verify the full pipeline:
//! index → search → incremental update.

use std::path::{Path, PathBuf};

use ferret_indexer_core::{
    ChangeEvent, ChangeKind, InputFile, SearchOptions, SegmentManager,
    search_segments, search_segments_with_options,
    search_segments_with_query,
    query::parse_query,
};

/// Path to the fixture repo relative to the workspace root.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("repo")
}

/// Read all fixture files and return them as `InputFile`s.
fn load_fixture_files() -> Vec<InputFile> {
    let base = fixtures_dir();
    let paths = &[
        "src/main.rs",
        "src/lib.rs",
        "src/utils.py",
        "README.md",
        "data/config.toml",
    ];
    paths
        .iter()
        .map(|rel| {
            let full = base.join(rel);
            let content = std::fs::read(&full)
                .unwrap_or_else(|e| panic!("failed to read fixture {rel}: {e}"));
            InputFile {
                path: rel.to_string(),
                content,
                mtime: 1000,
            }
        })
        .collect()
}

/// Create a SegmentManager in a temp dir, index fixtures, return (manager, tempdir).
fn build_index() -> (SegmentManager, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let index_dir = tmp.path().join(".ferret_index");
    let manager = SegmentManager::new(&index_dir).unwrap();
    let files = load_fixture_files();
    manager.index_files(files).unwrap();
    (manager, tmp)
}

#[test]
fn test_index_known_files() {
    let (manager, _tmp) = build_index();
    let snap = manager.snapshot();

    // Should have exactly 1 segment (5 small files fit in one).
    assert_eq!(snap.len(), 1);

    // Total file count across all segments.
    let total: usize = snap.iter().map(|s| s.entry_count()).sum();
    assert_eq!(total, 5);

    // Verify all expected paths are in the metadata.
    let seg = &snap[0];
    let paths: Vec<String> = (0..seg.entry_count())
        .map(|i| {
            seg.metadata_reader()
                .read_entry(ferret_indexer_core::FileId(i as u32))
                .unwrap()
                .path
                .clone()
        })
        .collect();
    assert!(paths.contains(&"src/main.rs".to_string()));
    assert!(paths.contains(&"src/lib.rs".to_string()));
    assert!(paths.contains(&"src/utils.py".to_string()));
    assert!(paths.contains(&"README.md".to_string()));
    assert!(paths.contains(&"data/config.toml".to_string()));
}

#[test]
fn test_search_literal() {
    let (manager, _tmp) = build_index();
    let snap = manager.snapshot();
    let result = search_segments(&snap, "fn main").unwrap();

    assert!(result.total_file_count >= 1, "should find at least 1 file");

    // main.rs should be in the results.
    let main_match = result.files.iter().find(|f| f.path == Path::new("src/main.rs"));
    assert!(main_match.is_some(), "src/main.rs should match 'fn main'");

    let main_match = main_match.unwrap();
    // "fn main" appears on line 10 of our fixture.
    assert!(
        main_match.lines.iter().any(|l| l.line_number == 10),
        "fn main() should be on line 10, got lines: {:?}",
        main_match.lines.iter().map(|l| l.line_number).collect::<Vec<_>>()
    );
}

#[test]
fn test_search_no_results() {
    let (manager, _tmp) = build_index();
    let snap = manager.snapshot();
    let result = search_segments(&snap, "xyzzy_nonexistent_string").unwrap();

    assert_eq!(result.total_match_count, 0);
    assert_eq!(result.total_file_count, 0);
    assert!(result.files.is_empty());
}
```

**Step 2: Run tests**

Run: `cargo test -p ferret-indexer-core --test integration -- --nocapture`
Expected: All 3 tests PASS.

**Step 3: Commit**

```bash
git add ferret-core/tests/integration.rs
git commit -m "test: add core integration tests for indexing and literal search (HHC-66)"
```

---

### Task 3: Core integration tests — regex, filters, case-insensitive, context

**Files:**
- Modify: `ferret-core/tests/integration.rs`

**Step 1: Add regex search test**

```rust
#[test]
fn test_search_regex() {
    let (manager, _tmp) = build_index();
    let snap = manager.snapshot();

    let query = parse_query("/def \\w+/").unwrap();
    let result = search_segments_with_query(&snap, &query, &SearchOptions::default()).unwrap();

    assert!(result.total_file_count >= 1, "regex should match Python file");

    let py_match = result.files.iter().find(|f| f.path == Path::new("src/utils.py"));
    assert!(py_match.is_some(), "src/utils.py should match /def \\w+/");

    // Should NOT match .rs files (they use "fn", not "def").
    let rs_matches: Vec<_> = result.files.iter()
        .filter(|f| f.path.extension().is_some_and(|e| e == "rs"))
        .collect();
    assert!(rs_matches.is_empty(), "regex /def \\w+/ should not match .rs files");
}
```

**Step 2: Add path filter test**

```rust
#[test]
fn test_search_path_filter() {
    let (manager, _tmp) = build_index();
    let snap = manager.snapshot();

    // "path:src/" combined with a search term.
    let query = parse_query("path:src/ fn").unwrap();
    let result = search_segments_with_query(&snap, &query, &SearchOptions::default()).unwrap();

    // All results should have paths starting with "src/".
    for file in &result.files {
        assert!(
            file.path.starts_with("src/"),
            "expected path starting with src/, got: {}",
            file.path.display()
        );
    }

    // README.md and data/config.toml should NOT appear.
    assert!(result.files.iter().all(|f| f.path != Path::new("README.md")));
    assert!(result.files.iter().all(|f| f.path != Path::new("data/config.toml")));
}
```

**Step 3: Add language filter test**

```rust
#[test]
fn test_search_language_filter() {
    let (manager, _tmp) = build_index();
    let snap = manager.snapshot();

    // Search for "def" with language:python filter.
    let query = parse_query("lang:python def").unwrap();
    let result = search_segments_with_query(&snap, &query, &SearchOptions::default()).unwrap();

    assert!(result.total_file_count >= 1);
    for file in &result.files {
        assert_eq!(
            file.language,
            ferret_indexer_core::Language::Python,
            "all matches should be Python, got {:?} for {}",
            file.language,
            file.path.display()
        );
    }
}
```

**Step 4: Add case-insensitive search test**

```rust
#[test]
fn test_search_case_insensitive() {
    let (manager, _tmp) = build_index();
    let snap = manager.snapshot();

    // Default search is case-insensitive. "FN MAIN" should still match "fn main".
    let result = search_segments(&snap, "FN MAIN").unwrap();
    assert!(
        result.total_file_count >= 1,
        "'FN MAIN' should match case-insensitively"
    );

    let main_match = result.files.iter().find(|f| f.path == Path::new("src/main.rs"));
    assert!(main_match.is_some());
}
```

**Step 5: Add context lines test**

```rust
#[test]
fn test_search_context_lines() {
    let (manager, _tmp) = build_index();
    let snap = manager.snapshot();

    let opts = SearchOptions {
        context_lines: 2,
        max_results: None,
    };
    let result = search_segments_with_options(&snap, "fn main", &opts).unwrap();

    assert!(result.total_file_count >= 1);
    let main_match = result.files.iter().find(|f| f.path == Path::new("src/main.rs")).unwrap();
    let line = main_match.lines.iter().find(|l| l.line_number == 10).unwrap();

    // With context_lines=2, we should have up to 2 lines before and after.
    assert!(
        !line.context_before.is_empty(),
        "context_before should be populated with context_lines=2"
    );
    assert!(
        !line.context_after.is_empty(),
        "context_after should be populated with context_lines=2"
    );
}
```

**Step 6: Run tests**

Run: `cargo test -p ferret-indexer-core --test integration -- --nocapture`
Expected: All 8 tests PASS (3 from Task 2 + 5 new).

**Step 7: Commit**

```bash
git add ferret-core/tests/integration.rs
git commit -m "test: add regex, filter, case-insensitive, and context search tests (HHC-66)"
```

---

### Task 4: Core integration tests — incremental modify and delete

**Files:**
- Modify: `ferret-core/tests/integration.rs`

**Step 1: Add incremental modify test**

```rust
#[test]
fn test_incremental_modify() {
    let (manager, _tmp) = build_index();
    let snap = manager.snapshot();

    // Verify "UNIQUE_MARKER_STRING" doesn't exist yet.
    let before = search_segments(&snap, "UNIQUE_MARKER_STRING").unwrap();
    assert_eq!(before.total_file_count, 0);

    // Write a modified version of main.rs to the fixture dir (in tempdir).
    let fixture_dir = fixtures_dir();
    let work_dir = tempfile::tempdir().unwrap();
    // Copy all fixtures to work_dir.
    copy_fixtures(work_dir.path());

    // Modify a file in the work_dir.
    let modified_path = work_dir.path().join("src/main.rs");
    let mut content = std::fs::read_to_string(&modified_path).unwrap();
    content.push_str("\n// UNIQUE_MARKER_STRING added by test\n");
    std::fs::write(&modified_path, &content).unwrap();

    // Apply the modification.
    let changes = vec![ChangeEvent {
        path: PathBuf::from("src/main.rs"),
        kind: ChangeKind::Modified,
    }];
    manager.apply_changes(work_dir.path(), &changes).unwrap();

    // Search again — should find the new content.
    let snap = manager.snapshot();
    let after = search_segments(&snap, "UNIQUE_MARKER_STRING").unwrap();
    assert_eq!(after.total_file_count, 1);
    assert_eq!(after.files[0].path, Path::new("src/main.rs"));
}

#[test]
fn test_incremental_delete() {
    let work_dir = tempfile::tempdir().unwrap();
    copy_fixtures(work_dir.path());

    let index_dir = work_dir.path().join(".ferret_index");
    let manager = SegmentManager::new(&index_dir).unwrap();
    let files = load_fixture_files();
    manager.index_files(files).unwrap();

    // Verify utils.py is searchable.
    let snap = manager.snapshot();
    let before = search_segments(&snap, "DataProcessor").unwrap();
    assert_eq!(before.total_file_count, 1);

    // Delete the file.
    std::fs::remove_file(work_dir.path().join("src/utils.py")).unwrap();
    let changes = vec![ChangeEvent {
        path: PathBuf::from("src/utils.py"),
        kind: ChangeKind::Deleted,
    }];
    manager.apply_changes(work_dir.path(), &changes).unwrap();

    // Search again — should no longer find it.
    let snap = manager.snapshot();
    let after = search_segments(&snap, "DataProcessor").unwrap();
    assert_eq!(after.total_file_count, 0);
}
```

**Step 2: Add the `copy_fixtures` helper at the top of the file (after `build_index`)**

```rust
/// Copy fixture files to a target directory.
fn copy_fixtures(target: &Path) {
    let base = fixtures_dir();
    let paths = &[
        "src/main.rs",
        "src/lib.rs",
        "src/utils.py",
        "README.md",
        "data/config.toml",
    ];
    for rel in paths {
        let src = base.join(rel);
        let dst = target.join(rel);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(&src, &dst).unwrap();
    }
}
```

**Step 3: Run tests**

Run: `cargo test -p ferret-indexer-core --test integration -- --nocapture`
Expected: All 10 tests PASS.

**Step 4: Commit**

```bash
git add ferret-core/tests/integration.rs
git commit -m "test: add incremental modify and delete integration tests (HHC-66)"
```

---

### Task 5: CLI integration tests — output format and exit codes

**Files:**
- Create: `ferret-cli/tests/cli_integration.rs`

The CLI dispatches all searches through a daemon (auto-started via `ensure_daemon`). The test flow is:
1. Copy fixtures to tempdir
2. Run `ferret init` in that dir (builds index + starts daemon)
3. Run `ferret search` commands and check stdout/exit code

The binary name is `ferret-indexer-cli` (from `ferret-cli/Cargo.toml` package name).

**Step 1: Write CLI integration tests**

`ferret-cli/tests/cli_integration.rs`:
```rust
//! CLI integration tests for ferret search output format and exit codes.
//!
//! These tests invoke the `ferret-indexer-cli` binary as a subprocess,
//! verifying vimgrep output format, exit codes, and MCP responses.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the fixture repo (shared with ferret-core tests).
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("ferret-core")
        .join("tests")
        .join("fixtures")
        .join("repo")
}

/// Copy fixture files to a target directory.
fn copy_fixtures(target: &Path) {
    let paths = &[
        "src/main.rs",
        "src/lib.rs",
        "src/utils.py",
        "README.md",
        "data/config.toml",
    ];
    let base = fixtures_dir();
    for rel in paths {
        let src = base.join(rel);
        let dst = target.join(rel);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(&src, &dst).unwrap();
    }
}

/// Get path to the built ferret binary.
fn ferret_bin() -> PathBuf {
    // cargo test builds to target/debug/
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_ferret-indexer-cli"));
    path
}

/// Set up a temp repo: copy fixtures, run `ferret init`.
fn setup_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    copy_fixtures(tmp.path());

    let output = Command::new(ferret_bin())
        .args(["--repo", tmp.path().to_str().unwrap(), "init"])
        .output()
        .expect("failed to run ferret init");
    assert!(
        output.status.success(),
        "ferret init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    tmp
}

#[test]
fn test_cli_search_output_format() {
    let tmp = setup_repo();

    let output = Command::new(ferret_bin())
        .args([
            "--repo", tmp.path().to_str().unwrap(),
            "--color", "never",
            "search", "fn main",
        ])
        .output()
        .expect("failed to run ferret search");

    assert!(output.status.success(), "search should exit 0 on match");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Each line should be in vimgrep format: path:line:col:content
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        assert_eq!(
            parts.len(), 4,
            "expected vimgrep format path:line:col:content, got: {line}"
        );
        // parts[1] should be a line number.
        let _line_num: u32 = parts[1].parse()
            .unwrap_or_else(|_| panic!("expected line number, got '{}' in: {line}", parts[1]));
        // parts[2] should be a column number.
        let _col: u32 = parts[2].parse()
            .unwrap_or_else(|_| panic!("expected column number, got '{}' in: {line}", parts[2]));
    }
}

#[test]
fn test_cli_search_exit_codes() {
    let tmp = setup_repo();

    // Exit 0: results found.
    let output = Command::new(ferret_bin())
        .args([
            "--repo", tmp.path().to_str().unwrap(),
            "--color", "never",
            "search", "fn main",
        ])
        .output()
        .expect("failed to run ferret search");
    assert_eq!(output.status.code(), Some(0), "should exit 0 when results found");

    // Exit 1: no results.
    let output = Command::new(ferret_bin())
        .args([
            "--repo", tmp.path().to_str().unwrap(),
            "--color", "never",
            "search", "xyzzy_nothing_matches_this",
        ])
        .output()
        .expect("failed to run ferret search");
    assert_eq!(output.status.code(), Some(1), "should exit 1 when no results");
}

#[test]
fn test_cli_search_no_color() {
    let tmp = setup_repo();

    let output = Command::new(ferret_bin())
        .args([
            "--repo", tmp.path().to_str().unwrap(),
            "--color", "never",
            "search", "fn main",
        ])
        .output()
        .expect("failed to run ferret search");

    let stdout = String::from_utf8_lossy(&output.stdout);
    // No ANSI escape codes should be present.
    assert!(
        !stdout.contains('\x1b'),
        "output should contain no ANSI escape codes with --color=never"
    );
}
```

**Step 2: Run tests**

Run: `cargo test -p ferret-indexer-cli --test cli_integration -- --nocapture`
Expected: All 3 tests PASS.

**Step 3: Commit**

```bash
git add ferret-cli/tests/cli_integration.rs
git commit -m "test: add CLI integration tests for output format and exit codes (HHC-66)"
```

---

### Task 6: CLI integration test — MCP tool response

**Files:**
- Modify: `ferret-cli/tests/cli_integration.rs`

The MCP server communicates over stdio with JSON-RPC. We spawn `ferret mcp`, send an `initialize` request, then a `tools/call` for `search_code`, and verify the response.

**Step 1: Add MCP test**

```rust
#[test]
fn test_mcp_search_response() {
    let tmp = setup_repo();

    let mut child = Command::new(ferret_bin())
        .args(["--repo", tmp.path().to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start ferret mcp");

    let stdin = child.stdin.as_mut().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);

    // Helper: send JSON-RPC and read response.
    use std::io::{BufRead, Write};

    let send_and_recv = |stdin: &mut std::process::ChildStdin,
                         reader: &mut std::io::BufReader<std::process::ChildStdout>,
                         msg: &serde_json::Value| -> serde_json::Value {
        let body = serde_json::to_string(msg).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        stdin.write_all(header.as_bytes()).unwrap();
        stdin.write_all(body.as_bytes()).unwrap();
        stdin.flush().unwrap();

        // Read response header.
        let mut header_line = String::new();
        reader.read_line(&mut header_line).unwrap();
        let content_len: usize = header_line
            .trim()
            .strip_prefix("Content-Length: ")
            .unwrap()
            .parse()
            .unwrap();
        // Read blank line.
        let mut blank = String::new();
        reader.read_line(&mut blank).unwrap();
        // Read body.
        let mut body_buf = vec![0u8; content_len];
        std::io::Read::read_exact(reader, &mut body_buf).unwrap();
        serde_json::from_slice(&body_buf).unwrap()
    };

    // 1. Initialize.
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0.1" }
        }
    });
    let init_resp = send_and_recv(stdin, &mut reader, &init_req);
    assert!(init_resp.get("result").is_some(), "initialize should return result");

    // Send initialized notification.
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let body = serde_json::to_string(&initialized).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).unwrap();
    stdin.write_all(body.as_bytes()).unwrap();
    stdin.flush().unwrap();

    // 2. Call search_code tool.
    let search_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "search_code",
            "arguments": {
                "query": "fn main",
                "max_results": 10
            }
        }
    });
    let search_resp = send_and_recv(stdin, &mut reader, &search_req);

    let result = search_resp.get("result").expect("search should return result");
    let content = result.get("content").expect("result should have content");
    let content_arr = content.as_array().expect("content should be array");
    assert!(!content_arr.is_empty(), "content should not be empty");

    let text = content_arr[0].get("text").expect("content[0] should have text");
    let text_str = text.as_str().unwrap();
    assert!(
        text_str.contains("main.rs"),
        "MCP search response should mention main.rs, got: {text_str}"
    );

    // Clean up.
    drop(stdin);
    let _ = child.wait();
}
```

Add to the top of the file:
```rust
use std::io::{BufRead, Read as _, Write as _};
```

**Step 2: Run tests**

Run: `cargo test -p ferret-indexer-cli --test cli_integration -- test_mcp_search_response --nocapture`
Expected: PASS.

**Step 3: Commit**

```bash
git add ferret-cli/tests/cli_integration.rs
git commit -m "test: add MCP tool response integration test (HHC-66)"
```

---

### Task 7: Final verification and lint

**Step 1: Run all tests**

Run: `cargo test --workspace`
Expected: All tests PASS.

**Step 2: Run lint**

Run: `make lint-all`
Expected: No warnings or errors.

**Step 3: Fix any issues and commit**

If any lint or test issues, fix them and commit.

**Step 4: Final commit (if needed)**

```bash
git add -A
git commit -m "test: fix lint issues in integration tests (HHC-66)"
```
