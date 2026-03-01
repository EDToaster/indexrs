# Parallel Segment Build Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Parallelize the per-file processing in `SegmentWriter::build_inner()` using rayon to utilize multiple CPU cores during blake3 hashing, trigram extraction, and zstd compression.

**Architecture:** The current `build_inner()` loop processes each file sequentially: hash, detect language, count lines, extract trigrams, compress content, record metadata. The CPU-bound work (hash, trigram extraction, compression) is independent per file and can run in parallel. The key constraint is that `ContentStoreWriter` writes sequentially to a file, so compressed blobs must be written in order. The solution: use `rayon::par_iter()` to compute per-file results (hash, language, line count, unique trigrams, compressed bytes) in parallel, collect into a Vec, then iterate sequentially to write compressed content and feed the posting list builder and metadata builder.

**Tech Stack:** Rust, `rayon` crate (already a dependency of `indexrs-core`).

---

### Task 1: Refactor build_inner to use a two-phase approach

**Files:**
- Modify: `indexrs-core/src/segment.rs:276-352` (the `build_inner` method)

**Step 1: Define a struct for per-file processed results**

Add a private struct inside the method (or at module level) to hold the results of parallel per-file processing:

```rust
/// Per-file results computed in parallel during segment build.
struct ProcessedFile {
    /// Index in the original input file list (becomes the FileId).
    index: u32,
    /// blake3 hash truncated to 16 bytes.
    content_hash: [u8; 16],
    /// Detected language from file path.
    language: Language,
    /// Number of newline bytes in content.
    line_count: u32,
    /// Zstd-compressed content bytes.
    compressed: Vec<u8>,
}
```

**Step 2: Replace the sequential loop with parallel map + sequential write**

Rewrite `build_inner` to:
1. Use `rayon::prelude::*` and `files.par_iter().enumerate().map(...)` to compute `ProcessedFile` for each input in parallel (hash, language, line_count, compress).
2. Collect into `Vec<ProcessedFile>` (preserves order since `par_iter` on a slice + `collect` is order-preserving with rayon's indexed iterators).
3. Iterate the collected results sequentially to: write compressed bytes to `ContentStoreWriter`, add trigrams to `PostingListBuilder`, add metadata to `MetadataBuilder`, call `on_file_done()`.

The key insight: trigram extraction feeds `PostingListBuilder::add_file()` which takes `&mut self`, so it cannot be called in parallel. However, the trigram extraction itself is just CPU work on the content bytes. Since `PostingListBuilder::add_file()` both extracts trigrams AND inserts them into the HashMap, we keep that call sequential. The big parallel wins are blake3 hashing and zstd compression, which are the most expensive per-file operations.

```rust
fn build_inner<F: FnMut()>(
    &self,
    temp_dir: &Path,
    final_dir: &Path,
    files: Vec<InputFile>,
    mut on_file_done: F,
) -> Result<Segment, IndexError> {
    use rayon::prelude::*;

    let mut posting_builder = PostingListBuilder::file_only();
    let mut metadata_builder = MetadataBuilder::new();
    let mut content_writer =
        ContentStoreWriter::new(&temp_dir.join("content.zst")).map_err(IndexError::Io)?;

    // Phase 1: Parallel per-file processing (hash + compress)
    let processed: Vec<ProcessedFile> = files
        .par_iter()
        .enumerate()
        .map(|(i, input)| {
            // Hash content with blake3, truncate to 16 bytes
            let hash = blake3::hash(&input.content);
            let mut content_hash = [0u8; 16];
            content_hash.copy_from_slice(&hash.as_bytes()[..16]);

            // Detect language from path
            let language = Language::from_path(Path::new(&input.path));

            // Count lines
            let line_count = input.content.iter().filter(|&&b| b == b'\n').count() as u32;

            // Compress content with zstd
            let compressed = zstd::bulk::compress(&input.content, 3)
                .expect("zstd compression should not fail on valid input");

            ProcessedFile {
                index: i as u32,
                content_hash,
                language,
                line_count,
                compressed,
            }
        })
        .collect();

    // Phase 2: Sequential writes (must maintain offset ordering)
    for (i, (input, proc)) in files.iter().zip(processed.iter()).enumerate() {
        let file_id = FileId(u32::try_from(i).map_err(|_| {
            IndexError::IndexCorruption("too many files for segment (>4B)".to_string())
        })?);

        // Add to trigram posting lists (mutates HashMap, must be sequential)
        posting_builder.add_file(file_id, &input.content);

        // Write pre-compressed content and get (offset, compressed_len)
        let compressed_len: u32 = proc.compressed.len().try_into().map_err(|_| {
            IndexError::Io(std::io::Error::other(format!(
                "compressed block size {} exceeds u32::MAX",
                proc.compressed.len()
            )))
        })?;
        let (content_offset, content_len) = content_writer
            .add_raw(&proc.compressed)
            .map_err(IndexError::Io)?;

        // Add metadata entry
        metadata_builder.add_file(FileMetadata {
            file_id,
            path: input.path.clone(),
            content_hash: proc.content_hash,
            language: proc.language,
            size_bytes: u32::try_from(input.content.len()).unwrap_or(u32::MAX),
            mtime_epoch_secs: input.mtime,
            line_count: proc.line_count,
            content_offset,
            content_len,
        });

        on_file_done();
    }

    // ... rest unchanged (finalize, write trigrams.bin, meta.bin, etc.)
}
```

**Step 3: Add `add_raw` method to `ContentStoreWriter`**

The parallel phase pre-compresses content, so we need a method on `ContentStoreWriter` to write already-compressed bytes (skipping double compression). Add to `content.rs`:

```rust
/// Write already-compressed content to the store.
///
/// Unlike [`add_content`], this method does not compress the input —
/// the caller is responsible for providing zstd-compressed bytes.
/// Returns `(offset, compressed_len)` like `add_content`.
pub fn add_raw(&mut self, compressed: &[u8]) -> std::io::Result<(u64, u32)> {
    let offset = self.current_offset;
    let compressed_len: u32 = compressed.len().try_into().map_err(|_| {
        std::io::Error::other(format!(
            "compressed block size {} exceeds u32::MAX",
            compressed.len()
        ))
    })?;

    self.writer.write_all(compressed)?;
    self.current_offset += compressed_len as u64;

    Ok((offset, compressed_len))
}
```

**Step 4: Run tests to verify correctness**

Run: `cargo test -p indexrs-core -- segment`
Expected: All existing segment tests pass — the refactor is behavior-preserving.

**Step 5: Run clippy and fmt**

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: No warnings, formatting clean.

**Step 6: Commit**

```bash
git add indexrs-core/src/segment.rs indexrs-core/src/content.rs
git commit -m "perf: parallelize segment build with rayon

Use rayon to parallelize per-file blake3 hashing and zstd compression
in SegmentWriter::build_inner(). Trigram extraction and sequential
writes remain on the main thread to maintain offset ordering and
PostingListBuilder's &mut self requirement.

Add ContentStoreWriter::add_raw() for writing pre-compressed blobs."
```
