# MCP Code Review Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix all bugs and design issues identified in the MCP code review without regressions.

**Architecture:** Targeted fixes across `ferret-mcp` (server, errors, formatter, resources) and `ferret-indexer-daemon` (wire). Consolidates duplicated `format_size` into `formatter.rs` as the single source of truth. Adds a `daemon_dispatch_error` helper for daemon failures. Fixes a potential panic, a grammar bug, a misleading tool description, and a missing payload-size guard.

**Tech Stack:** Rust, rmcp, tokio, serde, schemars

---

### Task 1: Add `daemon_dispatch_error` to `errors.rs` (Bug #3)

Daemon failures (connection errors, timeouts) are currently reported via `errors::invalid_query`, which implies a syntax problem. Add a dedicated error helper.

**Files:**
- Modify: `ferret-mcp/src/errors.rs`

**Step 1: Write the failing test**

Add to `ferret-mcp/src/errors.rs` at the bottom of `mod tests`:

```rust
// ---- daemon_dispatch_error ----

#[test]
fn test_daemon_dispatch_error() {
    let result = daemon_dispatch_error("connection refused");
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("connection refused"));
    assert!(!text.contains("Invalid query"));
}

#[test]
fn test_daemon_dispatch_error_timeout() {
    let result = daemon_dispatch_error("daemon did not start within timeout");
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("timeout"));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-mcp -- test_daemon_dispatch_error`
Expected: FAIL — `daemon_dispatch_error` not found

**Step 3: Write minimal implementation**

Add to `ferret-mcp/src/errors.rs` after the `index_building` function (before `no_results`):

```rust
/// Create an error response for a daemon communication failure.
///
/// Used when the daemon is unreachable, times out, or returns a
/// protocol-level error — distinct from a query syntax error.
pub fn daemon_dispatch_error(msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(format!(
        "Error: Daemon request failed: {msg}"
    ))])
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-mcp -- test_daemon_dispatch_error`
Expected: PASS (both tests)

**Step 5: Update callers in `server.rs`**

Replace the three `errors::invalid_query(&e)` calls at daemon dispatch sites:

- Line 264: `return Ok(errors::invalid_query(&e));` → `return Ok(errors::daemon_dispatch_error(&e));`
- Line 339: `return Ok(errors::invalid_query(&e));` → `return Ok(errors::daemon_dispatch_error(&e));`

Do NOT change line 615 or 638 — those are actual query/search errors in the direct fallback path and `invalid_query` is correct there.

**Step 6: Run full test suite**

Run: `cargo test -p ferret-mcp`
Expected: All tests pass

**Step 7: Commit**

```bash
git add ferret-mcp/src/errors.rs ferret-mcp/src/server.rs
git commit -m "fix(mcp): use daemon_dispatch_error for daemon failures instead of invalid_query"
```

---

### Task 2: Fix `saturating_sub` in per-segment live count (Bug #2)

`server.rs:521` uses bare subtraction for the per-segment live file count, which could panic on underflow in debug builds if tombstones exceed entry count (e.g., index corruption).

**Files:**
- Modify: `ferret-mcp/src/server.rs`

**Step 1: Fix the subtraction**

At `ferret-mcp/src/server.rs` line 521, change:

```rust
let live = entry_count as u64 - tombstones.len() as u64;
```

to:

```rust
let live = (entry_count as u64).saturating_sub(tombstones.len() as u64);
```

**Step 2: Run tests to verify no regression**

Run: `cargo test -p ferret-mcp -- test_index_status`
Expected: All index_status tests pass

**Step 3: Commit**

```bash
git add ferret-mcp/src/server.rs
git commit -m "fix(mcp): use saturating_sub for per-segment live file count"
```

---

### Task 3: Fix `get_file` tool description (Bug — misleading param name)

The `get_file` tool description says `start_line/max_lines` but the actual parameter is `end_line`.

**Files:**
- Modify: `ferret-mcp/src/server.rs`

**Step 1: Fix the description**

At `ferret-mcp/src/server.rs` line 357, change the tool description from:

```
"Read file contents from the index. Returns the file with line numbers. Supports reading a range of lines (start_line/max_lines) to avoid large payloads. Note: contents reflect the last index time, so prefer cat/head/tail for reading files directly when freshness matters."
```

to:

```
"Read file contents from the index. Returns the file with line numbers. Supports reading a range of lines (start_line/end_line) to avoid large payloads. Max 500 lines per request. Note: contents reflect the last index time, so prefer cat/head/tail for reading files directly when freshness matters."
```

**Step 2: Run tests to verify no regression**

Run: `cargo test -p ferret-mcp -- test_get_file`
Expected: All get_file tests pass

**Step 3: Commit**

```bash
git add ferret-mcp/src/server.rs
git commit -m "fix(mcp): correct get_file tool description (max_lines -> end_line)"
```

---

### Task 4: Fix "1 hours" grammar in `format_duration_approx` (Bug #4)

`formatter.rs:469` — `format_duration_approx(3600)` returns `"1 hours"` instead of `"1 hour"`.

**Files:**
- Modify: `ferret-mcp/src/formatter.rs`

**Step 1: Update the test assertion first**

At `ferret-mcp/src/formatter.rs` line 1020, change:

```rust
assert_eq!(format_duration_approx(3600), "1 hours");
```

to:

```rust
assert_eq!(format_duration_approx(3600), "1 hour");
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-mcp -- test_format_duration_hours`
Expected: FAIL — `"1 hours" != "1 hour"`

**Step 3: Fix the implementation**

At `ferret-mcp/src/formatter.rs` lines 469-483, replace the entire function with:

```rust
fn format_duration_approx(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} seconds")
    } else if secs < 3600 {
        format!("{} minutes", secs / 60)
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins == 0 {
            if hours == 1 {
                "1 hour".to_string()
            } else {
                format!("{hours} hours")
            }
        } else {
            format!("{hours}h {mins}m")
        }
    }
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-mcp -- test_format_duration`
Expected: All pass

**Step 5: Commit**

```bash
git add ferret-mcp/src/formatter.rs
git commit -m "fix(mcp): correct '1 hours' grammar in format_duration_approx"
```

---

### Task 5: Add `MAX_STRING_PAYLOAD` guard to `encode_line_frame` (Bug #5)

The sync `encode_line_frame` in `wire.rs` doesn't enforce the 64 MB limit that the async `write_string_frame` does.

**Files:**
- Modify: `ferret-indexer-daemon/src/wire.rs`

**Step 1: Write the failing test**

Add to `ferret-indexer-daemon/src/wire.rs` at the bottom of `mod tests`:

```rust
#[test]
fn test_encode_line_frame_rejects_oversized_payload() {
    // Create a string just over the 64 MB limit
    let huge = "x".repeat(MAX_STRING_PAYLOAD as usize + 1);
    let result = std::panic::catch_unwind(|| encode_line_frame(&huge));
    // After the fix, encode_line_frame returns a Result, so this test will change.
    // For now, just verify the function exists and the constant is accessible.
    assert!(result.is_err() || true); // placeholder
}
```

Actually, since `encode_line_frame` currently returns `Vec<u8>` (not `Result`), the cleanest fix is to add a `debug_assert!` or change the return type. Given the function is only called internally for daemon streaming, let's add a panic guard via `assert!` — matching how the rest of the codebase treats invariant violations.

**Step 1 (revised): Write the failing test**

Add to `ferret-indexer-daemon/src/wire.rs` at the bottom of `mod tests`:

```rust
#[test]
#[should_panic(expected = "payload too large")]
fn test_encode_line_frame_rejects_oversized() {
    let huge = "x".repeat(MAX_STRING_PAYLOAD as usize + 1);
    encode_line_frame(&huge);
}

#[test]
fn test_encode_line_frame_accepts_max_size() {
    let max = "x".repeat(MAX_STRING_PAYLOAD as usize);
    let frame = encode_line_frame(&max);
    assert_eq!(frame.len(), 5 + MAX_STRING_PAYLOAD as usize);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-daemon -- test_encode_line_frame`
Expected: `test_encode_line_frame_rejects_oversized` FAILS (no panic occurs)

**Step 3: Fix the implementation**

At `ferret-indexer-daemon/src/wire.rs` line 54, add an assertion at the start of `encode_line_frame`:

```rust
pub fn encode_line_frame(content: &str) -> Vec<u8> {
    let payload = content.as_bytes();
    assert!(
        payload.len() <= MAX_STRING_PAYLOAD as usize,
        "string payload too large to encode: {} bytes (max {MAX_STRING_PAYLOAD})",
        payload.len()
    );
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(TAG_LINE);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-indexer-daemon -- test_encode_line_frame`
Expected: Both tests pass

**Step 5: Commit**

```bash
git add ferret-indexer-daemon/src/wire.rs
git commit -m "fix(daemon): add MAX_STRING_PAYLOAD guard to encode_line_frame"
```

---

### Task 6: Consolidate duplicated `format_size` functions (Design #6)

There are four nearly identical byte-size formatters. Consolidate to one public function in `formatter.rs` and one thin `format_entry_size` wrapper for `u32`.

**Files:**
- Modify: `ferret-mcp/src/formatter.rs` — make `format_size` public, remove `format_entry_size`
- Modify: `ferret-mcp/src/server.rs` — remove private `format_size`, import from `formatter`
- Modify: `ferret-mcp/src/resources.rs` — remove private `format_bytes`, import from `formatter`

**Step 1: Make `format_size` public in `formatter.rs`**

At `ferret-mcp/src/formatter.rs` line 445, change:

```rust
fn format_size(bytes: u64) -> String {
```

to:

```rust
pub fn format_size(bytes: u64) -> String {
```

**Step 2: Replace `format_entry_size` calls with `format_size`**

At `ferret-mcp/src/formatter.rs` line 266, change:

```rust
let size_str = format_entry_size(entry.size_bytes);
```

to:

```rust
let size_str = format_size(entry.size_bytes as u64);
```

Then delete the `format_entry_size` function entirely (lines 458-466).

**Step 3: Update `format_entry_size` tests to test `format_size` with equivalent u64 values**

In `ferret-mcp/src/formatter.rs` tests, delete the `test_format_entry_size_*` tests (lines 983-1003) since `format_size` already has identical coverage via `test_format_size_*` tests. The `1_048_575` edge case (`"1024.0 KB"`) is already effectively tested.

**Step 4: Remove `format_size` from `server.rs`**

Delete the entire `format_size` function from `ferret-mcp/src/server.rs` (lines 884-894).

Add an import at the top of `server.rs`: the file already imports `crate::formatter::{self, FileListEntry}`. Change to:

```rust
use crate::formatter::{self, FileListEntry, format_size};
```

Wait — `format_size` is called unqualified in `server.rs` at lines 511, 529, and 553. After removing the local definition and adding the import, these calls will resolve to `formatter::format_size`. But the import `use crate::formatter::{self, FileListEntry}` can be extended.

Actually, the cleaner approach: since `server.rs` already imports `crate::formatter` as a module, just prefix the calls. Change the three call sites:

- Line 511: `format_size(total_disk_bytes)` → `formatter::format_size(total_disk_bytes)`
- Line 529: `format_size(disk)` → `formatter::format_size(disk)`
- Line 553: `format_size(total_disk_bytes)` → `formatter::format_size(total_disk_bytes)`

Delete the local `format_size` function (lines 883-894) and its test (lines 1100-1107).

**Step 5: Remove `format_bytes` from `resources.rs`**

Delete the entire `format_bytes` function from `ferret-mcp/src/resources.rs` (lines 333-343) and its test (`test_format_bytes`, lines 428-438).

Add an import: `use crate::formatter::format_size;`

Update the two call sites:

- Line 177: `format_bytes(total_size_bytes)` → `format_size(total_size_bytes)`
- Line 285: `format_bytes(total_size_bytes)` → `format_size(total_size_bytes)`

**Step 6: Run full test suite**

Run: `cargo test -p ferret-mcp`
Expected: All tests pass. Existing `test_format_size_*` tests in `formatter.rs` cover the unified function.

**Step 7: Run clippy**

Run: `cargo clippy -p ferret-mcp -- -D warnings`
Expected: Clean

**Step 8: Commit**

```bash
git add ferret-mcp/src/formatter.rs ferret-mcp/src/server.rs ferret-mcp/src/resources.rs
git commit -m "refactor(mcp): consolidate four duplicated format_size functions into one"
```

---

### Task 7: Remove double language validation in `search_files` (Design #7)

`search_files` validates the language at lines 299-312 before daemon dispatch, then `search_files_direct` validates it again at lines 680-693.

**Files:**
- Modify: `ferret-mcp/src/server.rs`

**Step 1: Remove redundant validation from `search_files_direct`**

The `search_files_direct` method receives `language: Option<&str>` which has already been validated. Change the language filter parsing at lines 679-693:

Replace:

```rust
        // Parse language filter
        let language_filter = match language {
            Some(lang_str) => match ferret_indexer_core::match_language(lang_str) {
                Ok(lang) => Some(lang),
                Err(_) => {
                    return Ok(errors::invalid_parameter(
                        "language",
                        &format!(
                            "Unknown language: \"{lang_str}\". Examples: rust, python, typescript."
                        ),
                    ));
                }
            },
            None => None,
        };
```

with:

```rust
        // Language was already validated by the caller.
        let language_filter = language.map(|lang_str| {
            ferret_indexer_core::match_language(lang_str)
                .expect("language already validated by search_files")
        });
```

**Step 2: Run tests to verify no regression**

Run: `cargo test -p ferret-mcp -- test_search_files`
Expected: All pass (including `test_search_files_invalid_language` which is caught at the caller before reaching `search_files_direct`)

**Step 3: Commit**

```bash
git add ferret-mcp/src/server.rs
git commit -m "refactor(mcp): remove redundant language validation in search_files_direct"
```

---

### Task 8: Add `#[schemars(description)]` to `SearchFilesParams` (Design #14)

Unlike `SearchCodeParams`, `SearchFilesParams` fields lack `#[schemars(description)]` annotations, so MCP clients see less helpful tool schemas.

**Files:**
- Modify: `ferret-mcp/src/server.rs`

**Step 1: Add descriptions**

Replace lines 91-113 with:

```rust
/// Parameters for the `search_files` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
pub struct SearchFilesParams {
    /// File name or path pattern. Supports glob patterns and substring matching.
    #[schemars(
        description = "File name or path pattern to search for. Supports glob patterns (*.rs, src/**/*.ts) and substring matching."
    )]
    pub query: String,

    /// Filter by programming language (e.g. "rust", "python", "typescript").
    #[serde(default)]
    #[schemars(
        description = "Filter by programming language. Examples: 'rust', 'python', 'typescript'."
    )]
    pub language: Option<String>,

    /// Filter to a specific indexed repository.
    #[serde(default)]
    #[schemars(description = "Filter to a specific indexed repository by name or path.")]
    pub repo: Option<String>,

    /// Maximum number of files to return. Default: 30. Max: 200.
    #[serde(default)]
    #[schemars(description = "Maximum number of files to return. Default: 30. Max: 200.")]
    pub max_results: Option<usize>,

    /// Skip this many results for pagination. Default: 0.
    #[serde(default)]
    #[schemars(description = "Skip this many results for pagination. Default: 0.")]
    pub offset: Option<usize>,
}
```

**Step 2: Run tests**

Run: `cargo test -p ferret-mcp -- test_search_files`
Expected: All pass (schemars annotations don't affect runtime behavior)

**Step 3: Commit**

```bash
git add ferret-mcp/src/server.rs
git commit -m "fix(mcp): add schemars descriptions to SearchFilesParams for better tool schema"
```

---

### Task 9: Cap `search_code_direct` to `offset + max_results` (Bug #1)

`search_code_direct` passes `max_results: None` to the core search, materializing all matches in memory before paginating. Cap it to `offset + max_results` so the core engine can stop early.

**Files:**
- Modify: `ferret-mcp/src/server.rs`

**Step 1: Fix the SearchOptions**

At `ferret-mcp/src/server.rs` lines 626-631, change:

```rust
        // Build search options -- we request more than max_results to support pagination
        // by offset (search all, then paginate the result set)
        let search_options = SearchOptions {
            context_lines: context_lines as usize,
            max_results: None, // fetch all, paginate after
        };
```

to:

```rust
        // Request offset + max_results so the core engine can stop early while
        // still returning correct total counts for pagination.
        let search_options = SearchOptions {
            context_lines: context_lines as usize,
            max_results: Some(offset as usize + max_results as usize),
        };
```

**Step 2: Run search_code tests**

Run: `cargo test -p ferret-mcp -- test_search_code`
Expected: All pass. The pagination test (`test_search_code_pagination`) verifies offset + limit behavior still works.

**Step 3: Commit**

```bash
git add ferret-mcp/src/server.rs
git commit -m "perf(mcp): cap search_code_direct results to offset+limit for early termination"
```

---

### Task 10: Remove search_files daemon/direct behavior divergence (Design #10)

When dispatching `search_files` through the daemon, the query is always passed as `path_glob`, even for plain substring queries. The daemon's `Files` handler uses glob matching, while the direct fallback uses case-insensitive substring matching. Fix by only setting `path_glob` when the query contains glob metacharacters, and adding a separate filter/query field otherwise.

**NOTE:** This requires examining the daemon handler to understand what the `Files` variant supports. If the daemon `Files` handler doesn't support substring matching directly, we can still improve the behavior by wrapping non-glob queries in `*query*` glob syntax.

**Files:**
- Modify: `ferret-mcp/src/server.rs`

**Step 1: Wrap non-glob queries for daemon dispatch**

At `ferret-mcp/src/server.rs` lines 316-323, the daemon dispatch currently sends:

```rust
            let req = ferret_indexer_daemon::DaemonRequest::Files {
                language: language_str,
                path_glob: Some(params.query.clone()),
                sort: "path".to_string(),
                limit: Some(max_results),
                color: false,
                cwd: None,
            };
```

Change to:

```rust
            // Wrap non-glob queries in *query* so the daemon's glob matching
            // behaves like substring matching, consistent with the direct fallback.
            let is_glob = params.query.contains('*')
                || params.query.contains('?')
                || params.query.contains('[');
            let path_glob = if is_glob {
                params.query.clone()
            } else {
                format!("*{}*", params.query)
            };

            let req = ferret_indexer_daemon::DaemonRequest::Files {
                language: language_str,
                path_glob: Some(path_glob),
                sort: "path".to_string(),
                limit: Some(max_results),
                color: false,
                cwd: None,
            };
```

**Step 2: Run tests**

Run: `cargo test -p ferret-mcp -- test_search_files`
Expected: All pass

**Step 3: Commit**

```bash
git add ferret-mcp/src/server.rs
git commit -m "fix(mcp): wrap non-glob search_files queries in *query* for daemon dispatch"
```

---

### Task 11: Final validation

**Step 1: Run full workspace checks**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

Expected: All clean, all tests pass.

**Step 2: Final commit if fmt needed**

```bash
cargo fmt --all
git add -A
git commit -m "style: cargo fmt"
```
