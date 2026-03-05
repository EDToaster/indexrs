# HHC-82: Candidate Ordering Heuristic (Smallest Files First) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Sort trigram candidates by file size (ascending) before verification so smaller files are checked first, improving time-to-first-result when combined with early termination.

**Architecture:** After `find_candidates()` (or `all_file_ids()`) returns file IDs, look up each candidate's `size_bytes` from the `MetadataReader` and sort ascending before entering the verification loop. This is done inside both `search_single_segment_with_context` and `search_single_segment_with_pattern` in `multi_search.rs`. To avoid deserializing full `FileMetadata` structs (which includes path string allocation) just for sorting, we add a lightweight `get_size_bytes()` method to `MetadataReader` that reads only the 4-byte `size_bytes` field at a known fixed offset in the entry.

**Tech Stack:** Rust, existing `MetadataReader` zero-copy mmap reader, no new dependencies.

---

### Task 1: Add `get_size_bytes()` to `MetadataReader`

**Files:**
- Modify: `ferret-indexer-core/src/metadata.rs` (add method + tests)

**Step 1: Write the failing test**

Add to the `mod tests` block in `metadata.rs`:

```rust
#[test]
fn test_reader_get_size_bytes() {
    let mut builder = MetadataBuilder::new();
    builder.add_file(make_entry(0, "small.rs", Language::Rust));   // size_bytes = 1000
    builder.add_file(make_entry(1, "medium.rs", Language::Rust));  // size_bytes = 1001
    builder.add_file(make_entry(2, "large.rs", Language::Rust));   // size_bytes = 1002

    let mut meta_buf = Vec::new();
    let mut paths_buf = Vec::new();
    builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

    let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();

    assert_eq!(reader.get_size_bytes(FileId(0)).unwrap(), Some(1000));
    assert_eq!(reader.get_size_bytes(FileId(1)).unwrap(), Some(1001));
    assert_eq!(reader.get_size_bytes(FileId(2)).unwrap(), Some(1002));
    assert_eq!(reader.get_size_bytes(FileId(99)).unwrap(), None);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_reader_get_size_bytes`
Expected: FAIL with compilation error (method does not exist)

**Step 3: Write minimal implementation**

Add this public method to `impl<'a> MetadataReader<'a>` in `metadata.rs`, after the existing `get()` method:

```rust
/// Look up only the `size_bytes` field for a file by its ID.
///
/// This is a lightweight alternative to [`get()`](Self::get) that avoids
/// deserializing the full entry (no path string allocation). Useful for
/// sorting candidates by file size before verification.
///
/// Uses O(1) direct indexing when file IDs are sequential (the common case).
pub fn get_size_bytes(&self, file_id: FileId) -> Result<Option<u32>, IndexError> {
    // Byte offset of size_bytes within a 58-byte entry: bytes [30..34]
    const SIZE_BYTES_OFFSET: usize = 30;

    // Fast path: direct indexing (O(1) when IDs are sequential)
    if file_id.0 < self.entry_count {
        let entry_offset = HEADER_SIZE + (file_id.0 as usize) * ENTRY_SIZE;
        let entry_data = &self.data[entry_offset..entry_offset + ENTRY_SIZE];
        let stored_id = u32::from_le_bytes(entry_data[0..4].try_into().unwrap());
        if stored_id == file_id.0 {
            let size = u32::from_le_bytes(
                entry_data[SIZE_BYTES_OFFSET..SIZE_BYTES_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            );
            return Ok(Some(size));
        }
    }

    // Slow path: linear scan for non-contiguous IDs
    for i in 0..self.entry_count {
        let entry_offset = HEADER_SIZE + (i as usize) * ENTRY_SIZE;
        let entry_data = &self.data[entry_offset..entry_offset + ENTRY_SIZE];
        let stored_id = u32::from_le_bytes(entry_data[0..4].try_into().unwrap());
        if stored_id == file_id.0 {
            let size = u32::from_le_bytes(
                entry_data[SIZE_BYTES_OFFSET..SIZE_BYTES_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            );
            return Ok(Some(size));
        }
    }

    Ok(None)
}
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_reader_get_size_bytes`
Expected: PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/metadata.rs
git commit -m "feat(metadata): add get_size_bytes() for lightweight file size lookup"
```

---

### Task 2: Add `get_size_bytes()` to `Segment`

**Files:**
- Modify: `ferret-indexer-core/src/segment.rs` (add delegating method)

**Step 1: Write the failing test**

Add to the `mod tests` block in `segment.rs`:

```rust
#[test]
fn test_segment_get_size_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    fs::create_dir_all(&base_dir).unwrap();

    let files = vec![
        InputFile {
            path: "small.rs".to_string(),
            content: b"ab".to_vec(), // 2 bytes
            mtime: 0,
        },
        InputFile {
            path: "large.rs".to_string(),
            content: vec![b'x'; 5000], // 5000 bytes
            mtime: 0,
        },
    ];

    let writer = SegmentWriter::new(&base_dir, SegmentId(0));
    let segment = writer.build(files).unwrap();

    assert_eq!(segment.get_size_bytes(FileId(0)).unwrap(), Some(2));
    assert_eq!(segment.get_size_bytes(FileId(1)).unwrap(), Some(5000));
    assert_eq!(segment.get_size_bytes(FileId(99)).unwrap(), None);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_segment_get_size_bytes`
Expected: FAIL with compilation error (method does not exist)

**Step 3: Write minimal implementation**

Add this method to `impl Segment` in `segment.rs`, after `get_metadata()`:

```rust
/// Look up only the `size_bytes` field for a file by its ID.
///
/// Lightweight alternative to [`get_metadata()`](Self::get_metadata) —
/// avoids deserializing the full entry. Used for candidate ordering
/// (sort by file size before verification).
pub fn get_size_bytes(&self, file_id: FileId) -> Result<Option<u32>, IndexError> {
    let reader = MetadataReader::new(&self.meta_mmap, &self.paths_mmap)?;
    reader.get_size_bytes(file_id)
}
```

**Step 4: Run test to verify it passes**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_segment_get_size_bytes`
Expected: PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/segment.rs
git commit -m "feat(segment): add get_size_bytes() delegate for lightweight size lookup"
```

---

### Task 3: Add candidate ordering helper function

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs` (add helper + tests)

**Step 1: Write the failing test**

Add to the `mod tests` block in `multi_search.rs`:

```rust
#[test]
fn test_sort_candidates_by_size() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // Create files of varying sizes; IDs assigned in order: 0=medium, 1=small, 2=large
    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![
            InputFile {
                path: "medium.rs".to_string(),
                content: vec![b'x'; 500],
                mtime: 0,
            },
            InputFile {
                path: "small.rs".to_string(),
                content: vec![b'x'; 100],
                mtime: 0,
            },
            InputFile {
                path: "large.rs".to_string(),
                content: vec![b'x'; 2000],
                mtime: 0,
            },
        ],
    );

    let candidates = vec![FileId(0), FileId(1), FileId(2)];
    let sorted = sort_candidates_by_size(&seg, candidates);

    // Should be ordered: small(1, 100B), medium(0, 500B), large(2, 2000B)
    assert_eq!(sorted, vec![FileId(1), FileId(0), FileId(2)]);
}

#[test]
fn test_sort_candidates_by_size_preserves_order_for_equal_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // Two files with the same size
    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![
            InputFile {
                path: "a.rs".to_string(),
                content: vec![b'x'; 200],
                mtime: 0,
            },
            InputFile {
                path: "b.rs".to_string(),
                content: vec![b'x'; 200],
                mtime: 0,
            },
        ],
    );

    let candidates = vec![FileId(0), FileId(1)];
    let sorted = sort_candidates_by_size(&seg, candidates);

    // Stable sort: original order preserved for equal sizes
    assert_eq!(sorted, vec![FileId(0), FileId(1)]);
}

#[test]
fn test_sort_candidates_by_size_empty() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() {}".to_vec(),
            mtime: 0,
        }],
    );

    let sorted = sort_candidates_by_size(&seg, vec![]);
    assert!(sorted.is_empty());
}
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_sort_candidates_by_size`
Expected: FAIL with compilation error (function does not exist)

**Step 3: Write minimal implementation**

Add this function to `multi_search.rs` (above the `search_single_segment_with_context` function):

```rust
/// Sort candidate file IDs by file size (ascending) for faster verification.
///
/// Smaller files verify faster (less data to decompress and scan) and are
/// more likely to be human-written source code. Combined with early
/// termination, this means the first N results come back much faster.
///
/// Uses a stable sort so equal-size files retain their original order.
/// Falls back to `u32::MAX` for any file ID whose size cannot be looked up
/// (pushes unknown entries to the end).
fn sort_candidates_by_size(segment: &Segment, mut candidates: Vec<FileId>) -> Vec<FileId> {
    if candidates.len() <= 1 {
        return candidates;
    }

    // Build a size lookup: for each candidate, read just size_bytes.
    // This is cheap — O(1) per candidate via direct mmap indexing.
    let sizes: Vec<u32> = candidates
        .iter()
        .map(|fid| {
            segment
                .get_size_bytes(*fid)
                .ok()
                .flatten()
                .unwrap_or(u32::MAX)
        })
        .collect();

    // Sort candidates by their corresponding size (stable sort preserves
    // original order for equal sizes).
    let mut indices: Vec<usize> = (0..candidates.len()).collect();
    indices.sort_by_key(|&i| sizes[i]);

    let sorted: Vec<FileId> = indices.into_iter().map(|i| candidates[i]).collect();
    sorted
}
```

Note: We use an index-based sort rather than `sort_by_key` directly on `candidates` because we pre-compute sizes to avoid repeated mmap lookups during the sort's comparisons.

**Step 4: Run test to verify it passes**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_sort_candidates_by_size`
Expected: PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "feat(multi_search): add sort_candidates_by_size helper"
```

---

### Task 4: Wire candidate ordering into `search_single_segment_with_context`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs` (two lines added to existing function)

**Step 1: Write the failing test**

Add to the `mod tests` block in `multi_search.rs`:

```rust
#[test]
fn test_search_single_segment_prefers_smaller_files() {
    // Verify that with early termination (max_file_results=1), the smaller file
    // is returned first (proving candidates were sorted by size).
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // File 0 is large (5000 bytes), file 1 is small (30 bytes).
    // Both contain the query "fn main".
    // The trigram intersection returns [FileId(0), FileId(1)] in natural order.
    // With candidate ordering, FileId(1) should be checked first.
    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![
            InputFile {
                path: "large.rs".to_string(),
                content: {
                    let mut c = b"fn main() { /* large */ }".to_vec();
                    c.extend(vec![b' '; 5000]);
                    c
                },
                mtime: 0,
            },
            InputFile {
                path: "small.rs".to_string(),
                content: b"fn main() { /* small */ }".to_vec(),
                mtime: 0,
            },
        ],
    );

    let tombstones = TombstoneSet::new();
    // Request only 1 result — with ordering, the small file should be returned
    let results =
        search_single_segment_with_context(&seg, "fn main", &tombstones, 0, Some(1)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].path,
        PathBuf::from("small.rs"),
        "smaller file should be verified first and returned under early termination"
    );
}
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_search_single_segment_prefers_smaller_files`
Expected: FAIL — currently returns "large.rs" because FileId(0) comes first in candidate order.

**Step 3: Wire ordering into `search_single_segment_with_context`**

In `search_single_segment_with_context`, change:

```rust
let candidates = find_candidates(segment.trigram_reader(), query)?;
```

to:

```rust
let candidates = find_candidates(segment.trigram_reader(), query)?;
let candidates = sort_candidates_by_size(segment, candidates);
```

**Step 4: Run tests to verify they pass**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_search_single_segment_prefers_smaller_files`
Expected: PASS

Also run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core`
Expected: All existing tests PASS (ordering doesn't break correctness, only changes verification order).

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "feat(multi_search): sort candidates by file size in search_single_segment_with_context"
```

---

### Task 5: Wire candidate ordering into `search_single_segment_with_pattern`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs` (two lines added to existing function)

**Step 1: Write the failing test**

Add to the `mod tests` block in `multi_search.rs`:

```rust
#[test]
fn test_search_single_segment_with_pattern_prefers_smaller_files() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![
            InputFile {
                path: "large.rs".to_string(),
                content: {
                    let mut c = b"fn main() { /* large */ }".to_vec();
                    c.extend(vec![b' '; 5000]);
                    c
                },
                mtime: 0,
            },
            InputFile {
                path: "small.rs".to_string(),
                content: b"fn main() { /* small */ }".to_vec(),
                mtime: 0,
            },
        ],
    );

    let tombstones = TombstoneSet::new();
    let pattern = MatchPattern::Literal("fn main".to_string());
    let results =
        search_single_segment_with_pattern(&seg, &pattern, &tombstones, 0, Some(1)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].path,
        PathBuf::from("small.rs"),
        "smaller file should be verified first with pattern search too"
    );
}
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_search_single_segment_with_pattern_prefers_smaller_files`
Expected: FAIL — returns "large.rs"

**Step 3: Wire ordering into `search_single_segment_with_pattern`**

In `search_single_segment_with_pattern`, after the `candidates` variable is assigned (the `if/else` block ending around line 383), add:

```rust
let candidates = sort_candidates_by_size(segment, candidates);
```

**Step 4: Run tests to verify they pass**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core -- test_search_single_segment_with_pattern_prefers_smaller_files`
Expected: PASS

Also run: `cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering && cargo test -p ferret-indexer-core`
Expected: All tests PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "feat(multi_search): sort candidates by file size in search_single_segment_with_pattern"
```

---

### Task 6: Final verification

**Step 1: Run full workspace checks**

```bash
cd /Users/howard/src/ferret/.claude/worktrees/hhc-82-ordering
cargo check --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

Expected: All pass with no warnings or errors.

**Step 2: Fix any issues**

If clippy or fmt reports issues, fix them and re-run.

**Step 3: Squash into a clean commit (if desired)**

If there were fixup commits, optionally squash. Otherwise, the individual task commits are fine.

---

## Design Notes

### Why sort by size?

1. **Smaller files verify faster** — less data to decompress (zstd) and scan for matches.
2. **Smaller files are more likely to be human-written source** — generated files, vendored deps, and data blobs tend to be large.
3. **Synergy with early termination** — when `max_results` limits how many files are verified, checking small files first means the budget is used on the cheapest-to-verify candidates.

### Why a lightweight `get_size_bytes()` instead of `get_metadata()`?

Each `get_metadata()` call allocates a `String` for the path (via `from_utf8` + `to_string`). When sorting hundreds or thousands of candidates, these allocations add up. `get_size_bytes()` reads 4 bytes from a fixed offset in the mmap — zero allocation, O(1).

### Why not also deprioritize vendor/node_modules paths?

The ticket mentions this as optional. Skipping it for now because:
- It requires reading the path string (allocation) for each candidate, negating the `get_size_bytes()` optimization.
- Large vendored files already sort to the end by size.
- Path-based deprioritization could be added later as a separate refinement if needed.
