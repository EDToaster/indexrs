# CWD-Relative Path Output Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make `indexrs-cli` output file paths relative to the caller's CWD, matching `rg`/`fd` conventions, so paths resolve correctly for downstream consumers (fzf preview, Helix `:open`).

**Architecture:** The CLI client sends its CWD to the daemon as part of each Search/Files request. The daemon constructs a `PathRewriter` from `(repo_root, cwd)` and rewrites each repo-root-relative path before ANSI formatting. This keeps the daemon as the single point of path transformation, operating on structured data before color codes are applied.

**Tech Stack:** Pure Rust `std::path`, no new crate dependencies. Path diffing is a ~20-line function.

**Behavior (matching rg/fd):**

| CWD location | Example CWD | Stored path | Output path |
|---|---|---|---|
| Is repo root | `/repo` | `src/main.rs` | `src/main.rs` |
| Inside repo | `/repo/src` | `src/main.rs` | `main.rs` |
| Inside repo (cross-dir) | `/repo/src` | `README.md` | `../README.md` |
| Outside repo | `/tmp` | `src/main.rs` | `/repo/src/main.rs` |

---

### Task 1: Create `paths.rs` — PathRewriter with tests

**Files:**
- Create: `indexrs-cli/src/paths.rs`

**Step 1: Write the failing tests**

Create `indexrs-cli/src/paths.rs` with tests only (struct/impl stubs that don't compile yet):

```rust
use std::path::{Path, PathBuf};

/// Rewrites repo-root-relative paths to be relative to the caller's CWD.
///
/// Constructed from a `(repo_root, cwd)` pair. Three modes:
/// - **Identity**: CWD == repo root → paths unchanged.
/// - **RelativeTo**: CWD inside repo → paths relative to CWD (may use `../`).
/// - **Absolute**: CWD outside repo → absolute paths.
pub struct PathRewriter {
    transform: PathTransform,
}

enum PathTransform {
    /// No rewriting (CWD == repo root, or no CWD provided).
    Identity,
    /// CWD is inside the repo. `cwd_from_root` is CWD relative to repo root.
    RelativeTo { cwd_from_root: PathBuf },
    /// CWD is outside the repo. Prepend repo_root to make absolute paths.
    Absolute { repo_root: PathBuf },
}

impl PathRewriter {
    /// Create a rewriter. Compares `cwd` against `repo_root` to determine mode.
    pub fn new(repo_root: &Path, cwd: &Path) -> Self {
        todo!()
    }

    /// Identity rewriter that returns paths unchanged.
    pub fn identity() -> Self {
        Self {
            transform: PathTransform::Identity,
        }
    }

    /// Rewrite a repo-root-relative path for display.
    pub fn rewrite<'a>(&self, repo_relative_path: &'a str) -> String {
        todo!()
    }
}

/// Compute the relative path from `base` to `target`.
///
/// Both paths must be relative (no leading `/`). Returns a path with `../`
/// components as needed.
///
/// ```text
/// diff_paths("src/main.rs", "src")  → "main.rs"
/// diff_paths("README.md",   "src")  → "../README.md"
/// diff_paths("a/b/c.rs",    "a/x")  → "../b/c.rs"
/// ```
fn diff_relative_paths(target: &Path, base: &Path) -> PathBuf {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- diff_relative_paths --

    #[test]
    fn test_diff_same_dir() {
        let result = diff_relative_paths(Path::new("src/main.rs"), Path::new("src"));
        assert_eq!(result, PathBuf::from("main.rs"));
    }

    #[test]
    fn test_diff_parent_dir() {
        let result = diff_relative_paths(Path::new("README.md"), Path::new("src"));
        assert_eq!(result, PathBuf::from("../README.md"));
    }

    #[test]
    fn test_diff_cross_dir() {
        let result = diff_relative_paths(Path::new("src/core/mod.rs"), Path::new("src/cli"));
        assert_eq!(result, PathBuf::from("../core/mod.rs"));
    }

    #[test]
    fn test_diff_deeply_nested() {
        let result = diff_relative_paths(Path::new("a/b/c.rs"), Path::new("x/y/z"));
        assert_eq!(result, PathBuf::from("../../../a/b/c.rs"));
    }

    #[test]
    fn test_diff_empty_base() {
        // base is repo root (empty) — path unchanged
        let result = diff_relative_paths(Path::new("src/main.rs"), Path::new(""));
        assert_eq!(result, PathBuf::from("src/main.rs"));
    }

    // -- PathRewriter::new mode selection --

    #[test]
    fn test_rewriter_identity_when_cwd_is_repo_root() {
        let rw = PathRewriter::new(Path::new("/repo"), Path::new("/repo"));
        assert_eq!(rw.rewrite("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_rewriter_relative_when_cwd_inside_repo() {
        let rw = PathRewriter::new(Path::new("/repo"), Path::new("/repo/src"));
        assert_eq!(rw.rewrite("src/main.rs"), "main.rs");
        assert_eq!(rw.rewrite("README.md"), "../README.md");
    }

    #[test]
    fn test_rewriter_relative_nested_subdir() {
        let rw = PathRewriter::new(Path::new("/repo"), Path::new("/repo/src/cli"));
        assert_eq!(rw.rewrite("src/cli/main.rs"), "main.rs");
        assert_eq!(rw.rewrite("src/core/mod.rs"), "../core/mod.rs");
        assert_eq!(rw.rewrite("README.md"), "../../README.md");
    }

    #[test]
    fn test_rewriter_absolute_when_cwd_outside_repo() {
        let rw = PathRewriter::new(Path::new("/repo"), Path::new("/tmp"));
        assert_eq!(rw.rewrite("src/main.rs"), "/repo/src/main.rs");
    }

    #[test]
    fn test_rewriter_identity_passthrough() {
        let rw = PathRewriter::identity();
        assert_eq!(rw.rewrite("src/main.rs"), "src/main.rs");
    }
}
```

**Step 2: Register the module and run tests to verify they fail**

Add `mod paths;` and `pub use paths::PathRewriter;` to `indexrs-cli/src/main.rs` (after the existing `mod` lines, before `use` lines).

Run: `cargo test -p indexrs-cli -- paths -v`
Expected: FAIL — `todo!()` panics.

**Step 3: Implement `diff_relative_paths`**

Replace the `todo!()` in `diff_relative_paths`:

```rust
fn diff_relative_paths(target: &Path, base: &Path) -> PathBuf {
    let mut target_components = target.components().peekable();
    let mut base_components = base.components().peekable();

    // Skip common prefix.
    while let (Some(t), Some(b)) = (target_components.peek(), base_components.peek()) {
        if t != b {
            break;
        }
        target_components.next();
        base_components.next();
    }

    // One `..` per remaining base component.
    let mut result = PathBuf::new();
    for _ in base_components {
        result.push("..");
    }

    // Append remaining target components.
    for component in target_components {
        result.push(component);
    }

    result
}
```

**Step 4: Implement `PathRewriter::new` and `rewrite`**

Replace the `todo!()`s:

```rust
impl PathRewriter {
    pub fn new(repo_root: &Path, cwd: &Path) -> Self {
        if cwd == repo_root {
            return Self::identity();
        }
        match cwd.strip_prefix(repo_root) {
            Ok(rel) => Self {
                transform: PathTransform::RelativeTo {
                    cwd_from_root: rel.to_path_buf(),
                },
            },
            Err(_) => Self {
                transform: PathTransform::Absolute {
                    repo_root: repo_root.to_path_buf(),
                },
            },
        }
    }

    pub fn identity() -> Self {
        Self {
            transform: PathTransform::Identity,
        }
    }

    pub fn rewrite(&self, repo_relative_path: &str) -> String {
        match &self.transform {
            PathTransform::Identity => repo_relative_path.to_string(),
            PathTransform::RelativeTo { cwd_from_root } => {
                diff_relative_paths(Path::new(repo_relative_path), cwd_from_root)
                    .to_string_lossy()
                    .into_owned()
            }
            PathTransform::Absolute { repo_root } => {
                repo_root.join(repo_relative_path)
                    .to_string_lossy()
                    .into_owned()
            }
        }
    }
}
```

**Step 5: Run tests to verify they pass**

Run: `cargo test -p indexrs-cli -- paths -v`
Expected: all 9 tests PASS.

**Step 6: Commit**

```bash
git add indexrs-cli/src/paths.rs indexrs-cli/src/main.rs
git commit -m "feat: add PathRewriter for CWD-relative path output"
```

---

### Task 2: Thread PathRewriter through search output

**Files:**
- Modify: `indexrs-cli/src/search_cmd.rs` (lines 59-222 — both `run_search` and `run_search_streaming`)

**Step 1: Update function signatures**

Add `use crate::paths::PathRewriter;` at the top of `search_cmd.rs`.

Add `path_rewriter: &PathRewriter` parameter to both functions:

```rust
pub fn run_search<W: std::io::Write>(
    snapshot: &SegmentList,
    opts: &SearchCmdOptions,
    color: &ColorConfig,
    path_rewriter: &PathRewriter,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
```

```rust
pub fn run_search_streaming<W: std::io::Write>(
    snapshot: &SegmentList,
    opts: &SearchCmdOptions,
    color: &ColorConfig,
    path_rewriter: &PathRewriter,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
```

**Step 2: Apply path rewriting before formatting**

In `run_search`, change line 84:
```rust
// Before:
let path_str = file_match.path.to_string_lossy();
// After:
let path_str = path_rewriter.rewrite(&file_match.path.to_string_lossy());
```

In `run_search_streaming`, change line 166:
```rust
// Before:
let path_str = file_match.path.to_string_lossy();
// After:
let path_str = path_rewriter.rewrite(&file_match.path.to_string_lossy());
```

**Step 3: Fix format_search_line call sites**

Both call sites pass `&path_str` to `format_search_line`. Since `path_str` is now a `String` (not `Cow<str>`), the reference `&path_str` still works as `&str`. No change needed at the call sites.

**Step 4: Fix all callers — pass `PathRewriter::identity()` in existing call sites**

In `daemon.rs`, `handle_search_request` (around line 265):
```rust
// Before:
search_cmd::run_search_streaming(&snapshot, opts, &color, &mut writer)
// After:
search_cmd::run_search_streaming(&snapshot, opts, &color, path_rewriter, &mut writer)
```

(The `path_rewriter` parameter will be added in Task 4. For now, to keep things compiling, temporarily use `&PathRewriter::identity()`. We'll update it in Task 4.)

**Step 5: Update existing search tests**

In `search_cmd.rs` tests, update all 4 test functions to pass `&PathRewriter::identity()`:

```rust
// Example for test_search_vimgrep_format:
let exit = {
    let mut writer = StreamingWriter::new(&mut buf);
    run_search(&snapshot, &opts, &color, &PathRewriter::identity(), &mut writer).unwrap()
};
```

Do the same for `test_search_no_results`, `test_search_streaming_vimgrep_format`, and `test_search_streaming_no_results`.

**Step 6: Add a test for CWD-relative search output**

Add to `search_cmd.rs` tests:

```rust
#[test]
fn test_search_rewrites_paths_to_cwd_relative() {
    let dir = tempfile::tempdir().unwrap();
    let manager = build_test_index(dir.path());
    let snapshot = manager.snapshot();

    let mut buf = Vec::new();
    let color = ColorConfig::new(false);
    // Simulate CWD = repo/src (inside repo)
    let rewriter = PathRewriter::new(Path::new("/repo"), Path::new("/repo/src"));

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
        run_search(&snapshot, &opts, &color, &rewriter, &mut writer).unwrap()
    };
    let output = String::from_utf8(buf).unwrap();

    // "src/main.rs" should become "main.rs" with CWD = /repo/src
    assert!(output.contains("main.rs:2:"), "expected rewritten path, got: {output}");
    assert!(!output.contains("src/main.rs:"), "path should not have src/ prefix");
    assert!(matches!(exit, ExitCode::Success));
}
```

**Step 7: Run tests**

Run: `cargo test -p indexrs-cli -- search -v`
Expected: all tests PASS (including the new rewrite test).

**Step 8: Commit**

```bash
git add indexrs-cli/src/search_cmd.rs indexrs-cli/src/daemon.rs
git commit -m "feat: thread PathRewriter through search output"
```

---

### Task 3: Thread PathRewriter through files output

**Files:**
- Modify: `indexrs-cli/src/files.rs` (lines 92-114, `run_files`)

**Step 1: Update function signature**

Add `use crate::paths::PathRewriter;` at the top of `files.rs`.

Add `path_rewriter: &PathRewriter` parameter:

```rust
pub fn run_files<W: std::io::Write>(
    snapshot: &SegmentList,
    filter: &FilesFilter,
    color: &ColorConfig,
    path_rewriter: &PathRewriter,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
```

**Step 2: Apply path rewriting before formatting**

Change line 105:
```rust
// Before:
let line = color.format_file_path(&file.path);
// After:
let display_path = path_rewriter.rewrite(&file.path);
let line = color.format_file_path(&display_path);
```

**Step 3: Fix caller in daemon.rs**

In `daemon.rs`, `handle_files_request` (around line 307):
```rust
// Before:
files::run_files(&snapshot, &filter, &color, &mut writer)
// After:
files::run_files(&snapshot, &filter, &color, path_rewriter, &mut writer)
```

(Same as search — temporarily use `&PathRewriter::identity()` until Task 4.)

**Step 4: Update existing files tests**

In `files.rs` tests, the `run_files` function isn't directly called in tests (tests use `collect_files`). No test changes needed for existing tests.

But add a new integration test:

```rust
#[test]
fn test_run_files_rewrites_paths_to_cwd_relative() {
    let dir = tempfile::tempdir().unwrap();
    let manager = build_test_index(dir.path());
    let snapshot = manager.snapshot();

    let mut buf = Vec::new();
    let color = ColorConfig::new(false);
    // Simulate CWD = repo/src (inside repo)
    let rewriter = PathRewriter::new(Path::new("/repo"), Path::new("/repo/src"));

    let exit = {
        let mut writer = StreamingWriter::new(&mut buf);
        run_files(&snapshot, &FilesFilter::default(), &color, &rewriter, &mut writer).unwrap()
    };
    let output = String::from_utf8(buf).unwrap();

    // "src/main.rs" should become "main.rs"
    assert!(output.contains("main.rs\n"), "expected rewritten src/main.rs → main.rs, got: {output}");
    // "README.md" should become "../README.md"
    assert!(output.contains("../README.md\n"), "expected ../README.md, got: {output}");
}
```

**Step 5: Run tests**

Run: `cargo test -p indexrs-cli -v`
Expected: all tests PASS.

**Step 6: Commit**

```bash
git add indexrs-cli/src/files.rs indexrs-cli/src/daemon.rs
git commit -m "feat: thread PathRewriter through files output"
```

---

### Task 4: Add `cwd` to daemon protocol and wire everything together

**Files:**
- Modify: `indexrs-cli/src/daemon.rs` (DaemonRequest, handle_search_request, handle_files_request, handle_connection)
- Modify: `indexrs-cli/src/main.rs` (Search and Files command branches)

**Step 1: Add `cwd` field to DaemonRequest variants**

In `daemon.rs`, update `DaemonRequest`:

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
        cwd: Option<String>,  // NEW — caller's working directory
    },
    Files {
        language: Option<String>,
        path_glob: Option<String>,
        sort: String,
        limit: Option<usize>,
        color: bool,
        cwd: Option<String>,  // NEW — caller's working directory
    },
    // ... Ping, Shutdown, Reindex unchanged
}
```

**Step 2: Update `handle_search_request` signature**

```rust
fn handle_search_request(
    manager: &SegmentManager,
    opts: &SearchCmdOptions,
    color: bool,
    path_rewriter: &PathRewriter,
) -> Result<(Vec<String>, Duration), String> {
```

Replace the call to `run_search_streaming` (around line 265):
```rust
search_cmd::run_search_streaming(&snapshot, opts, &color, path_rewriter, &mut writer)
```

**Step 3: Update `handle_files_request` signature**

```rust
fn handle_files_request(
    manager: &SegmentManager,
    language: Option<String>,
    path_glob: Option<String>,
    sort: String,
    limit: Option<usize>,
    color: bool,
    path_rewriter: &PathRewriter,
) -> Result<(Vec<String>, Duration), String> {
```

Replace the call to `run_files` (around line 307):
```rust
files::run_files(&snapshot, &filter, &color, path_rewriter, &mut writer)
```

**Step 4: Construct PathRewriter in `handle_connection`**

In the `DaemonRequest::Search { .. }` match arm (around line 363), add `cwd` to the destructure and build the rewriter:

```rust
DaemonRequest::Search {
    query, regex, case_sensitive, ignore_case,
    limit, context_lines, language, path_glob, color, cwd,
} => {
    let path_rewriter = match cwd {
        Some(ref cwd_str) => PathRewriter::new(repo_root, Path::new(cwd_str)),
        None => PathRewriter::identity(),
    };
    // ... existing code ...
    match handle_search_request(manager, &opts, color, &path_rewriter) {
```

Do the same in the `DaemonRequest::Files { .. }` arm (around line 425):

```rust
DaemonRequest::Files {
    language, path_glob, sort, limit, color, cwd,
} => {
    let path_rewriter = match cwd {
        Some(ref cwd_str) => PathRewriter::new(repo_root, Path::new(cwd_str)),
        None => PathRewriter::identity(),
    };
    // ... existing code ...
    match handle_files_request(manager, language, path_glob, sort, limit, color, &path_rewriter) {
```

**Step 5: Send CWD from client in `main.rs`**

In `main.rs`, at the top of the `run` function (line 50), compute CWD once:

```rust
async fn run(cli: Cli, color: &ColorConfig) -> Result<ExitCode, indexrs_core::IndexError> {
    // Resolve CWD for path rewriting (best-effort; None if unavailable).
    let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned());

    match cli.command {
```

Then in the `Command::Search` branch (around line 96), add `cwd` to the request:

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
    cwd: cwd.clone(),
};
```

And in the `Command::Files` branch (around line 132):

```rust
let request = daemon::DaemonRequest::Files {
    language,
    path_glob: path,
    sort: sort_str.to_string(),
    limit,
    color: color.enabled,
    cwd: cwd.clone(),
};
```

**Step 6: Run all tests**

Run: `cargo test -p indexrs-cli -v`
Expected: all tests PASS.

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings.

**Step 7: Commit**

```bash
git add indexrs-cli/src/daemon.rs indexrs-cli/src/main.rs
git commit -m "feat: send CWD to daemon for CWD-relative path output"
```

---

### Task 5: Verify formatting and run full CI checks

**Files:**
- No new files.

**Step 1: Format check**

Run: `cargo fmt --all -- --check`
Fix any formatting issues with: `cargo fmt --all`

**Step 2: Clippy**

Run: `cargo clippy --workspace -- -D warnings`
Fix any warnings.

**Step 3: Full test suite**

Run: `cargo test --workspace`
Expected: all tests PASS.

**Step 4: Manual smoke test**

From the repo root:
```bash
cargo run -p indexrs-cli --release -- search "fn main"
```
Expected: paths like `src/main.rs:1:1:fn main() {`

From a subdirectory:
```bash
cd indexrs-cli/src && cargo run -p indexrs-cli --release -- search "fn main"
```
Expected: paths like `../../indexrs-core/src/lib.rs` (relative to CWD, with `../` as needed).

**Step 5: Commit any fixes**

```bash
git add -A
git commit -m "chore: fix formatting and clippy for CWD-relative paths"
```
