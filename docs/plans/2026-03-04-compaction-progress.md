# Compaction Progress Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Show file-level compaction progress in the reindex CLI, with the CLI waiting for compaction to complete before exiting.

**Architecture:** Add new `ReindexProgress` variants for compaction phases. Add `compact_with_progress()` to `SegmentManager` that accepts a callback. In `catchup.rs`, call it synchronously (instead of fire-and-forget) during reindex. The CLI renderer handles the new events with a progress bar.

**Tech Stack:** Rust, `indicatif` (progress bars), `rayon` (parallel decompression), `serde` (JSON events), `std::sync::atomic` (thread-safe counters)

---

### Task 1: Add New Progress Event Variants

**Files:**
- Modify: `ferret-indexer-core/src/reindex_progress.rs:9-40`

**Step 1: Add the four new variants to `ReindexProgress`**

Add these variants after `CompactingSegments` and before `Complete`:

```rust
/// Live entries collected from segments for compaction.
CompactingCollected {
    live_files: usize,
    tombstoned: usize,
},
/// Decompressing file content during compaction.
CompactingFiles { current: usize, total: usize },
/// Writing a compacted segment.
CompactingWriting {
    segment_id: u32,
    files_done: usize,
    files_total: usize,
},
/// Compaction finished.
CompactionComplete {
    input_segments: usize,
    output_segments: usize,
    duration_ms: u64,
},
```

**Step 2: Add serde roundtrip tests for the new variants**

Add tests in the existing `mod tests` block:

```rust
#[test]
fn test_serde_roundtrip_compacting_collected() {
    let event = ReindexProgress::CompactingCollected {
        live_files: 100,
        tombstoned: 25,
    };
    let json = serde_json::to_string(&event).unwrap();
    let back: ReindexProgress = serde_json::from_str(&json).unwrap();
    assert_eq!(back, event);
}

#[test]
fn test_serde_roundtrip_compacting_files() {
    let event = ReindexProgress::CompactingFiles {
        current: 50,
        total: 200,
    };
    let json = serde_json::to_string(&event).unwrap();
    let back: ReindexProgress = serde_json::from_str(&json).unwrap();
    assert_eq!(back, event);
}

#[test]
fn test_serde_roundtrip_compacting_writing() {
    let event = ReindexProgress::CompactingWriting {
        segment_id: 5,
        files_done: 30,
        files_total: 100,
    };
    let json = serde_json::to_string(&event).unwrap();
    let back: ReindexProgress = serde_json::from_str(&json).unwrap();
    assert_eq!(back, event);
}

#[test]
fn test_serde_roundtrip_compaction_complete() {
    let event = ReindexProgress::CompactionComplete {
        input_segments: 5,
        output_segments: 1,
        duration_ms: 3200,
    };
    let json = serde_json::to_string(&event).unwrap();
    let back: ReindexProgress = serde_json::from_str(&json).unwrap();
    assert_eq!(back, event);
}
```

**Step 3: Run tests**

Run: `cargo test -p ferret-indexer-core -- reindex_progress`
Expected: All tests pass including new serde roundtrip tests.

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/reindex_progress.rs
git commit -m "feat(progress): add compaction progress event variants"
```

---

### Task 2: Add `compact_with_progress` to `SegmentManager`

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs:845-947`

**Step 1: Add `compact_with_progress` method**

Add this method right after `compact_with_budget` (line ~947) and before `compact_background`:

```rust
/// Like [`compact_with_budget`], but calls `on_progress` with
/// [`ReindexProgress`] events at each phase.
pub fn compact_with_progress<F: Fn(ReindexProgress) + Send + Sync>(
    &self,
    max_segment_bytes: usize,
    on_progress: &F,
) -> Result<(), IndexError> {
    let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let current_segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();

    if current_segments.is_empty() {
        tracing::debug!("compaction skipped: no segments");
        return Ok(());
    }

    if current_segments.len() == 1 {
        let ts = current_segments[0].load_tombstones()?;
        if ts.is_empty() {
            tracing::debug!("compaction skipped: single segment with no tombstones");
            return Ok(());
        }
    }

    tracing::info!(
        input_segments = current_segments.len(),
        max_segment_bytes,
        "compaction starting"
    );
    let start = std::time::Instant::now();

    // Phase 1: Collect live entries.
    let mut live_entries: Vec<(usize, FileMetadata)> = Vec::new();
    let mut tombstoned_count: usize = 0;
    for (seg_idx, segment) in current_segments.iter().enumerate() {
        let tombstones = segment.load_tombstones()?;
        let reader = segment.metadata_reader();
        for entry_result in reader.iter_all() {
            let entry: FileMetadata = entry_result?;
            if tombstones.contains(entry.file_id) {
                tombstoned_count += 1;
            } else {
                live_entries.push((seg_idx, entry));
            }
        }
    }
    on_progress(ReindexProgress::CompactingCollected {
        live_files: live_entries.len(),
        tombstoned: tombstoned_count,
    });

    // Phase 2: Decompress content in parallel with progress.
    let total_files = live_entries.len();
    let files_done = AtomicUsize::new(0);
    let input_files: Vec<InputFile> = live_entries
        .par_iter()
        .map(|(seg_idx, entry)| {
            let segment = &current_segments[*seg_idx];
            let content = segment
                .content_reader()
                .read_content(entry.content_offset, entry.content_len)?;
            let done = files_done.fetch_add(1, Ordering::Relaxed) + 1;
            // Emit progress every 50 files or on the last file.
            if done % 50 == 0 || done == total_files {
                on_progress(ReindexProgress::CompactingFiles {
                    current: done,
                    total: total_files,
                });
            }
            Ok(InputFile {
                path: entry.path.clone(),
                content,
                mtime: entry.mtime_epoch_secs,
            })
        })
        .collect::<Result<Vec<InputFile>, IndexError>>()?;

    // Phase 3: Budget-batched segment writing.
    let mut batch: Vec<InputFile> = Vec::new();
    let mut batch_bytes: usize = 0;
    let mut new_segments: Vec<Arc<Segment>> = Vec::new();
    let mut written_files: usize = 0;

    for file in input_files {
        let content_len = file.content.len();
        batch.push(file);
        batch_bytes += content_len;

        if max_segment_bytes > 0 && batch_bytes > max_segment_bytes {
            let batch_len = batch.len();
            let seg_id = self.next_segment_id()?;
            let writer = SegmentWriter::new(&self.segments_dir, seg_id);
            new_segments.push(Arc::new(writer.build(std::mem::take(&mut batch))?));
            written_files += batch_len;
            on_progress(ReindexProgress::CompactingWriting {
                segment_id: seg_id.0,
                files_done: written_files,
                files_total: total_files,
            });
            batch_bytes = 0;
        }
    }

    if !batch.is_empty() {
        let batch_len = batch.len();
        let seg_id = self.next_segment_id()?;
        let writer = SegmentWriter::new(&self.segments_dir, seg_id);
        new_segments.push(Arc::new(writer.build(batch)?));
        written_files += batch_len;
        on_progress(ReindexProgress::CompactingWriting {
            segment_id: seg_id.0,
            files_done: written_files,
            files_total: total_files,
        });
    }

    let old_dirs: Vec<PathBuf> = current_segments
        .iter()
        .map(|s| s.dir_path().to_path_buf())
        .collect();

    let input_segment_count = old_dirs.len();
    let output_segment_count = new_segments.len();
    self.state.publish(new_segments);

    for old_dir in &old_dirs {
        if let Err(e) = fs::remove_dir_all(old_dir) {
            tracing::warn!(path = %old_dir.display(), error = %e, "failed to remove old segment directory");
        }
    }

    let elapsed = start.elapsed();
    on_progress(ReindexProgress::CompactionComplete {
        input_segments: input_segment_count,
        output_segments: output_segment_count,
        duration_ms: elapsed.as_millis() as u64,
    });

    tracing::info!(
        input_segments = input_segment_count,
        output_segments = output_segment_count,
        elapsed_ms = elapsed.as_millis() as u64,
        "compaction complete"
    );
    Ok(())
}
```

**Step 2: Add the missing import for `ReindexProgress`**

At the top of `segment_manager.rs`, add:

```rust
use crate::reindex_progress::ReindexProgress;
```

**Step 3: Run clippy and tests**

Run: `cargo clippy -p ferret-indexer-core -- -D warnings && cargo test -p ferret-indexer-core -- segment_manager`
Expected: No warnings, all tests pass.

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "feat(compact): add compact_with_progress method"
```

---

### Task 3: Wire Compaction Progress Into Catchup

**Files:**
- Modify: `ferret-indexer-core/src/catchup.rs:70-112`

**Step 1: Replace fire-and-forget compaction with synchronous progress-reporting compaction**

In `run_catchup_with_progress`, replace the two compaction blocks.

Replace the no-changes compaction block (lines 72-78):

```rust
// Force compaction even with no changes if requested.
if force_compact && !manager.snapshot().is_empty() {
    let snap = manager.snapshot();
    on_progress(ReindexProgress::CompactingSegments {
        input_segments: snap.len(),
    });
    manager.compact_with_progress(DEFAULT_COMPACTION_BUDGET, &on_progress)?;
    on_progress(ReindexProgress::Complete { changes_applied: 0 });
}
```

Replace the post-changes compaction block (lines 101-108):

```rust
if force_compact || manager.should_compact() {
    tracing::info!("compaction recommended after catch-up");
    let snap = manager.snapshot();
    on_progress(ReindexProgress::CompactingSegments {
        input_segments: snap.len(),
    });
    manager.compact_with_progress(DEFAULT_COMPACTION_BUDGET, &on_progress)?;
}
```

**Step 2: Add the `DEFAULT_COMPACTION_BUDGET` import**

At the top of `catchup.rs`, the constant isn't currently accessible. Either:
- Make `DEFAULT_COMPACTION_BUDGET` `pub(crate)` in `segment_manager.rs`, OR
- Use the same value `256 * 1024 * 1024` inline as a local constant

Preferred: make it `pub(crate)` in `segment_manager.rs` (change `const` to `pub(crate) const`), then add to catchup.rs imports:

```rust
use crate::segment_manager::DEFAULT_COMPACTION_BUDGET;
```

**Step 3: Run tests**

Run: `cargo test -p ferret-indexer-core -- catchup`
Expected: All existing tests pass. The `test_catchup_force_compact_emits_compacting_event` test should now also see `CompactingCollected` and `CompactionComplete` events (though it only asserts `CompactingSegments`).

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/segment_manager.rs ferret-indexer-core/src/catchup.rs
git commit -m "feat(catchup): use synchronous compact_with_progress during reindex"
```

---

### Task 4: Handle New Events in CLI Progress Renderer

**Files:**
- Modify: `ferret-indexer-cli/src/reindex_display.rs:190-210`

**Step 1: Add handlers for new compaction events in `ProgressRenderer::handle`**

After the existing `CompactingSegments` handler and before `Complete`, add:

```rust
ReindexProgress::CompactingCollected {
    live_files,
    tombstoned,
} => {
    if tombstoned > 0 {
        eprintln!(
            "Compacting {live_files} live files ({tombstoned} tombstoned)"
        );
    } else {
        eprintln!("Compacting {live_files} files");
    }
    // Set up progress bar for decompression + writing.
    if let Some(sp) = self.spinner.take() {
        sp.finish_and_clear();
    }
    let bar = ProgressBar::new(live_files as u64);
    bar.set_style(
        ProgressStyle::with_template(
            "Compacting [{bar:30.yellow/dim}] {pos}/{len} files  {msg}",
        )
        .unwrap()
        .progress_chars("##-"),
    );
    bar.set_message("decompressing...");
    self.bar = Some(bar);
}
ReindexProgress::CompactingFiles { current, total } => {
    if let Some(bar) = &self.bar {
        bar.set_length(total as u64);
        bar.set_position(current as u64);
        bar.set_message("decompressing...");
    }
}
ReindexProgress::CompactingWriting {
    segment_id,
    files_done,
    files_total,
} => {
    if let Some(bar) = &self.bar {
        bar.set_length(files_total as u64);
        bar.set_position(files_done as u64);
        bar.set_message(format!("writing seg_{segment_id:04}"));
    }
}
ReindexProgress::CompactionComplete {
    input_segments,
    output_segments,
    duration_ms,
} => {
    if let Some(bar) = self.bar.take() {
        bar.finish_and_clear();
    }
    let secs = duration_ms as f64 / 1000.0;
    eprintln!(
        "Compaction complete: {input_segments} segments \u{2192} {output_segments} in {secs:.1}s"
    );
}
```

**Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

**Step 3: Run fmt check**

Run: `cargo fmt --all -- --check`
Expected: No formatting issues.

**Step 4: Commit**

```bash
git add ferret-indexer-cli/src/reindex_display.rs
git commit -m "feat(cli): render compaction progress bar and stats"
```

---

### Task 5: Add Integration Test for Compaction Progress Events

**Files:**
- Modify: `ferret-indexer-core/src/catchup.rs` (test module at bottom)

**Step 1: Add a test that verifies compaction progress events are emitted**

Add to the existing test module:

```rust
#[tokio::test]
async fn test_catchup_force_compact_emits_detailed_compaction_events() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo(repo);

    let ferret_dir = repo.join(".ferret_index");
    fs::create_dir_all(ferret_dir.join("segments")).unwrap();
    let manager = Arc::new(SegmentManager::new(&ferret_dir).unwrap());

    // Write checkpoint at current HEAD.
    let git = GitChangeDetector::new(repo.to_path_buf());
    let head = git.get_head_sha().unwrap();
    let cp = Checkpoint::new(Some(head), 0);
    write_checkpoint(&ferret_dir, &cp).unwrap();

    // Create two files so there's something to compact.
    fs::write(repo.join("a.rs"), "fn a() { let x = 1; }").unwrap();
    fs::write(repo.join("b.rs"), "fn b() { let y = 2; }").unwrap();

    let events = std::sync::Mutex::new(Vec::new());
    let _changes = run_catchup_with_progress(repo, &ferret_dir, &manager, true, |ev| {
        events.lock().unwrap().push(ev);
    })
    .unwrap();

    let events = events.into_inner().unwrap();

    // Should have CompactingSegments (start signal).
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ReindexProgress::CompactingSegments { .. })),
        "expected CompactingSegments, got: {events:?}"
    );
    // Should have CompactingCollected with live_files > 0.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ReindexProgress::CompactingCollected { live_files, .. } if *live_files > 0)),
        "expected CompactingCollected with live_files > 0, got: {events:?}"
    );
    // Should have CompactionComplete.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ReindexProgress::CompactionComplete { .. })),
        "expected CompactionComplete, got: {events:?}"
    );
    // Complete should come AFTER CompactionComplete.
    let compact_done_idx = events
        .iter()
        .position(|e| matches!(e, ReindexProgress::CompactionComplete { .. }));
    let complete_idx = events
        .iter()
        .position(|e| matches!(e, ReindexProgress::Complete { .. }));
    assert!(
        compact_done_idx < complete_idx,
        "CompactionComplete should precede Complete"
    );
}
```

**Step 2: Run the test**

Run: `cargo test -p ferret-indexer-core -- test_catchup_force_compact_emits_detailed_compaction_events`
Expected: PASS

**Step 3: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/catchup.rs
git commit -m "test(catchup): verify detailed compaction progress events"
```

---

### Task 6: Final CI Checks

**Step 1: Clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

**Step 2: Format check**

Run: `cargo fmt --all -- --check`
Expected: Clean.

**Step 3: Full test suite**

Run: `cargo test --workspace`
Expected: All pass.
