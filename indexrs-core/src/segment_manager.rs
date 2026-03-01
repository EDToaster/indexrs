//! Segment lifecycle manager with background compaction.
//!
//! [`SegmentManager`] is the primary entry point for indexing operations. It
//! owns the [`IndexState`](crate::index_state::IndexState), tracks active
//! segments, builds new segments from files, applies incremental changes with
//! tombstoning, and compacts fragmented segments.
//!
//! # Concurrency
//!
//! - A writer `Mutex` ensures only one indexing or compaction operation runs
//!   at a time.
//! - Readers call `snapshot()` for a lock-free view of the current segment list.
//! - Compaction can run in the background via `compact_background()`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

use crate::changes::ChangeEvent;
use crate::error::IndexError;
use crate::index_state::{IndexState, SegmentList};
use crate::metadata::FileMetadata;
use crate::segment::{InputFile, Segment, SegmentWriter};
use crate::tombstone::{self, TombstoneSet};
use crate::types::{FileId, SegmentId};

/// Default maximum number of segments before compaction is recommended.
const DEFAULT_MAX_SEGMENTS: usize = 10;

/// Default tombstone ratio threshold for compaction.
const DEFAULT_MAX_TOMBSTONE_RATIO: f32 = 0.30;

/// Default per-segment size budget for compaction (256 MB of uncompressed content).
///
/// This bounds peak memory during compaction to ~256 MB for file content plus
/// overhead for posting lists (~2x content size for positional postings, much
/// less for file-level postings). A 256 MB budget keeps total compaction RAM
/// under ~1 GB on typical codebases.
const DEFAULT_COMPACTION_BUDGET: usize = 256 * 1024 * 1024;

/// Segment lifecycle manager.
///
/// The primary entry point for all indexing operations. Owns the `IndexState`
/// (for snapshot isolation), a writer mutex (serializes indexing/compaction),
/// and a monotonic segment ID counter.
pub struct SegmentManager {
    /// Base directory for the index (e.g. `.indexrs/`). Segments live
    /// under `<base_dir>/segments/`. Reserved for future use (e.g. recovery).
    #[allow(dead_code)]
    base_dir: PathBuf,

    /// The directory where segment subdirectories are created.
    segments_dir: PathBuf,

    /// Atomic counter for assigning monotonically increasing segment IDs.
    next_id: AtomicU32,

    /// The index state holding the current segment list. Readers get
    /// lock-free snapshots; the writer mutex below serializes mutations.
    state: IndexState,

    /// Serializes write operations (add_segment, index_files, apply_changes,
    /// compact). Only one write operation can run at a time.
    write_lock: Mutex<()>,
}

impl SegmentManager {
    /// Create a new segment manager, scanning existing segments from disk.
    ///
    /// If `base_dir` does not exist, it is created along with the `segments/`
    /// subdirectory. Any existing `seg_NNNN/` directories are loaded and the
    /// segment ID counter is set past the highest existing ID.
    ///
    /// # Arguments
    ///
    /// * `base_dir` - The index root directory (e.g. `.indexrs/`).
    ///
    /// # Errors
    ///
    /// Returns `IndexError::Io` if directory creation or segment loading fails.
    pub fn new(base_dir: &Path) -> Result<Self, IndexError> {
        let segments_dir = base_dir.join("segments");
        fs::create_dir_all(&segments_dir)?;

        let state = IndexState::new();
        let mut max_id: u32 = 0;
        let mut segments: Vec<Arc<Segment>> = Vec::new();

        // Scan for existing seg_NNNN directories
        let mut entries: Vec<_> = fs::read_dir(&segments_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("seg_") && e.path().is_dir()
            })
            .collect();

        // Sort by name to load in order
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(id_str) = name.strip_prefix("seg_")
                && let Ok(id) = id_str.parse::<u32>()
            {
                let segment = Segment::open(&entry.path(), SegmentId(id))?;
                if id >= max_id {
                    max_id = id + 1;
                }
                segments.push(Arc::new(segment));
            }
        }

        if !segments.is_empty() {
            tracing::info!(
                segment_count = segments.len(),
                next_id = max_id,
                "loaded existing segments from disk"
            );
            state.publish(segments);
        } else {
            tracing::info!("no existing segments found, starting fresh");
        }

        Ok(SegmentManager {
            base_dir: base_dir.to_path_buf(),
            segments_dir,
            next_id: AtomicU32::new(max_id),
            state,
            write_lock: Mutex::new(()),
        })
    }

    /// Return the next monotonically increasing segment ID.
    ///
    /// Thread-safe via `AtomicU32::fetch_update`. Returns an error if the
    /// counter would overflow `u32::MAX`.
    pub fn next_segment_id(&self) -> Result<SegmentId, IndexError> {
        self.next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(SegmentId)
            .map_err(|_| {
                IndexError::IndexCorruption("segment ID counter overflow (u32::MAX)".to_string())
            })
    }

    /// Take a lock-free snapshot of the current segment list.
    ///
    /// Delegates to `IndexState::snapshot()`. The returned `SegmentList` is
    /// a frozen view that remains valid regardless of concurrent writes.
    pub fn snapshot(&self) -> SegmentList {
        self.state.snapshot()
    }

    /// Add a pre-built segment to the active segment list.
    ///
    /// Acquires the writer lock, appends the segment to the current list,
    /// and publishes the new list atomically.
    pub fn add_segment(&self, segment: Arc<Segment>) {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();
        segments.push(segment);
        self.state.publish(segments);
    }

    /// Find all (segment_index, file_id) pairs for a given relative path
    /// across the current segments.
    ///
    /// Searches segments in order, checking metadata for path matches.
    /// This is used by `apply_changes()` to locate entries that need tombstoning.
    fn find_file_in_segments(segments: &[Arc<Segment>], path: &str) -> Vec<(usize, FileId)> {
        let mut results = Vec::new();
        for (seg_idx, segment) in segments.iter().enumerate() {
            let reader = match segment.metadata_reader() {
                Ok(r) => r,
                Err(_) => continue,
            };
            let tombstones = segment.load_tombstones().unwrap_or_default();

            for entry in reader.iter_all() {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if entry.path == path && !tombstones.contains(entry.file_id) {
                    results.push((seg_idx, entry.file_id));
                }
            }
        }
        results
    }

    /// Index a set of files, splitting into multiple segments when the
    /// accumulated content size exceeds `max_segment_bytes`.
    ///
    /// This bounds peak memory during the build to approximately
    /// `max_segment_bytes` of file content plus overhead for posting lists.
    ///
    /// # Arguments
    ///
    /// * `files` - The files to index.
    /// * `max_segment_bytes` - Soft limit on total uncompressed content bytes
    ///   per segment. A single file larger than this limit will still be placed
    ///   in its own segment. A value of 0 means no limit (single segment).
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
        let results: Vec<Result<Arc<Segment>, IndexError>> = id_batches
            .into_par_iter()
            .map(|(seg_id, files)| {
                let writer = SegmentWriter::new(segments_dir, seg_id);
                writer.build(files).map(Arc::new)
            })
            .collect();
        let new_segments: Vec<Arc<Segment>> = results.into_iter().collect::<Result<Vec<_>, _>>()?;

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

    /// Index a set of files into the index.
    ///
    /// Uses [`DEFAULT_COMPACTION_BUDGET`] to split large inputs into
    /// multiple capped segments, bounding peak memory.
    pub fn index_files(&self, files: Vec<InputFile>) -> Result<(), IndexError> {
        self.index_files_with_budget(files, DEFAULT_COMPACTION_BUDGET)
    }

    /// Index files with a progress callback.
    ///
    /// Behaves identically to [`index_files`](Self::index_files) but calls
    /// `on_progress(files_done, files_total)` after each file is processed
    /// during segment building.
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

        // Phase 3: Build segments in parallel with atomic progress counter
        let done = AtomicUsize::new(0);
        let segments_dir = &self.segments_dir;

        let results: Vec<Result<Arc<Segment>, IndexError>> = id_batches
            .into_par_iter()
            .map(|(seg_id, files)| {
                let writer = SegmentWriter::new(segments_dir, seg_id);
                writer
                    .build_with_progress(files, || {
                        done.fetch_add(1, Ordering::Relaxed);
                    })
                    .map(Arc::new)
            })
            .collect();

        let new_segments: Vec<Arc<Segment>> = results.into_iter().collect::<Result<Vec<_>, _>>()?;

        // Report progress sequentially (callback is FnMut, not Sync)
        let final_done = done.load(Ordering::Relaxed);
        for i in 1..=final_done {
            on_progress(i, total);
        }

        // Phase 4: Publish
        let mut segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();
        segments.extend(new_segments);
        self.state.publish(segments);
        Ok(())
    }

    /// Apply a batch of file change events to the index.
    ///
    /// For each change:
    /// - **Modified/Deleted/Renamed**: tombstones the old entry in whatever
    ///   segment currently holds it.
    /// - **Created/Modified/Renamed**: reads the file from `repo_dir` and
    ///   includes it in a new segment.
    ///
    /// If any files need new entries, a new segment is built and published.
    /// Tombstones are written to the affected segments' `tombstones.bin` files.
    ///
    /// # Arguments
    ///
    /// * `repo_dir` - The repository root directory for reading file contents.
    /// * `changes` - The list of change events to process.
    ///
    /// # Errors
    ///
    /// Returns `IndexError` if reading files, building segments, or writing
    /// tombstones fails.
    pub fn apply_changes(
        &self,
        repo_dir: &Path,
        changes: &[ChangeEvent],
    ) -> Result<(), IndexError> {
        if changes.is_empty() {
            return Ok(());
        }

        tracing::info!(change_count = changes.len(), "applying changes");
        let start = std::time::Instant::now();

        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let current_segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();

        // Track tombstones to write per segment index
        let mut tombstone_updates: std::collections::HashMap<usize, TombstoneSet> =
            std::collections::HashMap::new();

        // Collect files that need new entries
        let mut new_files: Vec<InputFile> = Vec::new();

        for change in changes {
            let path_str = change.path.to_string_lossy().to_string();

            // Tombstone old entries if needed
            if tombstone::needs_tombstone(&change.kind) {
                let locations = Self::find_file_in_segments(&current_segments, &path_str);
                for (seg_idx, file_id) in locations {
                    tombstone_updates
                        .entry(seg_idx)
                        .or_default()
                        .insert(file_id);
                }
            }

            // Read new content if needed
            if tombstone::needs_new_entry(&change.kind) {
                // Finding 8: Validate path to prevent path traversal attacks.
                // Since the file may not exist yet we cannot canonicalize it,
                // so we check that the path has no `..` components and is not absolute.
                let has_dotdot = change
                    .path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir);
                if has_dotdot || change.path.is_absolute() {
                    tracing::warn!(
                        path = %change.path.display(),
                        "skipping change with potentially unsafe path (contains '..' or is absolute)"
                    );
                    continue;
                }

                let full_path = repo_dir.join(&change.path);
                if full_path.exists() {
                    let content = fs::read(&full_path)?;

                    // Finding 9: Skip binary files and files exceeding the size limit.
                    if !crate::binary::should_index_file(&full_path, &content, 1_048_576) {
                        continue;
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
                    new_files.push(InputFile {
                        path: path_str,
                        content,
                        mtime,
                    });
                }
            }
        }

        // Build new segment BEFORE writing tombstones to avoid data loss
        // on crash: if we tombstone first and crash before building the
        // replacement segment, those files would be permanently lost.
        let mut updated_segments = current_segments.clone();
        let new_file_count = new_files.len();
        if !new_files.is_empty() {
            let seg_id = self.next_segment_id()?;
            tracing::debug!(
                segment_id = seg_id.0,
                new_file_count,
                "building replacement segment"
            );
            let writer = SegmentWriter::new(&self.segments_dir, seg_id);
            let segment = writer.build(new_files)?;
            updated_segments.push(Arc::new(segment));
        }

        // Write tombstones to affected segments (safe now — replacement exists on disk)
        let tombstone_count: u32 = tombstone_updates.values().map(|ts| ts.len()).sum();
        for (seg_idx, new_tombstones) in &tombstone_updates {
            let segment = &current_segments[*seg_idx];
            let mut existing = segment.load_tombstones()?;
            existing.merge(new_tombstones);
            existing.write_to(&segment.dir_path().join("tombstones.bin"))?;
        }

        self.state.publish(updated_segments);

        tracing::info!(
            change_count = changes.len(),
            tombstone_count,
            new_file_count,
            segments_affected = tombstone_updates.len(),
            elapsed_ms = start.elapsed().as_millis() as u64,
            "changes applied"
        );
        Ok(())
    }

    /// Check whether the index should be compacted.
    ///
    /// Returns `true` if:
    /// - The number of segments exceeds the threshold (default 10), or
    /// - Any segment's tombstone ratio exceeds the threshold (default 30%).
    pub fn should_compact(&self) -> bool {
        let snap = self.state.snapshot();

        if snap.len() > DEFAULT_MAX_SEGMENTS {
            return true;
        }

        for segment in snap.iter() {
            if segment.entry_count() == 0 {
                continue;
            }
            let tombstones = match segment.load_tombstones() {
                Ok(ts) => ts,
                Err(_) => continue,
            };
            if tombstones.tombstone_ratio(segment.entry_count()) > DEFAULT_MAX_TOMBSTONE_RATIO {
                return true;
            }
        }

        false
    }

    /// Compact all segments into a single new segment.
    ///
    /// Reads all non-tombstoned entries from every segment, builds a new
    /// merged segment via `SegmentWriter`, atomically swaps the segment list,
    /// then deletes the old segment directories.
    ///
    /// This is a no-op if there are 0 segments, or if there is exactly 1
    /// segment with no tombstones.
    ///
    /// The core logic is synchronous and testable. For background execution,
    /// use `compact_background()`.
    ///
    /// # Errors
    ///
    /// Returns `IndexError` if reading segments, building the merged segment,
    /// or deleting old directories fails.
    pub fn compact(&self) -> Result<(), IndexError> {
        self.compact_with_budget(0)
    }

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

        // Phase 1: Collect all live (non-tombstoned) entries with their segment index.
        // This is cheap — metadata is already memory-mapped.
        let mut live_entries: Vec<(usize, FileMetadata)> = Vec::new();
        for (seg_idx, segment) in current_segments.iter().enumerate() {
            let tombstones = segment.load_tombstones()?;
            let reader = segment.metadata_reader()?;
            for entry_result in reader.iter_all() {
                let entry: FileMetadata = entry_result?;
                if !tombstones.contains(entry.file_id) {
                    live_entries.push((seg_idx, entry));
                }
            }
        }

        // Phase 2: Decompress content in parallel using rayon.
        // Each decompression is independent and CPU-bound (zstd decode).
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

        // Phase 3: Budget-batched segment writing (sequential).
        let mut batch: Vec<InputFile> = Vec::new();
        let mut batch_bytes: usize = 0;
        let mut new_segments: Vec<Arc<Segment>> = Vec::new();

        for file in input_files {
            let content_len = file.content.len();
            batch.push(file);
            batch_bytes += content_len;

            // Flush if over budget (0 means unlimited)
            if max_segment_bytes > 0 && batch_bytes > max_segment_bytes {
                let seg_id = self.next_segment_id()?;
                let writer = SegmentWriter::new(&self.segments_dir, seg_id);
                new_segments.push(Arc::new(writer.build(std::mem::take(&mut batch))?));
                batch_bytes = 0;
            }
        }

        // Flush remaining batch
        if !batch.is_empty() {
            let seg_id = self.next_segment_id()?;
            let writer = SegmentWriter::new(&self.segments_dir, seg_id);
            new_segments.push(Arc::new(writer.build(batch)?));
        }

        let old_dirs: Vec<PathBuf> = current_segments
            .iter()
            .map(|s| s.dir_path().to_path_buf())
            .collect();

        let output_segment_count = new_segments.len();
        self.state.publish(new_segments);

        for old_dir in &old_dirs {
            if let Err(e) = fs::remove_dir_all(old_dir) {
                tracing::warn!(path = %old_dir.display(), error = %e, "failed to remove old segment directory");
            }
        }

        tracing::info!(
            input_segments = old_dirs.len(),
            output_segments = output_segment_count,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "compaction complete"
        );
        Ok(())
    }

    /// Run compaction in the background via `tokio::spawn`.
    ///
    /// Returns a `JoinHandle` that resolves to the compaction result.
    /// The caller can `await` or ignore it.
    ///
    /// # Panics
    ///
    /// The `SegmentManager` must be wrapped in an `Arc` for this method
    /// to work, since the spawned task needs a `'static` reference.
    pub fn compact_background(self: &Arc<Self>) -> tokio::task::JoinHandle<Result<(), IndexError>> {
        tracing::info!("spawning background compaction task");
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = this.compact_with_budget(DEFAULT_COMPACTION_BUDGET);
            if let Err(ref e) = result {
                tracing::error!(error = %e, "background compaction failed");
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::ChangeKind;
    use std::path::PathBuf;

    #[test]
    fn test_new_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

        let manager = SegmentManager::new(&base_dir).unwrap();
        let snap = manager.snapshot();
        assert!(snap.is_empty());
    }

    #[test]
    fn test_next_segment_id_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

        let manager = SegmentManager::new(&base_dir).unwrap();
        let id0 = manager.next_segment_id().unwrap();
        let id1 = manager.next_segment_id().unwrap();
        let id2 = manager.next_segment_id().unwrap();

        assert_eq!(id0, SegmentId(0));
        assert_eq!(id1, SegmentId(1));
        assert_eq!(id2, SegmentId(2));
    }

    #[test]
    fn test_add_segment() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let segments_dir = base_dir.join("segments");
        fs::create_dir_all(&segments_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Build a segment externally and add it
        let seg_id = manager.next_segment_id().unwrap();
        let writer = SegmentWriter::new(&segments_dir, seg_id);
        let segment = writer
            .build(vec![InputFile {
                path: "a.rs".to_string(),
                content: b"fn a() {}".to_vec(),
                mtime: 0,
            }])
            .unwrap();

        manager.add_segment(Arc::new(segment));

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].segment_id(), SegmentId(0));
        assert_eq!(snap[0].entry_count(), 1);
    }

    #[test]
    fn test_index_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

        let manager = SegmentManager::new(&base_dir).unwrap();

        let files = vec![
            InputFile {
                path: "src/main.rs".to_string(),
                content: b"fn main() { println!(\"hello\"); }".to_vec(),
                mtime: 1700000000,
            },
            InputFile {
                path: "src/lib.rs".to_string(),
                content: b"pub fn add(a: i32, b: i32) -> i32 { a + b }".to_vec(),
                mtime: 1700000001,
            },
        ];

        manager.index_files(files).unwrap();

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry_count(), 2);

        // Verify metadata accessible
        let meta = snap[0].get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(meta.path, "src/main.rs");
    }

    #[test]
    fn test_index_files_multiple_calls() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

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

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].segment_id(), SegmentId(0));
        assert_eq!(snap[1].segment_id(), SegmentId(1));
    }

    #[test]
    fn test_index_files_empty() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

        let manager = SegmentManager::new(&base_dir).unwrap();
        manager.index_files(vec![]).unwrap();

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 0); // no segment created for empty input
    }

    // ---- apply_changes tests ----

    #[test]
    fn test_apply_changes_create() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        // Write a file to the "repo"
        fs::write(repo_dir.join("new.rs"), b"fn new() {}").unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        let changes = vec![ChangeEvent {
            path: PathBuf::from("new.rs"),
            kind: ChangeKind::Created,
        }];

        manager.apply_changes(&repo_dir, &changes).unwrap();

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry_count(), 1);

        let meta = snap[0].get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(meta.path, "new.rs");
    }

    #[test]
    fn test_apply_changes_modify() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // First, index the original file
        fs::write(repo_dir.join("a.rs"), b"fn a() {}").unwrap();
        manager
            .index_files(vec![InputFile {
                path: "a.rs".to_string(),
                content: b"fn a() {}".to_vec(),
                mtime: 100,
            }])
            .unwrap();

        // Now modify the file on disk
        fs::write(repo_dir.join("a.rs"), b"fn a_updated() {}").unwrap();

        let changes = vec![ChangeEvent {
            path: PathBuf::from("a.rs"),
            kind: ChangeKind::Modified,
        }];

        manager.apply_changes(&repo_dir, &changes).unwrap();

        let snap = manager.snapshot();
        // Should have 2 segments: original + new
        assert_eq!(snap.len(), 2);

        // The old entry in segment 0 should be tombstoned
        let ts = snap[0].load_tombstones().unwrap();
        assert_eq!(ts.len(), 1);
        assert!(ts.contains(FileId(0)));

        // The new segment should have the updated file
        let meta = snap[1].get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(meta.path, "a.rs");
    }

    #[test]
    fn test_apply_changes_delete() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Index original file
        manager
            .index_files(vec![InputFile {
                path: "a.rs".to_string(),
                content: b"fn a() {}".to_vec(),
                mtime: 100,
            }])
            .unwrap();

        let changes = vec![ChangeEvent {
            path: PathBuf::from("a.rs"),
            kind: ChangeKind::Deleted,
        }];

        manager.apply_changes(&repo_dir, &changes).unwrap();

        let snap = manager.snapshot();
        // Still 1 segment, but the file is tombstoned
        assert_eq!(snap.len(), 1);
        let ts = snap[0].load_tombstones().unwrap();
        assert_eq!(ts.len(), 1);
        assert!(ts.contains(FileId(0)));
    }

    #[test]
    fn test_apply_changes_mixed() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Index two files
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

        // Create a new file, modify b.rs, delete a.rs
        fs::write(repo_dir.join("c.rs"), b"fn c() {}").unwrap();
        fs::write(repo_dir.join("b.rs"), b"fn b_v2() {}").unwrap();

        let changes = vec![
            ChangeEvent {
                path: PathBuf::from("a.rs"),
                kind: ChangeKind::Deleted,
            },
            ChangeEvent {
                path: PathBuf::from("b.rs"),
                kind: ChangeKind::Modified,
            },
            ChangeEvent {
                path: PathBuf::from("c.rs"),
                kind: ChangeKind::Created,
            },
        ];

        manager.apply_changes(&repo_dir, &changes).unwrap();

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 2); // original + new

        // a.rs and b.rs should be tombstoned in segment 0
        let ts = snap[0].load_tombstones().unwrap();
        assert_eq!(ts.len(), 2);
        assert!(ts.contains(FileId(0))); // a.rs
        assert!(ts.contains(FileId(1))); // b.rs

        // New segment should have b.rs (updated) and c.rs (created)
        assert_eq!(snap[1].entry_count(), 2);
    }

    // ---- should_compact and compact tests ----

    #[test]
    fn test_should_compact_empty() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let manager = SegmentManager::new(&base_dir).unwrap();
        assert!(!manager.should_compact());
    }

    #[test]
    fn test_should_compact_too_many_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let manager = SegmentManager::new(&base_dir).unwrap();

        // Add 11 segments (exceeds default threshold of 10)
        for i in 0..11 {
            manager
                .index_files(vec![InputFile {
                    path: format!("file_{i}.rs"),
                    content: format!("fn f_{i}() {{}}").into_bytes(),
                    mtime: 0,
                }])
                .unwrap();
        }

        assert!(manager.should_compact());
    }

    #[test]
    fn test_should_compact_high_tombstone_ratio() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Index 3 files
        let files: Vec<InputFile> = (0..3)
            .map(|i| InputFile {
                path: format!("f{i}.rs"),
                content: format!("fn f{i}() {{}}").into_bytes(),
                mtime: 0,
            })
            .collect();
        manager.index_files(files).unwrap();

        // Delete all 3 -- tombstone ratio = 3/3 = 100% > 30%
        let changes: Vec<ChangeEvent> = (0..3)
            .map(|i| ChangeEvent {
                path: PathBuf::from(format!("f{i}.rs")),
                kind: ChangeKind::Deleted,
            })
            .collect();
        manager.apply_changes(&repo_dir, &changes).unwrap();

        assert!(manager.should_compact());
    }

    #[test]
    fn test_compact_merges_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let manager = SegmentManager::new(&base_dir).unwrap();

        // Create 3 segments with 1 file each
        for i in 0..3 {
            manager
                .index_files(vec![InputFile {
                    path: format!("file_{i}.rs"),
                    content: format!("fn func_{i}() {{ let x = {i}; }}").into_bytes(),
                    mtime: 1700000000 + i as u64,
                }])
                .unwrap();
        }

        let snap_before = manager.snapshot();
        assert_eq!(snap_before.len(), 3);

        // Compact all segments
        manager.compact().unwrap();

        let snap_after = manager.snapshot();
        assert_eq!(snap_after.len(), 1);
        assert_eq!(snap_after[0].entry_count(), 3);

        // Verify all files are accessible
        let reader = snap_after[0].metadata_reader().unwrap();
        let all: Vec<_> = reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap();
        let paths: Vec<&str> = all.iter().map(|m| m.path.as_str()).collect();
        assert!(paths.contains(&"file_0.rs"));
        assert!(paths.contains(&"file_1.rs"));
        assert!(paths.contains(&"file_2.rs"));
    }

    #[test]
    fn test_compact_excludes_tombstoned() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Index 2 files
        manager
            .index_files(vec![
                InputFile {
                    path: "keep.rs".to_string(),
                    content: b"fn keep() {}".to_vec(),
                    mtime: 0,
                },
                InputFile {
                    path: "delete_me.rs".to_string(),
                    content: b"fn delete_me() {}".to_vec(),
                    mtime: 0,
                },
            ])
            .unwrap();

        // Delete one file
        let changes = vec![ChangeEvent {
            path: PathBuf::from("delete_me.rs"),
            kind: ChangeKind::Deleted,
        }];
        manager.apply_changes(&repo_dir, &changes).unwrap();

        // Compact
        manager.compact().unwrap();

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry_count(), 1); // only keep.rs

        let meta = snap[0].get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(meta.path, "keep.rs");
    }

    #[test]
    fn test_compact_cleans_old_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let segments_dir = base_dir.join("segments");

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Create 2 segments
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

        // Before compact: seg_0000 and seg_0001 exist
        assert!(segments_dir.join("seg_0000").exists());
        assert!(segments_dir.join("seg_0001").exists());

        manager.compact().unwrap();

        // After compact: old dirs removed, new one exists
        assert!(!segments_dir.join("seg_0000").exists());
        assert!(!segments_dir.join("seg_0001").exists());

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap[0].dir_path().exists());
    }

    #[test]
    fn test_compact_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let manager = SegmentManager::new(&base_dir).unwrap();

        // Compacting an empty index should be a no-op
        manager.compact().unwrap();

        let snap = manager.snapshot();
        assert!(snap.is_empty());
    }

    #[test]
    fn test_compact_single_segment_no_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let manager = SegmentManager::new(&base_dir).unwrap();

        manager
            .index_files(vec![InputFile {
                path: "a.rs".to_string(),
                content: b"fn a() {}".to_vec(),
                mtime: 0,
            }])
            .unwrap();

        // Compacting a single segment with no tombstones is a no-op
        manager.compact().unwrap();

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry_count(), 1);
    }

    // ---- compact_background test ----

    #[tokio::test]
    async fn test_compact_background() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

        let manager = Arc::new(SegmentManager::new(&base_dir).unwrap());

        // Create 3 segments
        for i in 0..3 {
            manager
                .index_files(vec![InputFile {
                    path: format!("file_{i}.rs"),
                    content: format!("fn func_{i}() {{}}").into_bytes(),
                    mtime: 0,
                }])
                .unwrap();
        }

        assert_eq!(manager.snapshot().len(), 3);

        // Compact in background
        let handle = manager.compact_background();
        handle.await.unwrap().unwrap();

        assert_eq!(manager.snapshot().len(), 1);
        assert_eq!(manager.snapshot()[0].entry_count(), 3);
    }

    // ---- reopen tests ----

    #[test]
    fn test_reopen_existing_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

        // Create a manager and index some files
        {
            let manager = SegmentManager::new(&base_dir).unwrap();
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
                        mtime: 200,
                    },
                ])
                .unwrap();

            manager
                .index_files(vec![InputFile {
                    path: "c.rs".to_string(),
                    content: b"fn c() {}".to_vec(),
                    mtime: 300,
                }])
                .unwrap();

            let snap = manager.snapshot();
            assert_eq!(snap.len(), 2);
        }
        // Manager dropped here

        // Reopen and verify segments are loaded
        let manager2 = SegmentManager::new(&base_dir).unwrap();
        let snap = manager2.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].segment_id(), SegmentId(0));
        assert_eq!(snap[1].segment_id(), SegmentId(1));
        assert_eq!(snap[0].entry_count(), 2);
        assert_eq!(snap[1].entry_count(), 1);

        // next_segment_id should be past the highest existing
        let next = manager2.next_segment_id().unwrap();
        assert_eq!(next, SegmentId(2));
    }

    #[test]
    fn test_compact_with_budget_produces_multiple_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
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

    #[test]
    fn test_reopen_after_compact() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

        {
            let manager = SegmentManager::new(&base_dir).unwrap();
            for i in 0..3 {
                manager
                    .index_files(vec![InputFile {
                        path: format!("file_{i}.rs"),
                        content: format!("fn f{i}() {{}}").into_bytes(),
                        mtime: 0,
                    }])
                    .unwrap();
            }
            manager.compact().unwrap();
        }

        let manager2 = SegmentManager::new(&base_dir).unwrap();
        let snap = manager2.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry_count(), 3);
    }

    // ---- compact_with_budget edge case tests ----

    #[test]
    fn test_compact_with_budget_zero_means_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
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
        let base_dir = dir.path().join(".indexrs");
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
        let base_dir = dir.path().join(".indexrs");
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
        let base_dir = dir.path().join(".indexrs");
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
        let base_dir = dir.path().join(".indexrs");
        let manager = SegmentManager::new(&base_dir).unwrap();

        manager.compact_with_budget(1024).unwrap();

        let snap = manager.snapshot();
        assert!(snap.is_empty());
    }

    #[test]
    fn test_compact_with_budget_single_segment_no_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
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
        let base_dir = dir.path().join(".indexrs");
        let manager = SegmentManager::new(&base_dir).unwrap();

        for i in 0..4 {
            manager
                .index_files(vec![InputFile {
                    path: format!("file_{i}.rs"),
                    content: format!("fn shared_func_{i}() {{ let result = compute(); }}")
                        .into_bytes(),
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

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
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
}
