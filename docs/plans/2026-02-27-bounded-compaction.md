# Bounded-Memory Compaction Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the current `compact()` which loads all live entries into memory with a bounded variant that merges N segments into M output segments, each respecting a configurable memory budget.

**Architecture:** Instead of collecting every `InputFile` into a single `Vec`, we stream entries from input segments and flush a new output segment whenever the accumulated content size exceeds the budget. The existing `SegmentWriter::build()` is reused for each output batch. The public API stays backward-compatible — `compact()` gains an internal budget, and a new `compact_with_budget()` is exposed for callers who need explicit control.

**Tech Stack:** Rust, existing crate internals (`SegmentWriter`, `MetadataReader`, `ContentStoreReader`, `TombstoneSet`, `IndexState`). No new dependencies.

---

### Task 1: Add `compact_with_budget()` skeleton with failing test

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

**Step 1: Write the failing test**

Add at the bottom of `mod tests` in `segment_manager.rs`:

```rust
#[test]
fn test_compact_with_budget_produces_multiple_segments() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let manager = SegmentManager::new(&base_dir).unwrap();

    // Create 3 segments with ~30 bytes of content each
    for i in 0..3 {
        manager
            .index_files(vec![InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn func_{i}() {{ let x = {i}; }}").into_bytes(),
                mtime: 0,
            }])
            .unwrap();
    }

    assert_eq!(manager.snapshot().len(), 3);

    // Use a tiny budget (1 byte) to force each file into its own segment
    manager.compact_with_budget(1).unwrap();

    let snap = manager.snapshot();
    // With a 1-byte budget, each ~30-byte file exceeds the budget,
    // so we should get 3 output segments (one per live file)
    assert_eq!(snap.len(), 3);

    // All files should still be findable
    let mut all_paths: Vec<String> = Vec::new();
    for seg in snap.iter() {
        let reader = seg.metadata_reader().unwrap();
        for entry in reader.iter_all() {
            all_paths.push(entry.unwrap().path);
        }
    }
    all_paths.sort();
    assert_eq!(all_paths, vec!["file_0.rs", "file_1.rs", "file_2.rs"]);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_compact_with_budget_produces_multiple_segments`
Expected: FAIL — `compact_with_budget` method does not exist.

**Step 3: Add the method stub**

In `segment_manager.rs`, add after `compact()`:

```rust
/// Compact segments with a per-segment memory budget.
///
/// Like [`compact()`](Self::compact), but instead of merging everything
/// into a single segment, flushes a new output segment whenever the
/// accumulated content size exceeds `max_segment_bytes`. This bounds
/// peak memory usage during compaction to approximately `max_segment_bytes`
/// plus overhead for posting lists and metadata.
///
/// # Arguments
///
/// * `max_segment_bytes` - Maximum total uncompressed content bytes per
///   output segment. When the accumulated size exceeds this threshold,
///   the current batch is flushed as a segment and a new batch begins.
///   A value of 0 means no limit (equivalent to `compact()`).
pub fn compact_with_budget(&self, max_segment_bytes: usize) -> Result<(), IndexError> {
    let _guard = self.write_lock.lock().unwrap();
    let current_segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();

    if current_segments.is_empty() {
        return Ok(());
    }

    if current_segments.len() == 1 {
        let ts = current_segments[0].load_tombstones()?;
        if ts.is_empty() {
            return Ok(());
        }
    }

    // Collect live entries, flushing to a new segment when budget is exceeded
    let mut batch: Vec<InputFile> = Vec::new();
    let mut batch_bytes: usize = 0;
    let mut new_segments: Vec<Arc<Segment>> = Vec::new();

    for segment in &current_segments {
        let tombstones = segment.load_tombstones()?;
        let reader = segment.metadata_reader()?;

        for entry_result in reader.iter_all() {
            let entry: FileMetadata = entry_result?;

            if tombstones.contains(entry.file_id) {
                continue;
            }

            let content = segment
                .content_reader()
                .read_content(entry.content_offset, entry.content_len)?;

            let content_len = content.len();
            batch.push(InputFile {
                path: entry.path,
                content,
                mtime: entry.mtime_epoch_secs,
            });
            batch_bytes += content_len;

            // Flush if over budget (0 means unlimited)
            if max_segment_bytes > 0 && batch_bytes > max_segment_bytes {
                let seg_id = self.next_segment_id();
                let writer = SegmentWriter::new(&self.segments_dir, seg_id);
                new_segments.push(Arc::new(writer.build(std::mem::take(&mut batch))?));
                batch_bytes = 0;
            }
        }
    }

    // Flush remaining batch
    if !batch.is_empty() {
        let seg_id = self.next_segment_id();
        let writer = SegmentWriter::new(&self.segments_dir, seg_id);
        new_segments.push(Arc::new(writer.build(batch)?));
    }

    let old_dirs: Vec<PathBuf> = current_segments
        .iter()
        .map(|s| s.dir_path().to_path_buf())
        .collect();

    self.state.publish(new_segments);

    for old_dir in old_dirs {
        let _ = fs::remove_dir_all(&old_dir);
    }

    Ok(())
}
```

Add `use crate::metadata::FileMetadata;` to the imports at the top of the file if not already present (it is already imported).

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-indexer-core -- test_compact_with_budget_produces_multiple_segments`
Expected: PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "feat: add compact_with_budget() for bounded-memory compaction"
```

---

### Task 2: Wire `compact()` to delegate to `compact_with_budget()`

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

**Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn test_compact_still_merges_to_single_segment() {
    // Verify compact() still produces a single segment (backward compat)
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let manager = SegmentManager::new(&base_dir).unwrap();

    for i in 0..3 {
        manager
            .index_files(vec![InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn func_{i}() {{ let x = {i}; }}").into_bytes(),
                mtime: 0,
            }])
            .unwrap();
    }

    manager.compact().unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].entry_count(), 3);
}
```

This test already exists as `test_compact_merges_segments`, so this step is really just confirming backward compatibility. We can skip adding it and instead just run the existing test suite.

**Step 2: Replace `compact()` body**

Replace the entire `compact()` method body with:

```rust
pub fn compact(&self) -> Result<(), IndexError> {
    self.compact_with_budget(0)
}
```

**Step 3: Run all existing compact tests to verify backward compatibility**

Run: `cargo test -p ferret-indexer-core -- test_compact`
Expected: ALL PASS — `test_compact_merges_segments`, `test_compact_excludes_tombstoned`, `test_compact_cleans_old_dirs`, `test_compact_empty_index`, `test_compact_single_segment_no_tombstones` should all still pass.

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "refactor: delegate compact() to compact_with_budget(0)"
```

---

### Task 3: Add a default budget constant and `compact_background()` support

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

**Step 1: Add the constant**

Near the top of `segment_manager.rs`, after the existing constants:

```rust
/// Default per-segment size budget for compaction (256 MB of uncompressed content).
///
/// This bounds peak memory during compaction to ~256 MB for file content plus
/// overhead for posting lists (~2x content size for positional postings, much
/// less for file-level postings). A 256 MB budget keeps total compaction RAM
/// under ~1 GB on typical codebases.
const DEFAULT_COMPACTION_BUDGET: usize = 256 * 1024 * 1024;
```

**Step 2: Update `compact_background()` to use the budget**

Replace `compact_background()`:

```rust
pub fn compact_background(self: &Arc<Self>) -> tokio::task::JoinHandle<Result<(), IndexError>> {
    let this = Arc::clone(self);
    tokio::spawn(async move { this.compact_with_budget(DEFAULT_COMPACTION_BUDGET) })
}
```

**Step 3: Run background compact test**

Run: `cargo test -p ferret-indexer-core -- test_compact_background`
Expected: PASS

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "feat: add DEFAULT_COMPACTION_BUDGET, use in compact_background()"
```

---

### Task 4: Add comprehensive edge case tests

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

**Step 1: Write the tests**

Add to `mod tests`:

```rust
#[test]
fn test_compact_with_budget_zero_means_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let manager = SegmentManager::new(&base_dir).unwrap();

    for i in 0..3 {
        manager
            .index_files(vec![InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn func_{i}() {{ let x = {i}; }}").into_bytes(),
                mtime: 0,
            }])
            .unwrap();
    }

    // budget=0 should produce a single segment (same as compact())
    manager.compact_with_budget(0).unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].entry_count(), 3);
}

#[test]
fn test_compact_with_budget_large_budget_merges_all() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let manager = SegmentManager::new(&base_dir).unwrap();

    for i in 0..5 {
        manager
            .index_files(vec![InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn func_{i}() {{ let x = {i}; }}").into_bytes(),
                mtime: 0,
            }])
            .unwrap();
    }

    // A very large budget should merge everything into one segment
    manager.compact_with_budget(100 * 1024 * 1024).unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].entry_count(), 5);
}

#[test]
fn test_compact_with_budget_excludes_tombstoned() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let repo_dir = dir.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();

    let manager = SegmentManager::new(&base_dir).unwrap();

    manager
        .index_files(vec![
            InputFile {
                path: "keep.rs".to_string(),
                content: b"fn keep() {}".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "delete.rs".to_string(),
                content: b"fn delete() {}".to_vec(),
                mtime: 0,
            },
        ])
        .unwrap();

    let changes = vec![ChangeEvent {
        path: PathBuf::from("delete.rs"),
        kind: ChangeKind::Deleted,
    }];
    manager.apply_changes(&repo_dir, &changes).unwrap();

    // Compact with tiny budget — should still exclude tombstoned
    manager.compact_with_budget(1).unwrap();

    let snap = manager.snapshot();
    let mut all_paths: Vec<String> = Vec::new();
    for seg in snap.iter() {
        let reader = seg.metadata_reader().unwrap();
        for entry in reader.iter_all() {
            all_paths.push(entry.unwrap().path);
        }
    }
    assert_eq!(all_paths, vec!["keep.rs"]);
}

#[test]
fn test_compact_with_budget_cleans_old_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let segments_dir = base_dir.join("segments");

    let manager = SegmentManager::new(&base_dir).unwrap();

    manager
        .index_files(vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() {}".to_vec(),
            mtime: 0,
        }])
        .unwrap();
    manager
        .index_files(vec![InputFile {
            path: "b.rs".to_string(),
            content: b"fn b() {}".to_vec(),
            mtime: 0,
        }])
        .unwrap();

    assert!(segments_dir.join("seg_0000").exists());
    assert!(segments_dir.join("seg_0001").exists());

    manager.compact_with_budget(1).unwrap();

    // Old dirs should be cleaned up
    assert!(!segments_dir.join("seg_0000").exists());
    assert!(!segments_dir.join("seg_0001").exists());

    // New segments should exist
    let snap = manager.snapshot();
    for seg in snap.iter() {
        assert!(seg.dir_path().exists());
    }
}

#[test]
fn test_compact_with_budget_empty_index() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let manager = SegmentManager::new(&base_dir).unwrap();

    manager.compact_with_budget(1024).unwrap();

    let snap = manager.snapshot();
    assert!(snap.is_empty());
}

#[test]
fn test_compact_with_budget_single_segment_no_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let manager = SegmentManager::new(&base_dir).unwrap();

    manager
        .index_files(vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() {}".to_vec(),
            mtime: 0,
        }])
        .unwrap();

    // Should be a no-op
    manager.compact_with_budget(1024).unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].entry_count(), 1);
}

#[test]
fn test_compact_with_budget_searchable_after() {
    use crate::multi_search::search_segments;

    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index");
    let manager = SegmentManager::new(&base_dir).unwrap();

    for i in 0..4 {
        manager
            .index_files(vec![InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn shared_func_{i}() {{ let result = compute(); }}").into_bytes(),
                mtime: 0,
            }])
            .unwrap();
    }

    // Compact with small budget to split across segments
    manager.compact_with_budget(1).unwrap();

    // Search should still find results across all output segments
    let snap = manager.snapshot();
    let result = search_segments(&snap, "result").unwrap();
    assert_eq!(result.files.len(), 4);
}
```

**Step 2: Run all tests**

Run: `cargo test -p ferret-indexer-core -- test_compact`
Expected: ALL PASS

**Step 3: Commit**

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "test: add comprehensive edge case tests for compact_with_budget()"
```

---

### Task 5: Run full workspace checks

**Step 1: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: PASS with no warnings

**Step 2: Run fmt check**

Run: `cargo fmt --all -- --check`
Expected: PASS

**Step 3: Run full test suite**

Run: `cargo test --workspace`
Expected: ALL PASS

**Step 4: Fix any issues found**

If clippy or tests flag anything, fix and re-run.

**Step 5: Commit any fixes**

```bash
git add -A
git commit -m "chore: fix clippy/fmt issues from bounded compaction"
```

(Skip this step if no fixes were needed.)
