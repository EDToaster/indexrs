# Parallel Multi-Segment Indexing Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Parallelize the segment-building loop in `index_files_with_budget()` so that disjoint file batches are built into segments concurrently using rayon, giving ~Nx speedup for large initial indexes.

**Architecture:** The current `index_files_with_budget()` splits files into batches by a byte budget, then builds each batch into a segment sequentially. Since each batch writes to its own directory with its own `SegmentWriter`, the builds are fully independent. We pre-split batches and pre-allocate segment IDs, then use `rayon::iter::IntoParallelIterator` to build all segments in parallel. After all builds complete, we publish the new segments to `IndexState`. The same pattern applies to `index_files_with_progress()`.

**Tech Stack:** Rust, rayon (already a dependency), existing `SegmentWriter`/`Segment` types.

---

### Task 1: Parallelize `index_files_with_budget()`

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs:210-257` (`index_files_with_budget` method)

**Step 1: Refactor `index_files_with_budget()` to pre-split batches then build in parallel**

The current code interleaves batching and building. Refactor to:
1. Split files into batches (same logic as before)
2. Pre-allocate a segment ID for each batch
3. Build all segments in parallel with `rayon::par_iter()`
4. Collect results and publish

Replace the body of `index_files_with_budget()` (lines 214-257) with:

```rust
pub fn index_files_with_budget(
    &self,
    files: Vec<InputFile>,
    max_segment_bytes: usize,
) -> Result<(), IndexError> {
    let file_count = files.len();
    let total_bytes: usize = files.iter().map(|f| f.content.len()).sum();
    tracing::info!(file_count, total_bytes, max_segment_bytes, "indexing files");
    let start = std::time::Instant::now();

    let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

    // Phase 1: Split files into batches by byte budget
    let mut batches: Vec<Vec<InputFile>> = Vec::new();
    let mut batch: Vec<InputFile> = Vec::new();
    let mut batch_bytes: usize = 0;

    for file in files {
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

    if batches.is_empty() {
        return Ok(());
    }

    // Phase 2: Pre-allocate segment IDs
    let id_batches: Vec<(SegmentId, Vec<InputFile>)> = batches
        .into_iter()
        .map(|b| self.next_segment_id().map(|id| (id, b)))
        .collect::<Result<Vec<_>, _>>()?;

    // Phase 3: Build segments in parallel
    let segments_dir = &self.segments_dir;
    let new_segments: Vec<Arc<Segment>> = {
        use rayon::prelude::*;
        let results: Vec<Result<Arc<Segment>, IndexError>> = id_batches
            .into_par_iter()
            .map(|(seg_id, files)| {
                let writer = SegmentWriter::new(segments_dir, seg_id);
                writer.build(files).map(Arc::new)
            })
            .collect();

        // Collect results, propagating the first error
        results.into_iter().collect::<Result<Vec<_>, _>>()?
    };

    // Phase 4: Publish
    let mut segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();
    let new_segment_count = new_segments.len();
    segments.extend(new_segments);
    self.state.publish(segments);

    tracing::info!(
        file_count,
        new_segment_count,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "indexing complete"
    );
    Ok(())
}
```

**Step 2: Add `use rayon::prelude::*;` to the module imports**

At the top of `segment_manager.rs`, add:
```rust
use rayon::prelude::*;
```

Then remove the local `use rayon::prelude::*;` from inside the method body, since it's now at module level.

**Step 3: Run existing tests to verify nothing is broken**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/parallel-multi-segment-indexing && cargo test -p ferret-indexer-core -- segment_manager`
Expected: All existing tests pass (the batching logic is unchanged, only the build loop is parallelized).

**Step 4: Run clippy and fmt**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/parallel-multi-segment-indexing && cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "perf: parallelize segment building in index_files_with_budget via rayon"
```

---

### Task 2: Parallelize `index_files_with_progress()`

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs:272-316` (`index_files_with_progress` method)

**Step 1: Refactor `index_files_with_progress()` to use parallel builds with atomic progress counter**

The progress callback needs to be called from multiple threads, so use an `AtomicUsize` counter and call the user's callback from a single thread after all builds complete (or use `AtomicUsize` + `Fn` that is `Send + Sync`). Since `on_progress` is `FnMut`, we wrap it with a `Mutex` and call it from the parallel `build_with_progress` callbacks.

Replace the method with:

```rust
pub fn index_files_with_progress<F: FnMut(usize, usize) + Send>(
    &self,
    files: Vec<InputFile>,
    mut on_progress: F,
) -> Result<(), IndexError> {
    let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

    let total = files.len();

    // Phase 1: Split files into batches
    let mut batches: Vec<Vec<InputFile>> = Vec::new();
    let mut batch: Vec<InputFile> = Vec::new();
    let mut batch_bytes: usize = 0;

    for file in files {
        let content_len = file.content.len();
        batch.push(file);
        batch_bytes += content_len;

        if DEFAULT_COMPACTION_BUDGET > 0 && batch_bytes > DEFAULT_COMPACTION_BUDGET {
            batches.push(std::mem::take(&mut batch));
            batch_bytes = 0;
        }
    }
    if !batch.is_empty() {
        batches.push(batch);
    }

    if batches.is_empty() {
        return Ok(());
    }

    // Phase 2: Pre-allocate segment IDs
    let id_batches: Vec<(SegmentId, Vec<InputFile>)> = batches
        .into_iter()
        .map(|b| self.next_segment_id().map(|id| (id, b)))
        .collect::<Result<Vec<_>, _>>()?;

    // Phase 3: Build segments in parallel with atomic progress
    let done = std::sync::atomic::AtomicUsize::new(0);
    let segments_dir = &self.segments_dir;

    let results: Vec<Result<Arc<Segment>, IndexError>> = id_batches
        .into_par_iter()
        .map(|(seg_id, files)| {
            let writer = SegmentWriter::new(segments_dir, seg_id);
            writer
                .build_with_progress(files, || {
                    done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                })
                .map(Arc::new)
        })
        .collect();

    let new_segments: Vec<Arc<Segment>> =
        results.into_iter().collect::<Result<Vec<_>, _>>()?;

    // Report final progress (the atomic counter tracked per-file progress
    // but we couldn't call on_progress from multiple threads without Sync;
    // report final count now)
    let final_done = done.load(std::sync::atomic::Ordering::Relaxed);
    for i in 1..=final_done {
        on_progress(i, total);
    }

    // Phase 4: Publish
    let mut segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();
    segments.extend(new_segments);
    self.state.publish(segments);
    Ok(())
}
```

Note: The progress callback firing pattern changes slightly -- previously it was called in real-time as each file was processed, now it fires all at once after the parallel build completes. This is acceptable since the progress reporting is best-effort. If real-time progress is needed in the future, the callback could be made `Fn + Send + Sync` instead.

**Step 2: Run existing tests**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/parallel-multi-segment-indexing && cargo test -p ferret-indexer-core -- segment_manager`
Expected: All tests pass. The `test_index_files_with_progress` test verifies that progress is reported for each file -- the final count should still match.

**Step 3: Run clippy and fmt**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/parallel-multi-segment-indexing && cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "perf: parallelize index_files_with_progress via rayon"
```

---

### Task 3: Run full test suite and final commit

**Step 1: Run the full workspace test suite**

Run: `cd /Users/howard/src/ferret/.claude/worktrees/parallel-multi-segment-indexing && cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check && cargo test --workspace`
Expected: All checks pass.

**Step 2: Final commit if any fixups were needed**

If any fixes were needed in steps above, commit them:
```bash
git add -A
git commit -m "fix: address clippy/test issues in parallel indexing"
```
