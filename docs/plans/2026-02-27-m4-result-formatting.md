# Result Formatting: File-Grouped Output with Context and Pagination

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Extend the existing search result types in `ferret-indexer-core` to support file-grouped output with context lines around matches and offset-based pagination. These types serve as the shared result format consumed by CLI, MCP, and web interfaces -- each renders them differently but all share the same underlying data.

**Architecture:** This plan extends the existing types in `search.rs` rather than replacing them, preserving backward compatibility. Three changes: (1) Add `ContextLine` struct and extend `LineMatch` to carry optional context lines before/after each match. (2) Add `total_file_count` to `SearchResult` for pagination awareness and add a `paginate()` method for offset-based pagination (skip N files, return M files). (3) Add a `collect_context()` function in `multi_search.rs` that enriches `LineMatch` results with surrounding context lines by re-reading file content. The existing `verify_content_matches()` is extended with a `context_lines: usize` parameter. The `Display` impl on `SearchResult` is updated to render file-grouped output with context.

**Tech Stack:** Rust 2024, existing `ferret-indexer-core` modules (search, multi_search, types, error), `serde` (already a dependency for JSON serialization)

**Prerequisite:** ASCII case-fold trigrams plan (`2026-02-27-ascii-casefold-trigrams.md`) must be implemented first. No direct impact on result formatting, but the upstream search pipeline uses case-folded trigrams.

---

## Task 1: Add `ContextLine` struct and extend `LineMatch` with context fields

**Files:**
- Modify: `ferret-indexer-core/src/search.rs`

### Step 1: Write the failing test

Add to the `#[cfg(test)] mod tests` section in `ferret-indexer-core/src/search.rs`:

```rust
#[test]
fn test_context_line_construction() {
    let ctx = ContextLine {
        line_number: 5,
        content: "    let x = 42;".to_string(),
    };
    assert_eq!(ctx.line_number, 5);
    assert_eq!(ctx.content, "    let x = 42;");
}

#[test]
fn test_line_match_with_context() {
    let line = LineMatch {
        line_number: 10,
        content: "fn parse_query(input: &str) -> Query".to_string(),
        ranges: vec![(3, 14)],
        context_before: vec![
            ContextLine { line_number: 8, content: "".to_string() },
            ContextLine { line_number: 9, content: "/// Parse a query string.".to_string() },
        ],
        context_after: vec![
            ContextLine { line_number: 11, content: "    let tokens = tokenize(input);".to_string() },
        ],
    };
    assert_eq!(line.context_before.len(), 2);
    assert_eq!(line.context_after.len(), 1);
    assert_eq!(line.context_before[1].line_number, 9);
    assert_eq!(line.context_after[0].line_number, 11);
}

#[test]
fn test_line_match_default_empty_context() {
    let line = LineMatch {
        line_number: 1,
        content: "use std::io;".to_string(),
        ranges: vec![],
        context_before: vec![],
        context_after: vec![],
    };
    assert!(line.context_before.is_empty());
    assert!(line.context_after.is_empty());
}
```

### Step 2: Run test to verify it fails

Run: `cargo test -p ferret-indexer-core -- test_context_line_construction -v`

Expected: FAIL -- `ContextLine` struct does not exist, `context_before`/`context_after` fields do not exist on `LineMatch`.

### Step 3: Add the `ContextLine` struct and extend `LineMatch`

Add `ContextLine` above the `LineMatch` definition in `search.rs`:

```rust
/// A single line of context (non-matching) surrounding a match.
///
/// Used to provide before/after context lines for search results,
/// similar to `grep -C`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextLine {
    /// 1-based line number within the file.
    pub line_number: u32,
    /// The full text content of the line.
    pub content: String,
}
```

Add two new fields to `LineMatch`:

```rust
pub struct LineMatch {
    /// 1-based line number within the file.
    pub line_number: u32,
    /// The full text content of the matching line.
    pub content: String,
    /// Byte-offset ranges `(start, end)` within `content` that matched the query.
    pub ranges: Vec<(usize, usize)>,
    /// Context lines before this match (nearest lines first, i.e., index 0 is furthest).
    /// Empty when context_lines=0.
    pub context_before: Vec<ContextLine>,
    /// Context lines after this match. Empty when context_lines=0.
    pub context_after: Vec<ContextLine>,
}
```

### Step 4: Fix all existing code that constructs `LineMatch`

After adding the new fields, all existing `LineMatch` construction sites will fail to compile. Fix each one by adding `context_before: vec![], context_after: vec![]`:

1. `multi_search.rs:verify_content_matches()` -- the `LineMatch` construction inside the loop
2. `search.rs` tests -- all test `LineMatch` literals
3. `multi_search.rs` tests -- all test `LineMatch` literals (indirectly via `FileMatch`)

Search for all `LineMatch {` across the workspace and add the two new fields to each.

### Step 5: Run all tests

Run: `cargo test -p ferret-indexer-core`

Expected: All tests pass, including the new context tests.

### Step 6: Update re-exports in lib.rs

Add `ContextLine` to the re-exports in `ferret-indexer-core/src/lib.rs`:

```rust
pub use search::{ContextLine, FileMatch, LineMatch, SearchResult};
```

---

## Task 2: Add `total_file_count` to `SearchResult` and implement `paginate()`

**Files:**
- Modify: `ferret-indexer-core/src/search.rs`
- Modify: `ferret-indexer-core/src/multi_search.rs`

### Step 1: Write the failing tests

Add to `search.rs` tests:

```rust
#[test]
fn test_search_result_total_file_count() {
    let result = SearchResult {
        total_match_count: 42,
        total_file_count: 10,
        files: vec![],
        duration: Duration::from_millis(5),
    };
    assert_eq!(result.total_file_count, 10);
}

#[test]
fn test_search_result_paginate_basic() {
    let files: Vec<FileMatch> = (0..10)
        .map(|i| FileMatch {
            file_id: FileId(i),
            path: PathBuf::from(format!("file_{i}.rs")),
            language: Language::Rust,
            lines: vec![],
            score: 1.0 - (i as f64 / 10.0),
        })
        .collect();

    let result = SearchResult {
        total_match_count: 50,
        total_file_count: 10,
        files,
        duration: Duration::from_millis(5),
    };

    let page = result.paginate(0, 3);
    assert_eq!(page.files.len(), 3);
    assert_eq!(page.total_file_count, 10);
    assert_eq!(page.total_match_count, 50);
    assert_eq!(page.files[0].path, PathBuf::from("file_0.rs"));
    assert_eq!(page.files[2].path, PathBuf::from("file_2.rs"));
}

#[test]
fn test_search_result_paginate_offset() {
    let files: Vec<FileMatch> = (0..10)
        .map(|i| FileMatch {
            file_id: FileId(i),
            path: PathBuf::from(format!("file_{i}.rs")),
            language: Language::Rust,
            lines: vec![],
            score: 0.5,
        })
        .collect();

    let result = SearchResult {
        total_match_count: 50,
        total_file_count: 10,
        files,
        duration: Duration::from_millis(5),
    };

    let page = result.paginate(3, 4);
    assert_eq!(page.files.len(), 4);
    assert_eq!(page.files[0].path, PathBuf::from("file_3.rs"));
    assert_eq!(page.files[3].path, PathBuf::from("file_6.rs"));
}

#[test]
fn test_search_result_paginate_past_end() {
    let files: Vec<FileMatch> = (0..3)
        .map(|i| FileMatch {
            file_id: FileId(i),
            path: PathBuf::from(format!("file_{i}.rs")),
            language: Language::Rust,
            lines: vec![],
            score: 0.5,
        })
        .collect();

    let result = SearchResult {
        total_match_count: 10,
        total_file_count: 3,
        files,
        duration: Duration::from_millis(5),
    };

    // Offset beyond available files
    let page = result.paginate(5, 10);
    assert_eq!(page.files.len(), 0);
    assert_eq!(page.total_file_count, 3);
}

#[test]
fn test_search_result_paginate_partial_last_page() {
    let files: Vec<FileMatch> = (0..7)
        .map(|i| FileMatch {
            file_id: FileId(i),
            path: PathBuf::from(format!("file_{i}.rs")),
            language: Language::Rust,
            lines: vec![],
            score: 0.5,
        })
        .collect();

    let result = SearchResult {
        total_match_count: 35,
        total_file_count: 7,
        files,
        duration: Duration::from_millis(5),
    };

    // Last page has only 2 items instead of 5
    let page = result.paginate(5, 5);
    assert_eq!(page.files.len(), 2);
    assert_eq!(page.files[0].path, PathBuf::from("file_5.rs"));
    assert_eq!(page.files[1].path, PathBuf::from("file_6.rs"));
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_search_result_total_file_count -v`

Expected: FAIL -- `total_file_count` field does not exist, `total_count` renamed to `total_match_count`.

### Step 3: Rename `total_count` to `total_match_count` and add `total_file_count`

In `search.rs`, modify `SearchResult`:

```rust
/// Aggregate result of a search query.
///
/// Contains the matched files, total match/file counts, and query duration.
/// Implements `Display` for plain-text summary output.
///
/// Files are ordered by relevance score (descending). Use [`paginate()`](Self::paginate)
/// for offset-based pagination over the file list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Total number of matching lines across all files (before pagination).
    pub total_match_count: usize,
    /// Total number of files with matches (before pagination).
    pub total_file_count: usize,
    /// Files that matched the query, ordered by relevance score (descending).
    pub files: Vec<FileMatch>,
    /// Wall-clock time taken to execute the query.
    pub duration: Duration,
}
```

### Step 4: Implement `paginate()`

Add an `impl SearchResult` block in `search.rs`:

```rust
impl SearchResult {
    /// Return a paginated view of this result set.
    ///
    /// `offset` is the number of files to skip (0-based).
    /// `limit` is the maximum number of files to return.
    ///
    /// The returned `SearchResult` preserves the original `total_match_count`,
    /// `total_file_count`, and `duration` so consumers know the full result
    /// size for pagination UI.
    pub fn paginate(&self, offset: usize, limit: usize) -> SearchResult {
        let files: Vec<FileMatch> = self
            .files
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();
        SearchResult {
            total_match_count: self.total_match_count,
            total_file_count: self.total_file_count,
            files,
            duration: self.duration,
        }
    }
}
```

### Step 5: Fix all existing `SearchResult` construction sites

After renaming `total_count` -> `total_match_count` and adding `total_file_count`, fix:

1. `multi_search.rs:search_segments()` -- update the `SearchResult` construction:
   ```rust
   let total_file_count = files.len();
   let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();
   Ok(SearchResult {
       total_match_count,
       total_file_count,
       files,
       duration: start.elapsed(),
   })
   ```

2. `multi_search.rs:search_segments()` -- the empty early-return case:
   ```rust
   return Ok(SearchResult {
       total_match_count: 0,
       total_file_count: 0,
       files: Vec::new(),
       duration: start.elapsed(),
   });
   ```

3. `search.rs` tests -- update all existing `SearchResult` literals to use `total_match_count` and add `total_file_count`.

4. `multi_search.rs` tests -- update any tests that assert on `result.total_count`.

### Step 6: Update the `Display` impl

Update the `Display` impl for `SearchResult`:

```rust
impl fmt::Display for SearchResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} results in {} files ({:.1?})",
            self.total_match_count,
            self.total_file_count,
            self.duration,
        )
    }
}
```

### Step 7: Run all tests

Run: `cargo test -p ferret-indexer-core`

Expected: All tests pass.

---

## Task 3: Add context line collection to `verify_content_matches()`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs`

### Step 1: Write the failing tests

Add to `multi_search.rs` tests:

```rust
#[test]
fn test_verify_with_context_lines() {
    let content = b"line one\nline two\nline three\nline four\nline five\n";
    let matches = verify_content_matches(content, "three", 1);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].line_number, 3);
    assert_eq!(matches[0].context_before.len(), 1);
    assert_eq!(matches[0].context_before[0].line_number, 2);
    assert_eq!(matches[0].context_before[0].content, "line two");
    assert_eq!(matches[0].context_after.len(), 1);
    assert_eq!(matches[0].context_after[0].line_number, 4);
    assert_eq!(matches[0].context_after[0].content, "line four");
}

#[test]
fn test_verify_with_context_at_start() {
    let content = b"line one\nline two\nline three\n";
    let matches = verify_content_matches(content, "one", 2);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].line_number, 1);
    // No context before first line
    assert_eq!(matches[0].context_before.len(), 0);
    assert_eq!(matches[0].context_after.len(), 2);
    assert_eq!(matches[0].context_after[0].line_number, 2);
    assert_eq!(matches[0].context_after[1].line_number, 3);
}

#[test]
fn test_verify_with_context_at_end() {
    let content = b"line one\nline two\nline three\n";
    let matches = verify_content_matches(content, "three", 2);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].line_number, 3);
    assert_eq!(matches[0].context_before.len(), 2);
    assert_eq!(matches[0].context_before[0].line_number, 1);
    assert_eq!(matches[0].context_before[1].line_number, 2);
    // No context after last line
    assert_eq!(matches[0].context_after.len(), 0);
}

#[test]
fn test_verify_with_zero_context() {
    let content = b"line one\nline two\nline three\n";
    let matches = verify_content_matches(content, "two", 0);
    assert_eq!(matches.len(), 1);
    assert!(matches[0].context_before.is_empty());
    assert!(matches[0].context_after.is_empty());
}

#[test]
fn test_verify_context_adjacent_matches_no_overlap() {
    // When two matches are adjacent, context should not overlap
    let content = b"line 1\nmatch A\nmatch B\nline 4\n";
    let matches = verify_content_matches(content, "match", 1);
    assert_eq!(matches.len(), 2);
    // First match context_after should include "match B" (it's context even though it's also a match)
    assert_eq!(matches[0].context_after.len(), 1);
    assert_eq!(matches[0].context_after[0].line_number, 3);
    // Second match context_before should include "match A"
    assert_eq!(matches[1].context_before.len(), 1);
    assert_eq!(matches[1].context_before[0].line_number, 2);
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_verify_with_context_lines -v`

Expected: FAIL -- `verify_content_matches` currently takes 2 args, not 3.

### Step 3: Add `context_lines` parameter to `verify_content_matches()`

Modify the signature:

```rust
fn verify_content_matches(content: &[u8], query: &str, context_lines: usize) -> Vec<LineMatch> {
```

Implementation approach:
1. First pass: collect all matching lines (indices and their `LineMatch` data) as before.
2. Second pass: for each match, collect `context_before` and `context_after` from the full line list.

```rust
fn verify_content_matches(content: &[u8], query: &str, context_lines: usize) -> Vec<LineMatch> {
    if query.is_empty() || content.is_empty() {
        return Vec::new();
    }

    let query_bytes = query.as_bytes();
    let text = String::from_utf8_lossy(content);
    let all_lines: Vec<&str> = text.split('\n').collect();

    // First pass: find matching line indices and their ranges
    let mut match_indices: Vec<(usize, Vec<(usize, usize)>)> = Vec::new();

    for (line_idx, line) in all_lines.iter().enumerate() {
        // Skip empty trailing line from trailing newline
        if line.is_empty() && line_idx > 0 && line_idx == all_lines.len() - 1 {
            continue;
        }

        let line_bytes = line.as_bytes();
        let mut ranges = Vec::new();
        let mut search_start = 0;

        while search_start + query_bytes.len() <= line_bytes.len() {
            if let Some(pos) = find_substring(&line_bytes[search_start..], query_bytes) {
                let abs_start = search_start + pos;
                let abs_end = abs_start + query_bytes.len();
                ranges.push((abs_start, abs_end));
                search_start = abs_start + 1;
            } else {
                break;
            }
        }

        if !ranges.is_empty() {
            match_indices.push((line_idx, ranges));
        }
    }

    // Second pass: build LineMatch with context
    match_indices
        .iter()
        .map(|(line_idx, ranges)| {
            let line_idx = *line_idx;

            let context_before = if context_lines > 0 {
                let start = line_idx.saturating_sub(context_lines);
                (start..line_idx)
                    .map(|i| ContextLine {
                        line_number: (i + 1) as u32,
                        content: all_lines[i].to_string(),
                    })
                    .collect()
            } else {
                vec![]
            };

            let context_after = if context_lines > 0 {
                let end = (line_idx + 1 + context_lines).min(all_lines.len());
                // Skip trailing empty line
                let effective_end = if end > 0
                    && all_lines.last().is_some_and(|l| l.is_empty())
                    && end == all_lines.len()
                {
                    end - 1
                } else {
                    end
                };
                ((line_idx + 1)..effective_end)
                    .map(|i| ContextLine {
                        line_number: (i + 1) as u32,
                        content: all_lines[i].to_string(),
                    })
                    .collect()
            } else {
                vec![]
            };

            LineMatch {
                line_number: (line_idx + 1) as u32,
                content: all_lines[line_idx].to_string(),
                ranges: ranges.clone(),
                context_before,
                context_after,
            }
        })
        .collect()
}
```

### Step 4: Fix the call site in `search_single_segment`

Update the call to `verify_content_matches` in `search_single_segment()`:

```rust
let line_matches = verify_content_matches(&content, query, 0);
```

For now, pass `0` (no context). The context parameter will be threaded through in Task 5 when we add `SearchOptions`.

### Step 5: Fix existing tests

Update all existing calls to `verify_content_matches` in tests to pass `0` as the third argument:

```rust
let matches = verify_content_matches(content, "println", 0);
```

### Step 6: Add `use` for `ContextLine`

Add `ContextLine` to the import from `crate::search` in `multi_search.rs`:

```rust
use crate::search::{ContextLine, FileMatch, LineMatch, SearchResult};
```

### Step 7: Run all tests

Run: `cargo test -p ferret-indexer-core`

Expected: All tests pass, including the new context tests.

---

## Task 4: Update `Display` impl with file-grouped output format

**Files:**
- Modify: `ferret-indexer-core/src/search.rs`

### Step 1: Write the failing tests

Add to `search.rs` tests:

```rust
#[test]
fn test_file_match_display_with_context() {
    let file_match = FileMatch {
        file_id: FileId(1),
        path: PathBuf::from("src/main.rs"),
        language: Language::Rust,
        lines: vec![
            LineMatch {
                line_number: 5,
                content: "fn main() {".to_string(),
                ranges: vec![(0, 7)],
                context_before: vec![
                    ContextLine { line_number: 4, content: "".to_string() },
                ],
                context_after: vec![
                    ContextLine { line_number: 6, content: "    println!(\"hello\");".to_string() },
                ],
            },
        ],
        score: 0.9,
    };
    let display = file_match.to_string();
    assert!(display.contains("src/main.rs"));
    assert!(display.contains("Rust"));
    assert!(display.contains("L5:"));
    assert!(display.contains("fn main() {"));
}

#[test]
fn test_search_result_display_with_pagination() {
    let result = SearchResult {
        total_match_count: 42,
        total_file_count: 10,
        files: vec![
            FileMatch {
                file_id: FileId(1),
                path: PathBuf::from("src/main.rs"),
                language: Language::Rust,
                lines: vec![],
                score: 0.9,
            },
        ],
        duration: Duration::from_millis(5),
    };
    let display = result.to_string();
    assert!(display.contains("42 results"));
    assert!(display.contains("10 files"));
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_file_match_display_with_context -v`

Expected: FAIL -- `FileMatch` does not implement `Display`.

### Step 3: Implement `Display` for `FileMatch`

Add to `search.rs`:

```rust
impl fmt::Display for FileMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // File header: ## path/to/file  (Language, N matches)
        writeln!(f, "## {} ({}, {} matches)", self.path.display(), self.language, self.lines.len())?;

        for line_match in &self.lines {
            // Context before
            for ctx in &line_match.context_before {
                writeln!(f, "L{}:  {}", ctx.line_number, ctx.content)?;
            }
            // Match line (marked with *)
            writeln!(f, "L{}:* {}", line_match.line_number, line_match.content)?;
            // Context after
            for ctx in &line_match.context_after {
                writeln!(f, "L{}:  {}", ctx.line_number, ctx.content)?;
            }
        }
        Ok(())
    }
}
```

### Step 4: Update the `SearchResult` `Display` impl

Already handled in Task 2 Step 6. Verify the format:

```rust
impl fmt::Display for SearchResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} results in {} files ({:.1?})",
            self.total_match_count,
            self.total_file_count,
            self.duration,
        )?;
        for file_match in &self.files {
            writeln!(f)?;
            write!(f, "{file_match}")?;
        }
        Ok(())
    }
}
```

### Step 5: Fix existing `Display` tests

Update the existing `test_search_result_display` and `test_search_result_display_empty` tests for the renamed fields and new output format.

### Step 6: Run all tests

Run: `cargo test -p ferret-indexer-core`

Expected: All tests pass.

---

## Task 5: Thread context lines through `search_segments()` via `SearchOptions`

**Files:**
- Modify: `ferret-indexer-core/src/search.rs`
- Modify: `ferret-indexer-core/src/multi_search.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Write the failing tests

Add to `multi_search.rs` tests:

```rust
#[test]
fn test_search_segments_with_context() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"use std::io;\n\nfn main() {\n    println!(\"hello\");\n}\n".to_vec(),
            mtime: 0,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);
    let opts = SearchOptions { context_lines: 1 };
    let result = search_segments_with_options(&snapshot, "println", &opts).unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].lines.len(), 1);

    let line = &result.files[0].lines[0];
    assert_eq!(line.line_number, 4);
    // Context before: line 3 "fn main() {"
    assert_eq!(line.context_before.len(), 1);
    assert_eq!(line.context_before[0].line_number, 3);
    assert!(line.context_before[0].content.contains("fn main()"));
    // Context after: line 5 "}"
    assert_eq!(line.context_after.len(), 1);
    assert_eq!(line.context_after[0].line_number, 5);
}

#[test]
fn test_search_segments_default_no_context() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"line 1\nline 2\nfn main() {\n    println!(\"hello\");\n}\n".to_vec(),
            mtime: 0,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);
    // Original search_segments should produce no context
    let result = search_segments(&snapshot, "println").unwrap();
    assert_eq!(result.files[0].lines[0].context_before.len(), 0);
    assert_eq!(result.files[0].lines[0].context_after.len(), 0);
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_search_segments_with_context -v`

Expected: FAIL -- `SearchOptions` and `search_segments_with_options` do not exist.

### Step 3: Add `SearchOptions` to `search.rs`

```rust
/// Options that control search behavior.
///
/// Passed to [`search_segments_with_options()`](crate::multi_search::search_segments_with_options)
/// to configure context lines and other search parameters.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Number of context lines to include before and after each match.
    /// Default: 0 (no context).
    pub context_lines: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions { context_lines: 0 }
    }
}
```

### Step 4: Add `search_segments_with_options()` to `multi_search.rs`

Add a new public function that accepts `SearchOptions`:

```rust
/// Search across multiple segments with options.
///
/// Like [`search_segments()`] but accepts [`SearchOptions`] to configure
/// context lines and other search parameters.
pub fn search_segments_with_options(
    snapshot: &SegmentList,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResult, IndexError> {
    let start = Instant::now();

    if snapshot.is_empty() || query.len() < 3 {
        return Ok(SearchResult {
            total_match_count: 0,
            total_file_count: 0,
            files: Vec::new(),
            duration: start.elapsed(),
        });
    }

    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();

    for segment in snapshot.iter() {
        let tombstones = segment.load_tombstones()?;
        let file_matches = search_single_segment_with_context(
            segment,
            query,
            &tombstones,
            options.context_lines,
        )?;

        for fm in file_matches {
            let seg_id = segment.segment_id();
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {}
                _ => {
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
        }
    }

    let mut files: Vec<FileMatch> = merged.into_values().map(|(_, fm)| fm).collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total_file_count = files.len();
    let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_match_count,
        total_file_count,
        files,
        duration: start.elapsed(),
    })
}
```

Also add `search_single_segment_with_context` (or modify `search_single_segment` to accept context_lines):

```rust
fn search_single_segment_with_context(
    segment: &Segment,
    query: &str,
    tombstones: &TombstoneSet,
    context_lines: usize,
) -> Result<Vec<FileMatch>, IndexError> {
    let candidates = find_candidates(segment.trigram_reader(), query)?;
    let mut file_matches = Vec::new();

    for file_id in candidates {
        if tombstones.contains(file_id) {
            continue;
        }

        let meta = match segment.get_metadata(file_id)? {
            Some(m) => m,
            None => continue,
        };

        let content = segment
            .content_reader()
            .read_content(meta.content_offset, meta.content_len)?;

        let line_matches = verify_content_matches(&content, query, context_lines);
        if line_matches.is_empty() {
            continue;
        }

        let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
        let line_count = meta.line_count.max(1) as f64;
        let score = (total_match_ranges as f64 / line_count).min(1.0);

        file_matches.push(FileMatch {
            file_id,
            path: PathBuf::from(&meta.path),
            language: meta.language,
            lines: line_matches,
            score,
        });
    }

    Ok(file_matches)
}
```

### Step 5: Refactor `search_segments` to delegate

Make the existing `search_segments` delegate to `search_segments_with_options` with default options:

```rust
pub fn search_segments(snapshot: &SegmentList, query: &str) -> Result<SearchResult, IndexError> {
    search_segments_with_options(snapshot, query, &SearchOptions::default())
}
```

### Step 6: Update re-exports in lib.rs

```rust
pub use multi_search::{search_segments, search_segments_with_options};
pub use search::{ContextLine, FileMatch, LineMatch, SearchOptions, SearchResult};
```

### Step 7: Run all tests

Run: `cargo test -p ferret-indexer-core`

Expected: All tests pass. The existing `search_segments` tests continue to work (they use context_lines=0 via the default).

---

## Task 6: Clippy and formatting cleanup

**Files:**
- All modified files

### Step 1: Run clippy

Run: `cargo clippy --workspace -- -D warnings`

Fix any warnings.

### Step 2: Run formatter

Run: `cargo fmt --all`

### Step 3: Run the full test suite

Run: `cargo test --workspace`

Expected: All tests pass, no warnings.

---

## Summary of Changes

### New Types
- `ContextLine` -- a non-matching line surrounding a match (line_number, content)
- `SearchOptions` -- configurable search parameters (context_lines)

### Modified Types
- `LineMatch` -- added `context_before: Vec<ContextLine>` and `context_after: Vec<ContextLine>` fields
- `SearchResult` -- renamed `total_count` to `total_match_count`, added `total_file_count: usize`, added `paginate(offset, limit)` method
- `FileMatch` -- added `Display` impl for file-grouped rendering

### New Functions
- `search_segments_with_options(snapshot, query, options)` -- search with configurable options
- `SearchResult::paginate(offset, limit)` -- offset-based pagination

### Modified Functions
- `verify_content_matches(content, query, context_lines)` -- added context_lines parameter
- `search_segments()` -- now delegates to `search_segments_with_options` with defaults

### Re-exports Added
- `ContextLine`, `SearchOptions`, `search_segments_with_options`

### Backward Compatibility
- All existing public function signatures preserved (`search_segments` still works with same args)
- `LineMatch` has new fields but they default to empty vecs (no context) when context_lines=0
- `SearchResult` field rename (`total_count` -> `total_match_count`) is a breaking change within the crate but no external consumers exist yet (CLI/MCP are stubs)
