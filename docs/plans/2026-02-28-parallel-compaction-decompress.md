# Parallel Compaction Decompression Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Parallelize the zstd decompression phase of `compact_with_budget()` using rayon for near-linear speedup on the read side.

**Architecture:** The current `compact_with_budget()` in `segment_manager.rs` iterates source segments sequentially, calling `read_content()` (zstd decompress) per file. Each decompression is independent and CPU-bound. We parallelize by first collecting all live (non-tombstoned) metadata entries from all segments, then using rayon's `par_iter()` to decompress content in parallel, producing `InputFile`s. The sequential budget-batching and `SegmentWriter` logic remains unchanged.

**Tech Stack:** rayon (already in `Cargo.toml`)

---

### Task 1: Parallelize decompression in compact_with_budget

**Files:**
- Modify: `indexrs-core/src/segment_manager.rs:519-609` (the `compact_with_budget` method)

**Step 1: Modify compact_with_budget to collect metadata first, then decompress in parallel**

The current code:
```rust
for segment in &current_segments {
    let tombstones = segment.load_tombstones()?;
    let reader = segment.metadata_reader()?;
    for entry_result in reader.iter_all() {
        let entry = entry_result?;
        if tombstones.contains(entry.file_id) { continue; }
        let content = segment.content_reader().read_content(entry.content_offset, entry.content_len)?;
        // ... push to batch, check budget ...
    }
}
```

Replace with a two-phase approach:

**Phase 1 (sequential):** Collect all live entries as `(segment_index, FileMetadata)` tuples, filtering tombstoned entries. This is cheap (metadata is already memory-mapped).

**Phase 2 (parallel):** Use `rayon::par_iter()` to decompress content for all live entries in parallel, producing `Vec<InputFile>`.

**Phase 3 (sequential):** Feed the resulting `InputFile`s through the existing budget-batching and `SegmentWriter` logic.

New code for `compact_with_budget`:

```rust
use rayon::prelude::*;

pub fn compact_with_budget(&self, max_segment_bytes: usize) -> Result<(), IndexError> {
    let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let current_segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();

    // ... existing early-return checks ...

    tracing::info!(
        input_segments = current_segments.len(),
        max_segment_bytes,
        "compaction starting"
    );
    let start = std::time::Instant::now();

    // Phase 1: Collect all live (non-tombstoned) entries with their segment reference
    let mut live_entries: Vec<(usize, FileMetadata)> = Vec::new();
    for (seg_idx, segment) in current_segments.iter().enumerate() {
        let tombstones = segment.load_tombstones()?;
        let reader = segment.metadata_reader()?;
        for entry_result in reader.iter_all() {
            let entry = entry_result?;
            if !tombstones.contains(entry.file_id) {
                live_entries.push((seg_idx, entry));
            }
        }
    }

    // Phase 2: Decompress content in parallel using rayon
    let input_files: Vec<InputFile> = live_entries
        .par_iter()
        .map(|(seg_idx, entry)| {
            let segment = &current_segments[*seg_idx];
            let content = segment
                .content_reader()
                .read_content(entry.content_offset, entry.content_len)?;
            Ok(InputFile {
                path: entry.path.clone(),
                content,
                mtime: entry.mtime_epoch_secs,
            })
        })
        .collect::<Result<Vec<InputFile>, IndexError>>()?;

    // Phase 3: Budget-batched segment writing (sequential, unchanged)
    let mut batch: Vec<InputFile> = Vec::new();
    let mut batch_bytes: usize = 0;
    let mut new_segments: Vec<Arc<Segment>> = Vec::new();

    for file in input_files {
        let content_len = file.content.len();
        batch.push(file);
        batch_bytes += content_len;

        if max_segment_bytes > 0 && batch_bytes > max_segment_bytes {
            let seg_id = self.next_segment_id()?;
            let writer = SegmentWriter::new(&self.segments_dir, seg_id);
            new_segments.push(Arc::new(writer.build(std::mem::take(&mut batch))?));
            batch_bytes = 0;
        }
    }

    if !batch.is_empty() {
        let seg_id = self.next_segment_id()?;
        let writer = SegmentWriter::new(&self.segments_dir, seg_id);
        new_segments.push(Arc::new(writer.build(batch)?));
    }

    // ... existing old-dir cleanup and publish logic ...
}
```

**Step 2: Add rayon import to segment_manager.rs**

Add `use rayon::prelude::*;` to the imports.

**Step 3: Run tests**

Run: `cargo test -p indexrs-core -- compact`
Expected: All existing compact tests pass (the behavior is identical, only the execution strategy changed).

**Step 4: Run clippy and fmt**

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: No warnings or formatting issues.

**Step 5: Commit**

```bash
git add indexrs-core/src/segment_manager.rs
git commit -m "perf: parallelize compaction decompression with rayon"
```

## Trade-offs

- **Peak memory:** The parallel approach collects ALL live `InputFile`s in memory before budget-batching. This is slightly higher peak memory than the sequential version which could flush mid-iteration. However, since `compact_with_budget` already accumulates content up to `max_segment_bytes` before flushing, and the typical compaction involves segments that fit in memory, this trade-off is acceptable. The parallel decompression speedup (near-linear on multi-core machines) more than compensates.
- **Error handling:** rayon's `collect::<Result<Vec<_>, _>>()` short-circuits on the first error, which matches the sequential behavior.
