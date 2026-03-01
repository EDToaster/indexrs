# Segment Build Progress Callback Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add per-file progress reporting during segment building so `build_index` can show inline percentage while indexing.

**Architecture:** Add an `FnMut()` callback to `SegmentWriter::build_inner` that fires after each file is processed. Expose this via `build_with_progress()`. Then add `SegmentManager::index_files_with_progress()` that wraps the callback with a file counter to report `(done, total)`. The existing `build()` and `index_files()` delegate to the new methods with no-op closures — zero duplication.

**Tech Stack:** Rust, no new dependencies

---

### Task 1: Add progress callback to SegmentWriter

**Files:**
- Modify: `indexrs-core/src/segment.rs:228-325` (build, build_inner)

**Step 1: Write the failing test**

Add this test inside the existing `#[cfg(test)] mod tests` block in `segment.rs`:

```rust
#[test]
fn test_build_with_progress_callback_count() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".indexrs/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let files = vec![
        InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() {}".to_vec(),
            mtime: 1,
        },
        InputFile {
            path: "b.rs".to_string(),
            content: b"fn b() {}".to_vec(),
            mtime: 2,
        },
        InputFile {
            path: "c.rs".to_string(),
            content: b"fn c() {}".to_vec(),
            mtime: 3,
        },
    ];

    let mut count = 0usize;
    let writer = SegmentWriter::new(&base_dir, SegmentId(1));
    writer
        .build_with_progress(files, || count += 1)
        .unwrap();

    assert_eq!(count, 3, "callback should fire once per file");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p indexrs-core -- test_build_with_progress_callback_count`
Expected: FAIL — `build_with_progress` method doesn't exist yet.

**Step 3: Implement the changes**

Make `build_inner` generic over a callback, then have `build` delegate through `build_with_progress`:

```rust
/// Build the segment from a list of input files.
///
/// This is the non-progress version — delegates to
/// [`build_with_progress`](Self::build_with_progress) with a no-op callback.
pub fn build(self, files: Vec<InputFile>) -> Result<Segment, IndexError> {
    self.build_with_progress(files, || {})
}

/// Build the segment from a list of input files, calling `on_file_done`
/// after each file has been processed (trigrams extracted, content
/// compressed, metadata recorded).
pub fn build_with_progress<F: FnMut()>(
    self,
    files: Vec<InputFile>,
    on_file_done: F,
) -> Result<Segment, IndexError> {
    let seg_name = format!("seg_{:04}", self.segment_id.0);
    let final_dir = self.base_dir.join(&seg_name);
    let temp_dir = self
        .base_dir
        .join(format!(".{seg_name}_tmp_{}", std::process::id()));

    // Clean up any leftover temp dir from a previous crash
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)?;
    }
    fs::create_dir_all(&temp_dir)?;

    // Build result, cleaning up temp dir on error
    match self.build_inner(&temp_dir, &final_dir, files, on_file_done) {
        Ok(segment) => Ok(segment),
        Err(e) => {
            // Best-effort cleanup of temp dir
            let _ = fs::remove_dir_all(&temp_dir);
            Err(e)
        }
    }
}
```

Update `build_inner` to accept and call the callback:

```rust
fn build_inner<F: FnMut()>(
    &self,
    temp_dir: &Path,
    final_dir: &Path,
    files: Vec<InputFile>,
    mut on_file_done: F,
) -> Result<Segment, IndexError> {
    let mut posting_builder = PostingListBuilder::file_only();
    let mut metadata_builder = MetadataBuilder::new();
    let mut content_writer =
        ContentStoreWriter::new(&temp_dir.join("content.zst")).map_err(IndexError::Io)?;

    for (i, input) in files.iter().enumerate() {
        // ... all existing per-file work stays exactly the same ...
        // (file_id, hash, language, line_count, posting_builder.add_file,
        //  content_writer.add_content, metadata_builder.add_file)

        on_file_done();
    }

    // ... rest of build_inner unchanged (finalize, write trigrams, meta, paths, content, rename) ...
}
```

The key change inside the loop: add `on_file_done();` as the **last line** of the `for` body, after the `metadata_builder.add_file(...)` call.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p indexrs-core -- test_build_with_progress`
Expected: PASS

Run: `cargo test -p indexrs-core -- test_segment_writer`
Expected: PASS (existing tests still work since `build` delegates to `build_with_progress`)

**Step 5: Commit**

```bash
git add indexrs-core/src/segment.rs
git commit -m "feat(segment): add build_with_progress callback to SegmentWriter"
```

---

### Task 2: Add index_files_with_progress to SegmentManager

**Files:**
- Modify: `indexrs-core/src/segment_manager.rs:203-244`

**Step 1: Write the failing test**

Add this test inside the existing `#[cfg(test)] mod tests` block in `segment_manager.rs`:

```rust
#[test]
fn test_index_files_with_progress() {
    let dir = tempfile::tempdir().unwrap();
    let indexrs_dir = dir.path().join(".indexrs");
    std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

    let manager = SegmentManager::new(&indexrs_dir).unwrap();
    let files = vec![
        InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() {}".to_vec(),
            mtime: 1,
        },
        InputFile {
            path: "b.rs".to_string(),
            content: b"fn b() {}".to_vec(),
            mtime: 2,
        },
        InputFile {
            path: "c.rs".to_string(),
            content: b"fn c() {}".to_vec(),
            mtime: 3,
        },
    ];

    let progress = std::sync::Mutex::new(Vec::new());
    manager
        .index_files_with_progress(files, |done, total| {
            progress.lock().unwrap().push((done, total));
        })
        .unwrap();

    let progress = progress.into_inner().unwrap();
    assert_eq!(
        progress,
        vec![(1, 3), (2, 3), (3, 3)],
        "should report (done, total) for each file"
    );

    // Verify index was actually built
    let snap = manager.snapshot();
    assert_eq!(snap.len(), 1);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p indexrs-core -- test_index_files_with_progress`
Expected: FAIL — method doesn't exist.

**Step 3: Implement `index_files_with_progress`**

Add the new public method to `SegmentManager` (after `index_files`):

```rust
/// Index files with a progress callback.
///
/// Behaves identically to [`index_files`](Self::index_files) but calls
/// `on_progress(files_done, files_total)` after each file is processed
/// during segment building.
pub fn index_files_with_progress<F: FnMut(usize, usize)>(
    &self,
    files: Vec<InputFile>,
    mut on_progress: F,
) -> Result<(), IndexError> {
    let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

    let total = files.len();
    let mut done = 0usize;
    let mut batch: Vec<InputFile> = Vec::new();
    let mut batch_bytes: usize = 0;
    let mut segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();

    for file in files {
        let content_len = file.content.len();
        batch.push(file);
        batch_bytes += content_len;

        if DEFAULT_COMPACTION_BUDGET > 0 && batch_bytes > DEFAULT_COMPACTION_BUDGET {
            let seg_id = self.next_segment_id()?;
            let writer = SegmentWriter::new(&self.segments_dir, seg_id);
            segments.push(Arc::new(
                writer.build_with_progress(std::mem::take(&mut batch), || {
                    done += 1;
                    on_progress(done, total);
                })?,
            ));
            batch_bytes = 0;
        }
    }

    // Flush remaining files
    if !batch.is_empty() {
        let seg_id = self.next_segment_id()?;
        let writer = SegmentWriter::new(&self.segments_dir, seg_id);
        segments.push(Arc::new(
            writer.build_with_progress(batch, || {
                done += 1;
                on_progress(done, total);
            })?,
        ));
    }

    self.state.publish(segments);
    Ok(())
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p indexrs-core -- test_index_files_with_progress`
Expected: PASS

Run: `cargo test -p indexrs-core`
Expected: All existing tests still pass.

**Step 5: Commit**

```bash
git add indexrs-core/src/segment_manager.rs
git commit -m "feat(segment-manager): add index_files_with_progress for per-file progress reporting"
```

---

### Task 3: Use progress callback in build_index example

**Files:**
- Modify: `indexrs-core/examples/build_index.rs:84-101` (full_build function)

**Step 1: Update `full_build` to use `index_files_with_progress`**

Replace the segment building block:

```rust
fn full_build(dir: &PathBuf, manager: &SegmentManager) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("  Walking directory...");
    let files = walk_and_collect(dir)?;
    let file_count = files.len();
    let total_bytes: u64 = files.iter().map(|f| f.content.len() as u64).sum();
    eprintln!(
        "  Found {} indexable files ({})",
        file_count,
        human_bytes(total_bytes)
    );

    let t_build = Instant::now();
    manager.index_files_with_progress(files, |done, total| {
        if done % 100 == 0 || done == total {
            let pct = done * 100 / total;
            eprint!("\x1b[2K\r  Building segments... {pct}% ({done}/{total})");
            let _ = std::io::stderr().flush();
        }
    })?;
    eprintln!();

    let snap = manager.snapshot();
    eprintln!(
        "  Built {} segment(s) in {:.1?}",
        snap.len(),
        t_build.elapsed()
    );

    Ok(())
}
```

**Step 2: Verify it compiles and runs**

Run: `cargo clippy -p indexrs-core --example build_index -- -D warnings`
Expected: No new warnings (the pre-existing `&PathBuf` warning may remain).

Run: `cargo run -p indexrs-core --example build_index --release -- .`
Expected: You should see inline progress like:
```
=== Full Index Build ===
  Walking directory...
  Filtering files... 100% (1234/1234)
  Found 456 indexable files (12.34 MB)
  Building segments... 78% (356/456)   <-- updates in-place
  Built 1 segment(s) in 1.2s
```

**Step 3: Run the full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

**Step 4: Commit**

```bash
git add indexrs-core/examples/build_index.rs
git commit -m "feat(examples): use progress callback for segment build in build_index"
```
