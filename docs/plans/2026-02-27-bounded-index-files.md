# Bounded `index_files` Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add size-based segment splitting to `index_files()` so that large initial builds produce capped segments instead of one giant segment, bounding peak RAM.

**Architecture:** Mirror the existing `compact()` / `compact_with_budget()` pattern. Add `index_files_with_budget(files, max_segment_bytes)` that chunks input files by accumulated content size, building one segment per chunk. Change `index_files()` to delegate to it with `DEFAULT_COMPACTION_BUDGET` (256 MB). Update `build_index.rs` to drop its manual file-count batching since `index_files` now handles splitting internally.

**Tech Stack:** Rust, existing `SegmentWriter`, `IndexState`, `DEFAULT_COMPACTION_BUDGET`

---

### Task 1: Write failing tests for `index_files_with_budget`

**Files:**
- Modify: `indexrs-core/src/segment_manager.rs` (tests module, after line ~1304)

**Step 1: Write the failing tests**

Add three tests to the existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn test_index_files_with_budget_splits_segments() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".indexrs");
    let manager = SegmentManager::new(&base_dir).unwrap();

    // Each file is ~30 bytes of content
    let files: Vec<InputFile> = (0..10)
        .map(|i| InputFile {
            path: format!("file_{i}.rs"),
            content: format!("fn func_{i}() {{ let x = {i}; }}").into_bytes(),
            mtime: 0,
        })
        .collect();

    // Budget of 50 bytes should split 10 files into ~6 segments
    // (each file ~30 bytes, so ~1-2 files per segment)
    manager.index_files_with_budget(files, 50).unwrap();

    let snap = manager.snapshot();
    assert!(
        snap.len() > 1,
        "should produce multiple segments, got {}",
        snap.len()
    );

    // Total entry count across all segments should be 10
    let total_entries: u32 = snap.iter().map(|s| s.entry_count()).sum();
    assert_eq!(total_entries, 10);
}

#[test]
fn test_index_files_with_budget_zero_means_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".indexrs");
    let manager = SegmentManager::new(&base_dir).unwrap();

    let files: Vec<InputFile> = (0..5)
        .map(|i| InputFile {
            path: format!("file_{i}.rs"),
            content: format!("fn func_{i}() {{ let x = {i}; }}").into_bytes(),
            mtime: 0,
        })
        .collect();

    // Budget of 0 means no limit — should produce exactly 1 segment
    manager.index_files_with_budget(files, 0).unwrap();

    let snap = manager.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].entry_count(), 5);
}

#[test]
fn test_index_files_with_budget_searchable_across_segments() {
    use crate::multi_search::search_segments;

    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".indexrs");
    let manager = SegmentManager::new(&base_dir).unwrap();

    let files: Vec<InputFile> = (0..6)
        .map(|i| InputFile {
            path: format!("file_{i}.rs"),
            content: format!("fn shared_keyword_{i}() {{ let result = compute(); }}")
                .into_bytes(),
            mtime: 0,
        })
        .collect();

    // Small budget to force multiple segments
    manager.index_files_with_budget(files, 1).unwrap();

    let snap = manager.snapshot();
    assert!(snap.len() > 1);

    // Search should find "result" across all segments
    let result = search_segments(&snap, "result").unwrap();
    assert_eq!(result.files.len(), 6);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p indexrs-core -- test_index_files_with_budget -v`
Expected: FAIL — `index_files_with_budget` method does not exist.

**Step 3: Commit**

```bash
git add indexrs-core/src/segment_manager.rs
git commit -m "test: add failing tests for index_files_with_budget"
```

---

### Task 2: Implement `index_files_with_budget`

**Files:**
- Modify: `indexrs-core/src/segment_manager.rs:192-204`

**Step 1: Add `index_files_with_budget` and update `index_files` to delegate**

Replace the existing `index_files` method (lines 192-204) with:

```rust
    /// Index a set of files, splitting into multiple segments when the
    /// accumulated content size exceeds `max_segment_bytes`.
    ///
    /// This bounds peak memory during the build to approximately
    /// `max_segment_bytes` of file content plus overhead for posting lists.
    ///
    /// # Arguments
    ///
    /// * `files` - The files to index.
    /// * `max_segment_bytes` - Maximum total uncompressed content bytes per
    ///   segment. A value of 0 means no limit (single segment).
    pub fn index_files_with_budget(
        &self,
        files: Vec<InputFile>,
        max_segment_bytes: usize,
    ) -> Result<(), IndexError> {
        let _guard = self.write_lock.lock().unwrap();

        let mut batch: Vec<InputFile> = Vec::new();
        let mut batch_bytes: usize = 0;
        let mut segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();

        for file in files {
            let content_len = file.content.len();
            batch.push(file);
            batch_bytes += content_len;

            if max_segment_bytes > 0 && batch_bytes > max_segment_bytes {
                let seg_id = self.next_segment_id();
                let writer = SegmentWriter::new(&self.segments_dir, seg_id);
                segments.push(Arc::new(writer.build(std::mem::take(&mut batch))?));
                batch_bytes = 0;
            }
        }

        // Flush remaining files (or empty batch, which creates an empty segment)
        let seg_id = self.next_segment_id();
        let writer = SegmentWriter::new(&self.segments_dir, seg_id);
        segments.push(Arc::new(writer.build(batch)?));

        self.state.publish(segments);
        Ok(())
    }

    /// Index a set of files into the index.
    ///
    /// Uses [`DEFAULT_COMPACTION_BUDGET`] to split large inputs into
    /// multiple capped segments, bounding peak memory.
    pub fn index_files(&self, files: Vec<InputFile>) -> Result<(), IndexError> {
        self.index_files_with_budget(files, DEFAULT_COMPACTION_BUDGET)
    }
```

**Step 2: Run all tests to verify they pass**

Run: `cargo test -p indexrs-core -- test_index_files -v`
Expected: All `test_index_files*` tests PASS (existing + new).

**Step 3: Run the full test suite**

Run: `cargo test --workspace`
Expected: All tests PASS. Existing tests use small inputs that fit well within 256 MB, so they produce a single segment as before.

**Step 4: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

**Step 5: Commit**

```bash
git add indexrs-core/src/segment_manager.rs
git commit -m "feat: add size-budgeted index_files_with_budget, cap index_files at 256MB/segment"
```

---

### Task 3: Simplify `build_index.rs` to drop manual batching

**Files:**
- Modify: `indexrs-core/examples/build_index.rs`

**Step 1: Remove BATCH_SIZE constant and simplify `full_build`**

Replace the `BATCH_SIZE` constant (line 23) and the `full_build` function (lines ~78-99) so it does a single `index_files` call instead of manual chunking:

```rust
fn full_build(
    dir: &PathBuf,
    manager: &SegmentManager,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("  Walking directory...");
    let files = walk_and_collect(dir)?;
    let file_count = files.len();
    eprintln!("  Found {file_count} indexable files");

    eprintln!("  Building segments...");
    manager.index_files(files)?;

    let snap = manager.snapshot();
    eprintln!("  Built {} segment(s)", snap.len());

    Ok(())
}
```

Remove the `BATCH_SIZE` constant (line 23: `const BATCH_SIZE: usize = 5000;`).

**Step 2: Run the example to verify it still works**

Run: `cargo run -p indexrs-core --example build_index --release -- .`
Expected: Builds successfully, prints segment count.

**Step 3: Commit**

```bash
git add indexrs-core/examples/build_index.rs
git commit -m "refactor: simplify build_index example, rely on index_files budget splitting"
```
