# Multi-Segment Query Merging with Snapshot Isolation Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement query execution across multiple segments with snapshot isolation, so that a search query sees a consistent view of the index even while new segments are being published concurrently.

**Architecture:** Three new pieces: (1) `IndexState` manages an `Arc<Vec<Arc<Segment>>>` representing the current set of active segments, with a `snapshot()` method that clones the Arc for lock-free reads and a `publish()` method that atomically swaps the segment list under a writer mutex. (2) A multi-segment search function that takes a snapshot and a query string, searches each segment (trigram intersection + tombstone filtering + content verification), merges results across segments (dedup by file path preferring the newest segment), and returns a `SearchResult` with timing. (3) `SegmentList` is a type alias for the snapshot (`Arc<Vec<Arc<Segment>>>`). The search pipeline per segment is: `find_candidates()` -> filter tombstones -> read metadata -> read content -> verify match -> build `LineMatch`/`FileMatch`. Cross-segment dedup uses a `HashMap<String, (SegmentId, FileMatch)>` keeping only the entry from the highest SegmentId.

**Tech Stack:** Rust 2024, `std::sync::{Arc, Mutex}`, existing `ferret-indexer-core` modules (segment, tombstone, intersection, search, content, metadata, types, error), `tempfile` (dev), `regex` for content verification

---

## Task 1: Add `SegmentList` type alias and `IndexState` struct skeleton

**Files:**
- Create: `ferret-indexer-core/src/index_state.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Write the failing test

Create `ferret-indexer-core/src/index_state.rs` with a test for constructing an empty `IndexState`:

```rust
//! Index state management with snapshot isolation.
//!
//! [`IndexState`] holds the current list of active segments as an
//! `Arc<Vec<Arc<Segment>>>`. Readers call [`snapshot()`](IndexState::snapshot)
//! to get a consistent, lock-free view. Writers call
//! [`publish()`](IndexState::publish) to atomically swap in a new segment list.
//!
//! The `SegmentList` type alias provides a convenient name for the snapshot type.

use std::sync::{Arc, Mutex};

use crate::error::IndexError;
use crate::segment::Segment;
use crate::types::SegmentId;

/// A snapshot of the active segment list. Lock-free for readers via `Arc::clone()`.
pub type SegmentList = Arc<Vec<Arc<Segment>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_state_new_is_empty() {
        let state = IndexState::new();
        let snap = state.snapshot();
        assert!(snap.is_empty());
    }
}
```

### Step 2: Register the module in lib.rs

Add to `ferret-indexer-core/src/lib.rs`:

```rust
pub mod index_state;
```

And add re-exports:

```rust
pub use index_state::{IndexState, SegmentList};
```

### Step 3: Run test to verify it fails

Run: `cargo test -p ferret-indexer-core -- test_index_state_new_is_empty -v`

Expected: FAIL -- `IndexState` struct does not exist yet.

### Step 4: Implement the IndexState struct

Add to `index_state.rs`, above the test module:

```rust
/// Manages the current set of active segments with snapshot isolation.
///
/// Readers call [`snapshot()`](Self::snapshot) to get a consistent `SegmentList`
/// (just an `Arc::clone()`, no locks). Writers call [`publish()`](Self::publish)
/// to atomically swap in a new segment list; a `Mutex` serializes writers.
///
/// # Concurrency Model
///
/// - **Readers**: Lock-free. `snapshot()` clones the outer `Arc`, giving a
///   consistent view even if the writer publishes a new list concurrently.
///   Old snapshots remain valid until all references are dropped.
///
/// - **Writers**: Serialized by an internal `Mutex`. Only one thread can call
///   `publish()` at a time. The actual swap is an `Arc` store, so readers
///   never block.
pub struct IndexState {
    /// The current segment list, wrapped in Arc for lock-free snapshot reads.
    /// The Mutex serializes writers; readers never take the lock.
    current: Mutex<SegmentList>,
}

impl IndexState {
    /// Create a new IndexState with an empty segment list.
    pub fn new() -> Self {
        IndexState {
            current: Mutex::new(Arc::new(Vec::new())),
        }
    }

    /// Take a snapshot of the current segment list.
    ///
    /// This is a cheap `Arc::clone()` -- no locks, no copies. The returned
    /// `SegmentList` is a frozen view that remains valid regardless of
    /// subsequent `publish()` calls.
    pub fn snapshot(&self) -> SegmentList {
        let guard = self.current.lock().unwrap();
        Arc::clone(&guard)
    }

    /// Atomically replace the segment list with a new one.
    ///
    /// Only one writer can publish at a time (serialized by internal Mutex).
    /// Existing snapshots are unaffected -- they hold their own `Arc` references.
    pub fn publish(&self, new_segments: Vec<Arc<Segment>>) {
        let mut guard = self.current.lock().unwrap();
        *guard = Arc::new(new_segments);
    }
}

impl Default for IndexState {
    fn default() -> Self {
        Self::new()
    }
}
```

### Step 5: Run test to verify it passes

Run: `cargo test -p ferret-indexer-core -- test_index_state_new_is_empty -v`

Expected: PASS

### Step 6: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 7: Commit

```bash
git add ferret-indexer-core/src/index_state.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(index_state): add IndexState struct with SegmentList type alias"
```

---

## Task 2: Add `snapshot()` and `publish()` tests for IndexState

**Files:**
- Modify: `ferret-indexer-core/src/index_state.rs`

### Step 1: Write the tests

Add these tests to the `tests` module in `index_state.rs`. These use real `Segment` objects built via `SegmentWriter`:

```rust
use crate::segment::{InputFile, SegmentWriter};

/// Helper: build a segment with the given ID and files in a temp directory.
fn build_test_segment(
    base_dir: &std::path::Path,
    segment_id: SegmentId,
    files: Vec<InputFile>,
) -> Arc<Segment> {
    let writer = SegmentWriter::new(base_dir, segment_id);
    Arc::new(writer.build(files).unwrap())
}

#[test]
fn test_publish_and_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg0 = build_test_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn alpha() {}".to_vec(),
            mtime: 0,
        }],
    );

    let state = IndexState::new();
    state.publish(vec![seg0.clone()]);

    let snap = state.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].segment_id(), SegmentId(0));
}

#[test]
fn test_snapshot_is_isolated_from_publish() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg0 = build_test_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn alpha() {}".to_vec(),
            mtime: 0,
        }],
    );

    let seg1 = build_test_segment(
        &base_dir,
        SegmentId(1),
        vec![InputFile {
            path: "b.rs".to_string(),
            content: b"fn beta() {}".to_vec(),
            mtime: 0,
        }],
    );

    let state = IndexState::new();
    state.publish(vec![seg0.clone()]);

    // Take a snapshot before publishing seg1
    let snap_before = state.snapshot();
    assert_eq!(snap_before.len(), 1);

    // Publish a new list with both segments
    state.publish(vec![seg0, seg1]);

    // The old snapshot should still see only 1 segment
    assert_eq!(snap_before.len(), 1);

    // A new snapshot sees both segments
    let snap_after = state.snapshot();
    assert_eq!(snap_after.len(), 2);
}

#[test]
fn test_publish_replaces_entirely() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg0 = build_test_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn alpha() {}".to_vec(),
            mtime: 0,
        }],
    );

    let seg1 = build_test_segment(
        &base_dir,
        SegmentId(1),
        vec![InputFile {
            path: "b.rs".to_string(),
            content: b"fn beta() {}".to_vec(),
            mtime: 0,
        }],
    );

    let state = IndexState::new();
    state.publish(vec![seg0, seg1]);
    assert_eq!(state.snapshot().len(), 2);

    // Publish empty list
    state.publish(vec![]);
    assert_eq!(state.snapshot().len(), 0);
}

#[test]
fn test_default_trait() {
    let state = IndexState::default();
    assert!(state.snapshot().is_empty());
}
```

### Step 2: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- index_state -v`

Expected: All tests PASS.

### Step 3: Commit

```bash
git add ferret-indexer-core/src/index_state.rs
git commit -m "test(index_state): add publish, snapshot isolation, and replace tests"
```

---

## Task 3: Add `Segment::load_tombstones()` method

**Files:**
- Modify: `ferret-indexer-core/src/segment.rs`

The multi-segment search needs to read tombstones from each segment. Currently `Segment` does not expose tombstone loading. We add a method that reads `tombstones.bin` from the segment directory, returning an empty `TombstoneSet` if the file is empty (which is the initial state written by `SegmentWriter`).

### Step 1: Write the failing test

Add to the `tests` module in `segment.rs`:

```rust
use crate::tombstone::TombstoneSet;

#[test]
fn test_segment_load_tombstones_empty() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    fs::create_dir_all(&base_dir).unwrap();

    let files = vec![InputFile {
        path: "a.rs".to_string(),
        content: b"fn a() {}".to_vec(),
        mtime: 0,
    }];

    let writer = SegmentWriter::new(&base_dir, SegmentId(0));
    let segment = writer.build(files).unwrap();

    let tombstones = segment.load_tombstones().unwrap();
    assert!(tombstones.is_empty());
    assert_eq!(tombstones.len(), 0);
}

#[test]
fn test_segment_load_tombstones_after_write() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    fs::create_dir_all(&base_dir).unwrap();

    let files = vec![
        InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() {}".to_vec(),
            mtime: 0,
        },
        InputFile {
            path: "b.rs".to_string(),
            content: b"fn b() {}".to_vec(),
            mtime: 0,
        },
    ];

    let writer = SegmentWriter::new(&base_dir, SegmentId(0));
    let segment = writer.build(files).unwrap();

    // Manually write a tombstone file marking FileId(0) as deleted
    let mut ts = TombstoneSet::new();
    ts.insert(FileId(0));
    ts.write_to(&segment.dir_path().join("tombstones.bin")).unwrap();

    let loaded = segment.load_tombstones().unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(loaded.contains(FileId(0)));
    assert!(!loaded.contains(FileId(1)));
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_segment_load_tombstones -v`

Expected: FAIL -- `load_tombstones` method does not exist.

### Step 3: Implement `load_tombstones`

Add this method to the `impl Segment` block in `segment.rs`. Also add the import for `TombstoneSet`:

At the top of `segment.rs`, add to the imports:

```rust
use crate::tombstone::TombstoneSet;
```

Add this method inside `impl Segment`:

```rust
    /// Load the tombstone set for this segment from disk.
    ///
    /// Reads `tombstones.bin` from the segment directory. If the file is empty
    /// (the initial state after segment creation), returns an empty `TombstoneSet`.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::Io`] if the file cannot be read, or
    /// [`IndexError::IndexCorruption`] if the file is non-empty but malformed.
    pub fn load_tombstones(&self) -> Result<TombstoneSet, IndexError> {
        let path = self.dir_path.join("tombstones.bin");
        let data = std::fs::read(&path)?;
        if data.is_empty() {
            return Ok(TombstoneSet::new());
        }
        TombstoneSet::read_from(&path)
    }
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_segment_load_tombstones -v`

Expected: PASS

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/segment.rs
git commit -m "feat(segment): add load_tombstones() method for reading tombstone bitmap"
```

---

## Task 4: Create `multi_search.rs` module with content verification helper

**Files:**
- Create: `ferret-indexer-core/src/multi_search.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

The content verification step is the core of turning trigram candidates into actual `LineMatch` results. This task implements the `verify_content_matches` function that, given file content bytes and a query string, finds all matching lines and returns `Vec<LineMatch>`.

### Step 1: Write the failing test

Create `ferret-indexer-core/src/multi_search.rs`:

```rust
//! Multi-segment search with snapshot isolation.
//!
//! Provides [`search_segments()`] which executes a query across multiple segments,
//! filtering tombstoned entries, verifying matches in file content, deduplicating
//! across segments (preferring the newest), and returning a unified [`SearchResult`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::error::IndexError;
use crate::index_state::SegmentList;
use crate::intersection::find_candidates;
use crate::search::{FileMatch, LineMatch, SearchResult};
use crate::segment::Segment;
use crate::types::{FileId, SegmentId};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_single_match() {
        let content = b"fn main() {\n    println!(\"hello\");\n}\n";
        let matches = verify_content_matches(content, "println");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);
        assert!(matches[0].content.contains("println"));
        assert_eq!(matches[0].ranges.len(), 1);
    }

    #[test]
    fn test_verify_no_match() {
        let content = b"fn main() {}\n";
        let matches = verify_content_matches(content, "foobar");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_verify_multiple_matches_same_line() {
        let content = b"let aa = aa + aa;\n";
        let matches = verify_content_matches(content, "aa");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 1);
        // Should have 3 ranges: positions 4, 9, 14
        assert_eq!(matches[0].ranges.len(), 3);
    }

    #[test]
    fn test_verify_multiple_lines() {
        let content = b"fn foo() {}\nfn bar() {}\nfn baz() {}\n";
        let matches = verify_content_matches(content, "fn ");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].line_number, 1);
        assert_eq!(matches[1].line_number, 2);
        assert_eq!(matches[2].line_number, 3);
    }

    #[test]
    fn test_verify_empty_query() {
        let content = b"fn main() {}\n";
        let matches = verify_content_matches(content, "");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_verify_empty_content() {
        let content = b"";
        let matches = verify_content_matches(content, "foo");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_verify_no_trailing_newline() {
        let content = b"line one\nline two";
        let matches = verify_content_matches(content, "two");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);
    }
}
```

### Step 2: Register the module in lib.rs

Add to `ferret-indexer-core/src/lib.rs`:

```rust
pub mod multi_search;
```

And add re-export:

```rust
pub use multi_search::search_segments;
```

### Step 3: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_verify -v`

Expected: FAIL -- `verify_content_matches` function does not exist.

### Step 4: Implement `verify_content_matches`

Add to `multi_search.rs`, above the test module:

```rust
/// Verify that a query string actually appears in file content, and return
/// the matching lines with highlight ranges.
///
/// This is the content verification step after trigram candidate filtering.
/// For each line in `content`, finds all occurrences of `query` (byte-level
/// substring search) and builds a `LineMatch` with 1-based line numbers and
/// byte-offset highlight ranges.
///
/// Returns an empty vector if the query is empty or not found.
fn verify_content_matches(content: &[u8], query: &str) -> Vec<LineMatch> {
    if query.is_empty() || content.is_empty() {
        return Vec::new();
    }

    let query_bytes = query.as_bytes();
    let text = String::from_utf8_lossy(content);
    let mut matches = Vec::new();

    for (line_idx, line) in text.split('\n').enumerate() {
        // Skip empty trailing line from trailing newline
        if line.is_empty() && line_idx > 0 {
            // Check if this is the last split element after a trailing newline
            continue;
        }

        let mut ranges = Vec::new();
        let line_bytes = line.as_bytes();
        let mut search_start = 0;

        while search_start + query_bytes.len() <= line_bytes.len() {
            if let Some(pos) = find_substring(&line_bytes[search_start..], query_bytes) {
                let abs_start = search_start + pos;
                let abs_end = abs_start + query_bytes.len();
                ranges.push((abs_start, abs_end));
                search_start = abs_start + 1; // advance past start to find overlapping matches
            } else {
                break;
            }
        }

        if !ranges.is_empty() {
            matches.push(LineMatch {
                line_number: (line_idx + 1) as u32, // 1-based
                content: line.to_string(),
                ranges,
            });
        }
    }

    matches
}

/// Find the first occurrence of `needle` in `haystack`, returning the byte offset.
fn find_substring(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
```

### Step 5: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_verify -v`

Expected: PASS

### Step 6: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 7: Commit

```bash
git add ferret-indexer-core/src/multi_search.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(multi_search): add verify_content_matches for line-level match extraction"
```

---

## Task 5: Implement single-segment search helper

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs`

This task adds `search_single_segment()`, which searches one segment: runs `find_candidates()`, filters tombstones, reads metadata and content, verifies matches, and builds `FileMatch` results.

### Step 1: Write the failing test

Add to the `tests` module in `multi_search.rs`:

```rust
use crate::segment::{InputFile, SegmentWriter};
use crate::tombstone::TombstoneSet;

/// Helper: build a segment with the given ID and files.
fn build_segment(
    base_dir: &std::path::Path,
    segment_id: SegmentId,
    files: Vec<InputFile>,
) -> Arc<Segment> {
    let writer = SegmentWriter::new(base_dir, segment_id);
    Arc::new(writer.build(files).unwrap())
}

#[test]
fn test_search_single_segment_basic() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
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
                content: b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n".to_vec(),
                mtime: 0,
            },
        ],
    );

    let tombstones = TombstoneSet::new();
    let results = search_single_segment(&seg, "println", &tombstones).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, PathBuf::from("main.rs"));
    assert_eq!(results[0].lines.len(), 1);
    assert_eq!(results[0].lines[0].line_number, 2);
}

#[test]
fn test_search_single_segment_with_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![
            InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"hello\"); }\n".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "lib.rs".to_string(),
                content: b"fn lib() { println!(\"world\"); }\n".to_vec(),
                mtime: 0,
            },
        ],
    );

    // Tombstone file 0 (main.rs)
    let mut tombstones = TombstoneSet::new();
    tombstones.insert(FileId(0));

    let results = search_single_segment(&seg, "println", &tombstones).unwrap();
    // Only lib.rs should appear (main.rs is tombstoned)
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, PathBuf::from("lib.rs"));
}

#[test]
fn test_search_single_segment_no_match() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() {}\n".to_vec(),
            mtime: 0,
        }],
    );

    let tombstones = TombstoneSet::new();
    let results = search_single_segment(&seg, "foobar", &tombstones).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_search_single_segment_short_query() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() {}\n".to_vec(),
            mtime: 0,
        }],
    );

    let tombstones = TombstoneSet::new();
    // Queries < 3 chars produce no trigrams, so no candidates
    let results = search_single_segment(&seg, "fn", &tombstones).unwrap();
    assert!(results.is_empty());
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_search_single_segment -v`

Expected: FAIL -- `search_single_segment` function does not exist.

### Step 3: Implement `search_single_segment`

Add to `multi_search.rs`, above the test module:

```rust
use crate::tombstone::TombstoneSet;

/// Search a single segment for the given query, filtering tombstoned entries.
///
/// Pipeline:
/// 1. `find_candidates(segment.trigram_reader(), query)` -> candidate FileIds
/// 2. Filter out tombstoned FileIds
/// 3. For each candidate: read metadata, read content, verify match
/// 4. Build FileMatch results with relevance score
///
/// Returns a vector of `FileMatch` for files in this segment that match.
fn search_single_segment(
    segment: &Segment,
    query: &str,
    tombstones: &TombstoneSet,
) -> Result<Vec<FileMatch>, IndexError> {
    let candidates = find_candidates(segment.trigram_reader(), query)?;

    let mut file_matches = Vec::new();

    for file_id in candidates {
        // Skip tombstoned entries
        if tombstones.contains(file_id) {
            continue;
        }

        // Read metadata
        let meta = match segment.get_metadata(file_id)? {
            Some(m) => m,
            None => continue,
        };

        // Read and decompress content
        let content = segment
            .content_reader()
            .read_content(meta.content_offset, meta.content_len)?;

        // Verify the query actually appears in the content
        let line_matches = verify_content_matches(&content, query);
        if line_matches.is_empty() {
            continue;
        }

        // Compute a simple relevance score: match count / line count
        // (more matches relative to file size = more relevant)
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

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_search_single_segment -v`

Expected: PASS

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "feat(multi_search): add search_single_segment with tombstone filtering"
```

---

## Task 6: Implement `search_segments()` with cross-segment dedup and merging

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs`

This is the main public API. It takes a `SegmentList` (snapshot) and a query, searches each segment, deduplicates results across segments (preferring the newest segment = highest SegmentId), sorts by relevance, and returns a `SearchResult`.

### Step 1: Write the failing tests

Add to the `tests` module in `multi_search.rs`:

```rust
#[test]
fn test_search_segments_single_segment() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
            mtime: 0,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);
    let result = search_segments(&snapshot, "println").unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, PathBuf::from("main.rs"));
    assert_eq!(result.total_count, 1);
}

#[test]
fn test_search_segments_multiple_segments() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg0 = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() { println!(\"hello\"); }\n".to_vec(),
            mtime: 0,
        }],
    );

    let seg1 = build_segment(
        &base_dir,
        SegmentId(1),
        vec![InputFile {
            path: "lib.rs".to_string(),
            content: b"pub fn lib() { println!(\"world\"); }\n".to_vec(),
            mtime: 0,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
    let result = search_segments(&snapshot, "println").unwrap();
    assert_eq!(result.files.len(), 2);
    // Both files should appear
    let paths: Vec<&str> = result.files.iter().map(|f| f.path.to_str().unwrap()).collect();
    assert!(paths.contains(&"main.rs"));
    assert!(paths.contains(&"lib.rs"));
}

#[test]
fn test_search_segments_dedup_prefers_newest() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // Segment 0 has main.rs with "hello"
    let seg0 = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() { println!(\"hello\"); }\n".to_vec(),
            mtime: 100,
        }],
    );

    // Segment 1 has main.rs with updated content (same path, different content)
    let seg1 = build_segment(
        &base_dir,
        SegmentId(1),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() { println!(\"goodbye world\"); }\n".to_vec(),
            mtime: 200,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
    let result = search_segments(&snapshot, "println").unwrap();

    // Should only have one result for main.rs (from newest segment)
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, PathBuf::from("main.rs"));
    // The content should be from seg1 (the newer one)
    assert!(result.files[0].lines[0].content.contains("goodbye"));
}

#[test]
fn test_search_segments_dedup_with_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // Segment 0 has main.rs
    let seg0 = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() { println!(\"hello\"); }\n".to_vec(),
            mtime: 100,
        }],
    );

    // Write tombstone for file 0 in segment 0
    let mut ts = TombstoneSet::new();
    ts.insert(FileId(0));
    ts.write_to(&seg0.dir_path().join("tombstones.bin")).unwrap();

    // Segment 1 has the updated main.rs
    let seg1 = build_segment(
        &base_dir,
        SegmentId(1),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() { println!(\"updated\"); }\n".to_vec(),
            mtime: 200,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
    let result = search_segments(&snapshot, "println").unwrap();

    // Only one result, from seg1
    assert_eq!(result.files.len(), 1);
    assert!(result.files[0].lines[0].content.contains("updated"));
}

#[test]
fn test_search_segments_empty_snapshot() {
    let snapshot: SegmentList = Arc::new(vec![]);
    let result = search_segments(&snapshot, "println").unwrap();
    assert_eq!(result.files.len(), 0);
    assert_eq!(result.total_count, 0);
}

#[test]
fn test_search_segments_sorted_by_score() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // File with many matches (high score)
    let seg0 = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "many.rs".to_string(),
            content: b"fn foo() {}\nfn foo() {}\nfn foo() {}\n".to_vec(),
            mtime: 0,
        }],
    );

    // File with one match in many lines (low score)
    let seg1 = build_segment(
        &base_dir,
        SegmentId(1),
        vec![InputFile {
            path: "few.rs".to_string(),
            content: b"line 1\nline 2\nline 3\nline 4\nline 5\nfn foo() {}\nline 7\nline 8\nline 9\nline 10\n".to_vec(),
            mtime: 0,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
    let result = search_segments(&snapshot, "foo").unwrap();
    assert_eq!(result.files.len(), 2);
    // many.rs should come first (higher score)
    assert_eq!(result.files[0].path, PathBuf::from("many.rs"));
    assert!(result.files[0].score >= result.files[1].score);
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_search_segments -v`

Expected: FAIL -- `search_segments` function does not exist.

### Step 3: Implement `search_segments`

Add to `multi_search.rs`, above the test module (after `search_single_segment`):

```rust
/// Search across multiple segments with snapshot isolation.
///
/// This is the main entry point for multi-segment queries. It:
/// 1. Takes a snapshot (`SegmentList`) and a query string
/// 2. For each segment: loads tombstones, runs `search_single_segment`
/// 3. Merges results: deduplicates by file path, preferring the newest segment
///    (highest `SegmentId`)
/// 4. Sorts by relevance score (descending)
/// 5. Returns a `SearchResult` with timing information
///
/// # Deduplication Strategy
///
/// When the same file path appears in multiple segments (e.g., a file was
/// modified and re-indexed), only the result from the segment with the highest
/// `SegmentId` is kept. This ensures callers see the most recent version.
///
/// # Edge Cases
///
/// - Empty snapshot: returns an empty `SearchResult`
/// - Query shorter than 3 chars: no trigrams can be extracted, returns empty
/// - All matches tombstoned: returns empty
pub fn search_segments(
    snapshot: &SegmentList,
    query: &str,
) -> Result<SearchResult, IndexError> {
    let start = Instant::now();

    if snapshot.is_empty() || query.len() < 3 {
        return Ok(SearchResult {
            total_count: 0,
            files: Vec::new(),
            duration: start.elapsed(),
        });
    }

    // Collect results from all segments, tagged with segment ID for dedup
    // Key: file path -> (segment_id, FileMatch)
    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();

    for segment in snapshot.iter() {
        let tombstones = segment.load_tombstones()?;
        let file_matches = search_single_segment(segment, query, &tombstones)?;

        for fm in file_matches {
            let seg_id = segment.segment_id();
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {
                    // Existing result is from a newer or same segment, keep it
                }
                _ => {
                    // This segment is newer, or path not seen yet
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
        }
    }

    // Extract FileMatch values and sort by score descending
    let mut files: Vec<FileMatch> = merged.into_values().map(|(_, fm)| fm).collect();
    files.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    let total_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_count,
        files,
        duration: start.elapsed(),
    })
}
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_search_segments -v`

Expected: PASS

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "feat(multi_search): implement search_segments with cross-segment dedup and merging"
```

---

## Task 7: Add `IndexState` integration test with `search_segments`

**Files:**
- Modify: `ferret-indexer-core/src/index_state.rs`

This test verifies the full end-to-end flow: build segments, publish them to `IndexState`, take a snapshot, and search it.

### Step 1: Write the integration test

Add to the `tests` module in `index_state.rs`:

```rust
use crate::multi_search::search_segments;
use std::path::PathBuf;

#[test]
fn test_index_state_search_integration() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg0 = build_test_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() {\n    println!(\"hello world\");\n}\n".to_vec(),
            mtime: 0,
        }],
    );

    let seg1 = build_test_segment(
        &base_dir,
        SegmentId(1),
        vec![InputFile {
            path: "lib.rs".to_string(),
            content: b"pub fn greet() {\n    println!(\"greetings\");\n}\n".to_vec(),
            mtime: 0,
        }],
    );

    let state = IndexState::new();
    state.publish(vec![seg0, seg1]);

    let snapshot = state.snapshot();
    let result = search_segments(&snapshot, "println").unwrap();

    assert_eq!(result.files.len(), 2);
    let paths: Vec<&str> = result.files.iter().map(|f| f.path.to_str().unwrap()).collect();
    assert!(paths.contains(&"main.rs"));
    assert!(paths.contains(&"lib.rs"));
    assert_eq!(result.total_count, 2);
}

#[test]
fn test_index_state_snapshot_isolation_during_search() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg0 = build_test_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() { println!(\"v1\"); }\n".to_vec(),
            mtime: 100,
        }],
    );

    let state = IndexState::new();
    state.publish(vec![seg0]);

    // Take snapshot before adding a new segment
    let snap_v1 = state.snapshot();

    let seg1 = build_test_segment(
        &base_dir,
        SegmentId(1),
        vec![InputFile {
            path: "extra.rs".to_string(),
            content: b"fn extra() { println!(\"new\"); }\n".to_vec(),
            mtime: 200,
        }],
    );

    // Publish updated list (both segments)
    let snap_current = state.snapshot();
    let mut new_list: Vec<Arc<Segment>> = snap_current.iter().cloned().collect();
    new_list.push(seg1);
    state.publish(new_list);

    // Search on the OLD snapshot should only find main.rs
    let result_v1 = search_segments(&snap_v1, "println").unwrap();
    assert_eq!(result_v1.files.len(), 1);
    assert_eq!(result_v1.files[0].path, PathBuf::from("main.rs"));

    // Search on a NEW snapshot should find both
    let snap_v2 = state.snapshot();
    let result_v2 = search_segments(&snap_v2, "println").unwrap();
    assert_eq!(result_v2.files.len(), 2);
}
```

### Step 2: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- index_state -v`

Expected: All tests PASS.

### Step 3: Commit

```bash
git add ferret-indexer-core/src/index_state.rs
git commit -m "test(index_state): add search integration and snapshot isolation tests"
```

---

## Task 8: Final verification and cleanup

### Step 1: Run full test suite

Run: `cargo test --workspace`

Expected: All tests pass (existing + new tests).

### Step 2: Run lints and formatting

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: No warnings, formatting OK. If formatting fails, run `cargo fmt --all` and commit the result.

### Step 3: Verify module structure

At this point, the new code should contain:

**`ferret-indexer-core/src/index_state.rs`:**
- `SegmentList` type alias (`Arc<Vec<Arc<Segment>>>`)
- `IndexState` struct with `Mutex<SegmentList>`
  - `IndexState::new()` -- creates with empty segment list
  - `snapshot()` -- returns `SegmentList` (cheap Arc clone, no locks for readers)
  - `publish(new_segments)` -- atomically swaps segment list (Mutex serializes writers)
- Tests for snapshot isolation, publish/replace, default trait

**`ferret-indexer-core/src/multi_search.rs`:**
- `verify_content_matches(content, query)` (private) -- byte-level substring match, returns `Vec<LineMatch>`
- `find_substring(haystack, needle)` (private) -- helper for byte-level search
- `search_single_segment(segment, query, tombstones)` (private) -- full pipeline for one segment
- `search_segments(snapshot, query)` (public) -- multi-segment search with dedup and merge
- Tests for verification, single-segment search, multi-segment dedup, tombstone filtering, score sorting

**`ferret-indexer-core/src/segment.rs`:**
- `load_tombstones()` method added to `Segment` struct
- Tests for empty and non-empty tombstone loading

**`ferret-indexer-core/src/lib.rs`:**
- New modules: `index_state`, `multi_search`
- New re-exports: `IndexState`, `SegmentList`, `search_segments`

### Step 4: Commit if any cleanup was needed

```bash
git add -A
git commit -m "chore: final cleanup for multi-segment query implementation"
```

---

## Reference: Existing APIs Used

These are the existing module APIs that the implementation calls. Do NOT modify any of these -- just call them:

| Module | API Used | Purpose |
|--------|----------|---------|
| `segment.rs` | `Segment::trigram_reader()` -> `&TrigramIndexReader` | Access trigram index for candidate search |
| `segment.rs` | `Segment::get_metadata(FileId)` -> `Option<FileMetadata>` | Get file path, language, content offset/len |
| `segment.rs` | `Segment::content_reader()` -> `&ContentStoreReader` | Access compressed content store |
| `segment.rs` | `Segment::segment_id()` -> `SegmentId` | Determine segment age for dedup |
| `segment.rs` | `Segment::dir_path()` -> `&Path` | Locate tombstones.bin |
| `tombstone.rs` | `TombstoneSet::new()` | Create empty tombstone set |
| `tombstone.rs` | `TombstoneSet::read_from(&path)` | Load tombstones from disk |
| `tombstone.rs` | `TombstoneSet::contains(FileId)` -> `bool` | Check if file is tombstoned |
| `intersection.rs` | `find_candidates(&TrigramIndexReader, &str)` -> `Vec<FileId>` | Trigram-based candidate file identification |
| `content.rs` | `ContentStoreReader::read_content(offset, len)` -> `Vec<u8>` | Decompress file content for verification |
| `search.rs` | `FileMatch`, `LineMatch`, `SearchResult` | Result types for search output |
| `types.rs` | `FileId(u32)`, `SegmentId(u32)`, `Language` | Core identifier types |
| `error.rs` | `IndexError` | Error type for all fallible operations |

## Reference: Concurrency Model

```
                          Writer Thread                    Reader Threads
                         ┌──────────────┐              ┌──────────────────┐
                         │              │              │ snapshot()        │
                         │ publish()    │              │   = Arc::clone()  │
                         │  lock Mutex  │              │   (no lock!)      │
                         │  swap Arc    │              │                   │
                         │  unlock      │              │ search_segments() │
                         │              │              │   uses snapshot   │
                         └──────────────┘              └──────────────────┘

  IndexState {
    current: Mutex<Arc<Vec<Arc<Segment>>>>
                    ^    ^
                    |    └── individual segments ref-counted,
                    |        can be shared across snapshots
                    └── outer Arc is the snapshot handle
  }
```

Old snapshots remain valid and usable even after a new `publish()` -- the `Arc` reference counting ensures segments are not dropped until all snapshots referencing them are gone.
