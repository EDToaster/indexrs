# Reindex Performance Optimization Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make `apply_changes` / `apply_changes_with_progress` comparable in speed to a full `init` by fixing three bottlenecks: O(N×M) tombstone lookup, sequential file reads, and single-threaded segment building.

**Architecture:** Build a per-segment path→FileId HashMap at lookup time (replacing the linear scan), parallelize file reading with rayon, and use budget-based multi-segment building. All changes are internal to `segment_manager.rs` with a small helper added to `MetadataReader`. Existing `apply_changes` tests provide regression coverage; new tests validate each optimization independently.

**Tech Stack:** Rust, rayon (already in deps), `std::collections::HashMap`

---

## Background

Currently `apply_changes_with_progress` has three performance problems vs `init`:

1. **`find_file_in_segments` is O(changes × total_indexed_files)** — for each change needing tombstoning, it does a full linear scan of every metadata entry in every segment. With 5000 changes and 30000 files = 150M string comparisons.

2. **File reads are sequential** — a plain `for` loop reads files one at a time. `init` uses `rayon::par_iter()`.

3. **Single segment build** — all new files go into one `SegmentWriter::build()` call. `init` uses `index_files_with_progress` which builds multiple segments in parallel with budget-based batching.

The fix refactors `apply_changes` into a three-phase pipeline that mirrors `init`:
- Phase 1: Build path→FileId index per segment, compute tombstones (O(1) per lookup)
- Phase 2: Read and filter files in parallel (rayon)
- Phase 3: Build segments with budget-based batching and parallel rayon builds

---

### Task 1: Add `find_file_id_by_path` to `MetadataReader`

The zero-copy `MetadataReader` currently only has `get(file_id)` and `iter_all()`. We need a lightweight path-based lookup that avoids deserializing the full entry — just extract path bytes and compare, returning the `FileId` on match.

**Files:**
- Modify: `ferret-indexer-core/src/metadata.rs` (add method + tests)

**Step 1: Write the failing test**

Add at the bottom of the `mod tests` block in `metadata.rs`:

```rust
#[test]
fn test_reader_find_file_id_by_path() {
    let mut builder = MetadataBuilder::new();
    builder.add_file(make_entry(0, "src/main.rs", Language::Rust));
    builder.add_file(make_entry(1, "src/lib.rs", Language::Rust));
    builder.add_file(make_entry(2, "README.md", Language::Markdown));

    let (meta_buf, paths_buf) = write_to_buffers(&builder);
    let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();

    assert_eq!(reader.find_file_id_by_path("src/main.rs"), Some(FileId(0)));
    assert_eq!(reader.find_file_id_by_path("src/lib.rs"), Some(FileId(1)));
    assert_eq!(reader.find_file_id_by_path("README.md"), Some(FileId(2)));
    assert_eq!(reader.find_file_id_by_path("nonexistent.rs"), None);
    assert_eq!(reader.find_file_id_by_path(""), None);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_reader_find_file_id_by_path`
Expected: FAIL — method does not exist

**Step 3: Write the implementation**

Add to `impl<'a> MetadataReader<'a>` in `metadata.rs`, after `iter_all()`:

```rust
/// Find the `FileId` for a given path by scanning entries.
///
/// Compares path bytes directly against the paths pool without allocating
/// a `String`, making it faster than `iter_all()` + filter for single lookups.
/// Returns `None` if no entry matches.
pub fn find_file_id_by_path(&self, path: &str) -> Option<FileId> {
    let needle = path.as_bytes();
    for i in 0..self.entry_count {
        let offset = HEADER_SIZE + (i as usize) * ENTRY_SIZE;
        let entry_data = &self.data[offset..offset + ENTRY_SIZE];

        let path_offset = u32::from_le_bytes(entry_data[4..8].try_into().unwrap()) as usize;
        let path_len = u32::from_le_bytes(entry_data[8..12].try_into().unwrap()) as usize;

        if path_len != needle.len() {
            continue;
        }
        let path_end = path_offset + path_len;
        if path_end > self.paths.len() {
            continue;
        }
        if &self.paths[path_offset..path_end] == needle {
            let file_id = u32::from_le_bytes(entry_data[0..4].try_into().unwrap());
            return Some(FileId(file_id));
        }
    }
    None
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-indexer-core -- test_reader_find_file_id_by_path`
Expected: PASS

**Step 5: Commit**

```
feat(core): add find_file_id_by_path to MetadataReader
```

---

### Task 2: Replace O(N×M) `find_file_in_segments` with HashMap-based batch lookup

The current `find_file_in_segments` is called once per change, each time scanning all entries in all segments. Replace it with a batch approach: build a HashMap<path, Vec<(seg_idx, FileId)>> for all paths-to-tombstone in one pass over the segments.

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs` (replace `find_file_in_segments` + add test)

**Step 1: Write the failing test**

Add a new test to the `mod tests` block in `segment_manager.rs`:

```rust
#[test]
fn test_batch_find_files_in_segments() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");

    let manager = SegmentManager::new(&base_dir).unwrap();

    // Build two segments with known files
    manager
        .index_files(vec![
            InputFile {
                path: "a.rs".to_string(),
                content: b"fn a() {}".to_vec(),
                mtime: 100,
            },
            InputFile {
                path: "b.rs".to_string(),
                content: b"fn b() {}".to_vec(),
                mtime: 100,
            },
        ])
        .unwrap();
    manager
        .index_files(vec![InputFile {
            path: "c.rs".to_string(),
            content: b"fn c() {}".to_vec(),
            mtime: 100,
        }])
        .unwrap();

    let snap = manager.snapshot();
    let paths: std::collections::HashSet<String> =
        ["a.rs", "c.rs", "missing.rs"].iter().map(|s| s.to_string()).collect();

    let result = SegmentManager::batch_find_files_in_segments(&snap, &paths);

    // a.rs is in segment 0, c.rs is in segment 1, missing.rs not found
    assert!(result.contains_key("a.rs"));
    assert!(result.contains_key("c.rs"));
    assert!(!result.contains_key("missing.rs"));

    let a_locs = &result["a.rs"];
    assert_eq!(a_locs.len(), 1);
    assert_eq!(a_locs[0].0, 0); // segment index 0

    let c_locs = &result["c.rs"];
    assert_eq!(c_locs.len(), 1);
    assert_eq!(c_locs[0].0, 1); // segment index 1
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_batch_find_files_in_segments`
Expected: FAIL — method does not exist

**Step 3: Write the implementation**

Replace the existing `find_file_in_segments` method with a new `batch_find_files_in_segments` method in `segment_manager.rs`:

```rust
/// Build a map from path → [(segment_index, file_id)] for a set of paths.
///
/// Scans each segment's metadata once, collecting FileIds for all requested
/// paths in a single pass. This is O(segments × entries_per_segment) regardless
/// of how many paths are queried, vs the previous approach which was
/// O(query_paths × total_entries).
fn batch_find_files_in_segments(
    segments: &[Arc<Segment>],
    paths: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, Vec<(usize, FileId)>> {
    let mut result: std::collections::HashMap<String, Vec<(usize, FileId)>> =
        std::collections::HashMap::new();

    for (seg_idx, segment) in segments.iter().enumerate() {
        let reader = segment.metadata_reader();
        let tombstones = segment.load_tombstones().unwrap_or_default();

        for entry in reader.iter_all() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if paths.contains(&entry.path) && !tombstones.contains(entry.file_id) {
                result
                    .entry(entry.path)
                    .or_default()
                    .push((seg_idx, entry.file_id));
            }
        }
    }

    result
}
```

Now update both `apply_changes` and `apply_changes_with_progress` to use the batch approach. Replace the per-change tombstone lookup loop. In both methods, change the loop body from calling `find_file_in_segments` per change to:

1. Before the loop: collect all paths that need tombstoning into a `HashSet<String>`.
2. Call `batch_find_files_in_segments` once.
3. In the loop: look up tombstone locations from the HashMap instead of scanning segments.

Here is the refactored structure for `apply_changes` (the same pattern applies to `apply_changes_with_progress`):

```rust
// Collect paths that need tombstoning
let tombstone_paths: std::collections::HashSet<String> = changes
    .iter()
    .filter(|c| tombstone::needs_tombstone(&c.kind))
    .map(|c| c.path.to_string_lossy().to_string())
    .collect();

// Batch lookup: one pass over all segments
let tombstone_locations = Self::batch_find_files_in_segments(&current_segments, &tombstone_paths);

// Build tombstone updates from batch results
let mut tombstone_updates: std::collections::HashMap<usize, TombstoneSet> =
    std::collections::HashMap::new();
for (_path, locations) in &tombstone_locations {
    for &(seg_idx, file_id) in locations {
        tombstone_updates
            .entry(seg_idx)
            .or_default()
            .insert(file_id);
    }
}

// Collect new files (same loop, but without the tombstone lookup)
let mut new_files: Vec<InputFile> = Vec::new();
for change in changes {
    if tombstone::needs_new_entry(&change.kind) {
        // ... same path validation, fs::read, should_index_file logic ...
    }
}
```

**Step 4: Run tests to verify everything passes**

Run: `cargo test -p ferret-indexer-core -- test_batch_find_files_in_segments test_apply_changes`
Expected: ALL PASS — batch lookup produces identical results

**Step 5: Run full test suite for regressions**

Run: `cargo test --workspace`
Expected: ALL PASS

**Step 6: Commit**

```
perf(core): replace O(N×M) tombstone lookup with batch HashMap scan
```

---

### Task 3: Parallelize file reading in `apply_changes_with_progress`

The file reading loop currently runs sequentially. Refactor it to use `rayon::par_iter()` for the I/O-bound file reading and filtering phase, then collect results. Progress reporting moves to after the parallel phase.

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs` (both `apply_changes` and `apply_changes_with_progress`)

**Step 1: Write the failing test (performance regression test)**

Add a test that verifies `apply_changes` handles a moderate batch correctly (ensures the parallel refactor doesn't break anything):

```rust
#[test]
fn test_apply_changes_bulk_creates() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();

    let manager = SegmentManager::new(&base_dir).unwrap();

    // Create 50 files on disk and corresponding change events
    let mut changes = Vec::new();
    for i in 0..50 {
        let name = format!("file_{i:03}.rs");
        fs::write(repo_dir.join(&name), format!("fn func_{i}() {{}}")).unwrap();
        changes.push(ChangeEvent {
            path: PathBuf::from(name),
            kind: ChangeKind::Created,
        });
    }

    manager.apply_changes(&repo_dir, &changes).unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].entry_count(), 50);
}

#[test]
fn test_apply_changes_bulk_mixed() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();

    let manager = SegmentManager::new(&base_dir).unwrap();

    // Pre-index 20 files
    let mut initial_files = Vec::new();
    for i in 0..20 {
        let name = format!("existing_{i:03}.rs");
        let content = format!("fn existing_{i}() {{}}");
        fs::write(repo_dir.join(&name), &content).unwrap();
        initial_files.push(InputFile {
            path: name,
            content: content.into_bytes(),
            mtime: 100,
        });
    }
    manager.index_files(initial_files).unwrap();

    // Now: modify 10, delete 5, create 15
    let mut changes = Vec::new();
    for i in 0..10 {
        let name = format!("existing_{i:03}.rs");
        fs::write(repo_dir.join(&name), format!("fn updated_{i}() {{}}")).unwrap();
        changes.push(ChangeEvent {
            path: PathBuf::from(name),
            kind: ChangeKind::Modified,
        });
    }
    for i in 10..15 {
        changes.push(ChangeEvent {
            path: PathBuf::from(format!("existing_{i:03}.rs")),
            kind: ChangeKind::Deleted,
        });
    }
    for i in 0..15 {
        let name = format!("new_{i:03}.rs");
        fs::write(repo_dir.join(&name), format!("fn new_{i}() {{}}")).unwrap();
        changes.push(ChangeEvent {
            path: PathBuf::from(name),
            kind: ChangeKind::Created,
        });
    }

    manager.apply_changes(&repo_dir, &changes).unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 2); // original + new

    // 15 tombstoned in original segment (10 modified + 5 deleted)
    let ts = snap[0].load_tombstones().unwrap();
    assert_eq!(ts.len(), 15);

    // New segment: 10 modified + 15 created = 25 files
    assert_eq!(snap[1].entry_count(), 25);
}
```

**Step 2: Run tests to confirm they pass with current code (baseline)**

Run: `cargo test -p ferret-indexer-core -- test_apply_changes_bulk`
Expected: PASS (these test correctness, not speed)

**Step 3: Refactor the file-reading loop to use rayon**

In both `apply_changes` and `apply_changes_with_progress`, replace the sequential file-reading loop with a parallel `par_iter()` + `filter_map()`. The pattern:

```rust
// Collect changes that need new entries
let new_file_changes: Vec<&ChangeEvent> = changes
    .iter()
    .filter(|c| tombstone::needs_new_entry(&c.kind))
    .collect();

// Parallel file reading and filtering
let new_files: Vec<InputFile> = new_file_changes
    .par_iter()
    .filter_map(|change| {
        let has_dotdot = change
            .path
            .components()
            .any(|c| c == std::path::Component::ParentDir);
        if has_dotdot || change.path.is_absolute() {
            tracing::warn!(
                path = %change.path.display(),
                "skipping change with potentially unsafe path"
            );
            return None;
        }

        let path_str = change.path.to_string_lossy().to_string();
        let full_path = repo_dir.join(&change.path);
        if !full_path.is_file() {
            return None;
        }
        let content = match fs::read(&full_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %full_path.display(), error = %e, "skipping file: read error");
                return None;
            }
        };

        if !crate::binary::should_index_file(&full_path, &content, 1_048_576) {
            return None;
        }

        let mtime = full_path
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            })
            .unwrap_or(0);
        Some(InputFile {
            path: path_str,
            content,
            mtime,
        })
    })
    .collect();
```

For `apply_changes_with_progress`, emit progress events for the file-reading phase using an `AtomicUsize` counter inside the `par_iter` closure (same pattern as `init.rs`):

```rust
let files_done = AtomicUsize::new(0);
let total_to_read = new_file_changes.len();

let new_files: Vec<InputFile> = new_file_changes
    .par_iter()
    .filter_map(|change| {
        // ... same logic as above ...

        let current = files_done.fetch_add(1, Ordering::Relaxed) + 1;
        on_progress(ReindexProgress::PreparingFiles {
            current,
            total: total_to_read,
        });
        Some(InputFile { path: path_str, content, mtime })
    })
    .collect();
```

Note: `on_progress` must be `Fn` (not `FnMut`) and `Send + Sync` — it already is, since the signature requires `Fn(...) + Send + Sync`.

**Important:** In this refactored version, `fs::read` errors are logged and skipped (returning `None`) rather than propagating with `?`. This is intentional — a single unreadable file should not abort the entire reindex of thousands of files.

**Step 4: Run all existing tests**

Run: `cargo test -p ferret-indexer-core -- test_apply_changes`
Expected: ALL PASS

**Step 5: Commit**

```
perf(core): parallelize file reading in apply_changes with rayon
```

---

### Task 4: Add budget-based multi-segment building to `apply_changes_with_progress`

Currently both `apply_changes` methods build a single segment for all new files. For large batches (thousands of files), this means a single-threaded build. Refactor to use the same budget-based batching + parallel rayon builds as `index_files_with_progress`.

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_apply_changes_large_batch_creates_multiple_segments() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();

    let manager = SegmentManager::new(&base_dir).unwrap();

    // Pre-index one file so we have an existing segment
    manager
        .index_files(vec![InputFile {
            path: "old.rs".to_string(),
            content: b"fn old() {}".to_vec(),
            mtime: 100,
        }])
        .unwrap();

    // Create many files with enough content to exceed a 1KB budget
    // (we'll use a tiny budget in the test to force splitting)
    let mut changes = Vec::new();
    for i in 0..20 {
        let name = format!("big_{i:03}.rs");
        // ~100 bytes each, 20 files = ~2KB total
        let content = format!("fn big_{i}() {{ let x = \"{}\"; }}", "a".repeat(80));
        fs::write(repo_dir.join(&name), &content).unwrap();
        changes.push(ChangeEvent {
            path: PathBuf::from(name),
            kind: ChangeKind::Created,
        });
    }

    // Use apply_changes_with_budget with a tiny budget to force multi-segment
    manager
        .apply_changes_with_budget(&repo_dir, &changes, 500)
        .unwrap();

    let snap = manager.snapshot();
    // Should have more than 2 segments (1 original + multiple new)
    assert!(
        snap.len() > 2,
        "expected >2 segments with 500B budget, got {}",
        snap.len()
    );

    // Total entry count across new segments should be 20
    let total_new_entries: u32 = snap[1..].iter().map(|s| s.entry_count()).sum();
    assert_eq!(total_new_entries, 20);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_apply_changes_large_batch_creates_multiple_segments`
Expected: FAIL — method `apply_changes_with_budget` does not exist

**Step 3: Write the implementation**

Add a new `apply_changes_with_budget` method that accepts a `max_segment_bytes` parameter. Refactor `apply_changes` to call it with `DEFAULT_COMPACTION_BUDGET`, and `apply_changes_with_progress` to call its own version with the budget + progress.

The segment-building phase (currently a single `writer.build(new_files)?`) becomes:

```rust
// Split files into budget-sized batches
let mut batches: Vec<Vec<InputFile>> = Vec::new();
let mut batch: Vec<InputFile> = Vec::new();
let mut batch_bytes: usize = 0;

for file in new_files {
    let content_len = file.content.len();
    batch.push(file);
    batch_bytes += content_len;
    if max_segment_bytes > 0 && batch_bytes > max_segment_bytes {
        batches.push(std::mem::take(&mut batch));
        batch_bytes = 0;
    }
}
if !batch.is_empty() {
    batches.push(batch);
}

// Pre-allocate segment IDs
let id_batches: Vec<(SegmentId, Vec<InputFile>)> = batches
    .into_iter()
    .map(|b| self.next_segment_id().map(|id| (id, b)))
    .collect::<Result<Vec<_>, _>>()?;

// Build segments in parallel
let results: Vec<Result<Arc<Segment>, IndexError>> = id_batches
    .into_par_iter()
    .map(|(seg_id, files)| {
        let writer = SegmentWriter::new(&self.segments_dir, seg_id);
        writer.build(files).map(Arc::new)
    })
    .collect();

let new_segments: Vec<Arc<Segment>> = results
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;

updated_segments.extend(new_segments);
```

For `apply_changes_with_progress`, use `build_with_progress` with an `AtomicUsize` counter for progress reporting (same pattern as `index_files_with_progress`).

**Step 4: Run tests**

Run: `cargo test -p ferret-indexer-core -- test_apply_changes`
Expected: ALL PASS

**Step 5: Commit**

```
perf(core): add budget-based parallel segment building to apply_changes
```

---

### Task 5: Write comprehensive regression tests

Add tests that exercise edge cases and verify the optimizations don't break correctness.

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs` (add tests)

**Step 1: Write the tests**

```rust
#[test]
fn test_apply_changes_skips_directories() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(repo_dir.join("subdir")).unwrap();

    let manager = SegmentManager::new(&base_dir).unwrap();

    // A change pointing to a directory should be skipped, not error
    let changes = vec![
        ChangeEvent {
            path: PathBuf::from("subdir"),
            kind: ChangeKind::Modified,
        },
        ChangeEvent {
            path: PathBuf::from("subdir"),
            kind: ChangeKind::Created,
        },
    ];

    manager.apply_changes(&repo_dir, &changes).unwrap();

    let snap = manager.snapshot();
    // No segments should be created — directory was skipped
    assert_eq!(snap.len(), 0);
}

#[test]
fn test_apply_changes_skips_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();

    let manager = SegmentManager::new(&base_dir).unwrap();

    // A Created change for a file that doesn't exist on disk
    let changes = vec![ChangeEvent {
        path: PathBuf::from("ghost.rs"),
        kind: ChangeKind::Created,
    }];

    manager.apply_changes(&repo_dir, &changes).unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 0);
}

#[test]
fn test_apply_changes_file_in_multiple_segments() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();

    let manager = SegmentManager::new(&base_dir).unwrap();

    // Index the same file in two segments (simulates modify without compaction)
    manager
        .index_files(vec![InputFile {
            path: "shared.rs".to_string(),
            content: b"fn v1() {}".to_vec(),
            mtime: 100,
        }])
        .unwrap();
    // Tombstone v1, add v2
    fs::write(repo_dir.join("shared.rs"), b"fn v2() {}").unwrap();
    manager
        .apply_changes(
            &repo_dir,
            &[ChangeEvent {
                path: PathBuf::from("shared.rs"),
                kind: ChangeKind::Modified,
            }],
        )
        .unwrap();

    // Now modify again — should tombstone the entry in segment 1 (v2)
    fs::write(repo_dir.join("shared.rs"), b"fn v3() {}").unwrap();
    manager
        .apply_changes(
            &repo_dir,
            &[ChangeEvent {
                path: PathBuf::from("shared.rs"),
                kind: ChangeKind::Modified,
            }],
        )
        .unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 3);

    // Segment 0: v1 tombstoned
    assert!(snap[0].load_tombstones().unwrap().contains(FileId(0)));
    // Segment 1: v2 tombstoned
    assert!(snap[1].load_tombstones().unwrap().contains(FileId(0)));
    // Segment 2: v3 alive
    let ts2 = snap[2].load_tombstones().unwrap();
    assert!(!ts2.contains(FileId(0)));
}

#[test]
fn test_apply_changes_with_progress_skips_directories() {
    use crate::reindex_progress::ReindexProgress;

    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path();
    let ferret_dir = repo_dir.join(".ferret_index");
    fs::create_dir_all(ferret_dir.join("segments")).unwrap();
    fs::create_dir_all(repo_dir.join("a_dir")).unwrap();

    let manager = SegmentManager::new(&ferret_dir).unwrap();

    let changes = vec![ChangeEvent {
        path: PathBuf::from("a_dir"),
        kind: ChangeKind::Created,
    }];

    let events = std::sync::Mutex::new(Vec::new());
    manager
        .apply_changes_with_progress(repo_dir, &changes, |ev| {
            events.lock().unwrap().push(ev);
        })
        .unwrap();

    // Should not have created any segments
    let snap = manager.snapshot();
    assert_eq!(snap.len(), 0);
}

#[test]
fn test_apply_changes_empty_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();

    let manager = SegmentManager::new(&base_dir).unwrap();

    manager.apply_changes(&repo_dir, &[]).unwrap();
    assert_eq!(manager.snapshot().len(), 0);
}

#[test]
fn test_apply_changes_skips_binary_files() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();

    // Write a binary file (contains null bytes)
    fs::write(repo_dir.join("image.png"), b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
    // Write a normal text file
    fs::write(repo_dir.join("code.rs"), b"fn main() {}").unwrap();

    let manager = SegmentManager::new(&base_dir).unwrap();
    let changes = vec![
        ChangeEvent {
            path: PathBuf::from("image.png"),
            kind: ChangeKind::Created,
        },
        ChangeEvent {
            path: PathBuf::from("code.rs"),
            kind: ChangeKind::Created,
        },
    ];

    manager.apply_changes(&repo_dir, &changes).unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 1);
    // Only code.rs should be indexed
    assert_eq!(snap[0].entry_count(), 1);
    let meta = snap[0].get_metadata(FileId(0)).unwrap().unwrap();
    assert_eq!(meta.path, "code.rs");
}
```

**Step 2: Run all tests**

Run: `cargo test -p ferret-indexer-core -- test_apply_changes`
Expected: ALL PASS

**Step 3: Run full workspace tests + clippy**

Run: `cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: ALL PASS, no warnings

**Step 4: Commit**

```
test(core): add comprehensive regression tests for apply_changes
```

---

### Task 6: Final verification

**Step 1: Run full CI checks**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

Expected: ALL PASS

**Step 2: Manual smoke test**

```bash
cargo run -p ferret-indexer-cli -- reindex --repo ~/carrot
```

Expected: Progress bar appears promptly after "Found N changes", reindex completes.
