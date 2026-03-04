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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
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

    /// Whether a compaction operation is currently running.
    is_compacting: AtomicBool,
}

/// RAII guard that sets `is_compacting` back to `false` on drop.
struct CompactingGuard<'a>(&'a AtomicBool);

impl Drop for CompactingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
            is_compacting: AtomicBool::new(false),
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

    /// Returns `true` if a compaction operation is currently running.
    pub fn is_compacting(&self) -> bool {
        self.is_compacting.load(Ordering::Acquire)
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

    /// Build a map from path to [(segment_index, file_id)] for a set of paths.
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

    /// Look up the content hash of each path in the current segments.
    ///
    /// Returns the hash from the **newest** non-tombstoned entry for each path
    /// (highest segment index wins). Used to skip re-indexing files whose
    /// content has not changed.
    fn batch_find_file_hashes(
        segments: &[Arc<Segment>],
        paths: &std::collections::HashSet<String>,
    ) -> std::collections::HashMap<String, [u8; 16]> {
        let mut result: std::collections::HashMap<String, [u8; 16]> =
            std::collections::HashMap::new();

        // Iterate segments in order so later (newer) segments overwrite earlier ones.
        for segment in segments.iter() {
            let reader = segment.metadata_reader();
            let tombstones = segment.load_tombstones().unwrap_or_default();

            for entry in reader.iter_all() {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if paths.contains(&entry.path) && !tombstones.contains(entry.file_id) {
                    result.insert(entry.path, entry.content_hash);
                }
            }
        }

        result
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
    pub fn index_files_with_progress<F: Fn(usize, usize) + Sync + Send>(
        &self,
        files: Vec<InputFile>,
        on_progress: F,
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

        // Phase 3: Build segments in parallel, reporting progress inline
        let done = AtomicUsize::new(0);
        let segments_dir = &self.segments_dir;

        let results: Vec<Result<Arc<Segment>, IndexError>> = id_batches
            .into_par_iter()
            .map(|(seg_id, files)| {
                let writer = SegmentWriter::new(segments_dir, seg_id);
                writer
                    .build_with_progress(files, || {
                        let current = done.fetch_add(1, Ordering::Relaxed) + 1;
                        on_progress(current, total);
                    })
                    .map(Arc::new)
            })
            .collect();

        let new_segments: Vec<Arc<Segment>> = results.into_iter().collect::<Result<Vec<_>, _>>()?;

        // Phase 4: Publish
        let mut segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();
        segments.extend(new_segments);
        self.state.publish(segments);
        Ok(())
    }

    /// Split files into budget-sized batches and build segments in parallel.
    ///
    /// A `max_segment_bytes` of 0 means no limit (single segment).
    fn build_segments_with_budget(
        &self,
        files: Vec<InputFile>,
        max_segment_bytes: usize,
    ) -> Result<Vec<Arc<Segment>>, IndexError> {
        let new_file_count = files.len();

        // Split files into budget-sized batches
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
            return Ok(Vec::new());
        }

        // Pre-allocate segment IDs
        let id_batches: Vec<(SegmentId, Vec<InputFile>)> = batches
            .into_iter()
            .map(|b| self.next_segment_id().map(|id| (id, b)))
            .collect::<Result<Vec<_>, _>>()?;

        let batch_count = id_batches.len();
        tracing::debug!(
            new_file_count,
            batch_count,
            max_segment_bytes,
            "building replacement segments"
        );

        // Build segments in parallel
        let segments_dir = &self.segments_dir;
        let results: Vec<Result<Arc<Segment>, IndexError>> = id_batches
            .into_par_iter()
            .map(|(seg_id, files)| {
                let writer = SegmentWriter::new(segments_dir, seg_id);
                writer.build(files).map(Arc::new)
            })
            .collect();

        results.into_iter().collect::<Result<Vec<_>, _>>()
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
    pub fn apply_changes(
        &self,
        repo_dir: &Path,
        changes: &[ChangeEvent],
    ) -> Result<(), IndexError> {
        self.apply_changes_with_budget(repo_dir, changes, DEFAULT_COMPACTION_BUDGET)
    }

    /// Like [`apply_changes`](Self::apply_changes) but with a custom per-segment
    /// size budget. A `max_segment_bytes` of 0 means no limit (single segment).
    pub fn apply_changes_with_budget(
        &self,
        repo_dir: &Path,
        changes: &[ChangeEvent],
        max_segment_bytes: usize,
    ) -> Result<(), IndexError> {
        if changes.is_empty() {
            return Ok(());
        }

        tracing::info!(change_count = changes.len(), "applying changes");
        let start = std::time::Instant::now();

        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let current_segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();

        // Collect paths that need tombstoning
        let tombstone_paths: std::collections::HashSet<String> = changes
            .iter()
            .filter(|c| tombstone::needs_tombstone(&c.kind))
            .map(|c| c.path.to_string_lossy().to_string())
            .collect();

        // Batch lookup: one pass over all segments
        let tombstone_locations =
            Self::batch_find_files_in_segments(&current_segments, &tombstone_paths);

        // Build tombstone updates from batch results
        let mut tombstone_updates: std::collections::HashMap<usize, TombstoneSet> =
            std::collections::HashMap::new();
        for locations in tombstone_locations.values() {
            for &(seg_idx, file_id) in locations {
                tombstone_updates
                    .entry(seg_idx)
                    .or_default()
                    .insert(file_id);
            }
        }

        // Collect changes that need new entries, then read files in parallel
        let new_file_changes: Vec<&ChangeEvent> = changes
            .iter()
            .filter(|c| tombstone::needs_new_entry(&c.kind))
            .collect();

        // Look up existing content hashes for paths that need new entries,
        // so we can skip files whose content hasn't changed.
        let new_entry_paths: std::collections::HashSet<String> = new_file_changes
            .iter()
            .map(|c| c.path.to_string_lossy().to_string())
            .collect();
        let existing_hashes = Self::batch_find_file_hashes(&current_segments, &new_entry_paths);

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

                // Skip if file is already indexed with the same content hash.
                if !tombstone_paths.contains(&path_str) {
                    let hash = blake3::hash(&content);
                    let hash_16: [u8; 16] = hash.as_bytes()[..16].try_into().unwrap();
                    if existing_hashes.get(&path_str) == Some(&hash_16) {
                        tracing::debug!(path = %path_str, "skipping unchanged file");
                        return None;
                    }
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

        // Build new segments BEFORE writing tombstones to avoid data loss
        // on crash: if we tombstone first and crash before building the
        // replacement segment, those files would be permanently lost.
        let mut updated_segments = current_segments.clone();
        let new_file_count = new_files.len();
        if !new_files.is_empty() {
            let built = self.build_segments_with_budget(new_files, max_segment_bytes)?;
            updated_segments.extend(built);
        }

        // Write tombstones to affected segments (safe now — replacement exists on disk)
        let tombstone_count: u32 = tombstone_updates.values().map(|ts| ts.len()).sum();
        for (seg_idx, new_tombstones) in &tombstone_updates {
            let segment = &current_segments[*seg_idx];
            let mut existing = segment.load_tombstones()?;
            existing.merge(new_tombstones);
            existing.write_to(&segment.dir_path().join("tombstones.bin"))?;
            segment.set_cached_tombstones(existing);
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

    /// Like [`apply_changes`](Self::apply_changes) but emits structured
    /// [`ReindexProgress`](crate::reindex_progress::ReindexProgress) events.
    pub fn apply_changes_with_progress<
        F: Fn(crate::reindex_progress::ReindexProgress) + Send + Sync,
    >(
        &self,
        repo_dir: &Path,
        changes: &[ChangeEvent],
        on_progress: F,
    ) -> Result<(), IndexError> {
        use crate::reindex_progress::ReindexProgress;

        if changes.is_empty() {
            return Ok(());
        }

        tracing::info!(change_count = changes.len(), "applying changes");
        let start = std::time::Instant::now();

        let _guard = match self.write_lock.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => {
                on_progress(ReindexProgress::WaitingForLock);
                self.write_lock.lock().unwrap_or_else(|e| e.into_inner())
            }
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        };
        let current_segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();

        // Collect paths that need tombstoning
        let tombstone_paths: std::collections::HashSet<String> = changes
            .iter()
            .filter(|c| tombstone::needs_tombstone(&c.kind))
            .map(|c| c.path.to_string_lossy().to_string())
            .collect();

        // Batch lookup: one pass over all segments
        let tombstone_locations =
            Self::batch_find_files_in_segments(&current_segments, &tombstone_paths);

        // Build tombstone updates from batch results
        let mut tombstone_updates: std::collections::HashMap<usize, TombstoneSet> =
            std::collections::HashMap::new();
        for locations in tombstone_locations.values() {
            for &(seg_idx, file_id) in locations {
                tombstone_updates
                    .entry(seg_idx)
                    .or_default()
                    .insert(file_id);
            }
        }

        // Collect changes that need new entries, then read files in parallel
        let new_file_changes: Vec<&ChangeEvent> = changes
            .iter()
            .filter(|c| tombstone::needs_new_entry(&c.kind))
            .collect();

        // Look up existing content hashes for paths that need new entries,
        // so we can skip files whose content hasn't changed.
        let new_entry_paths: std::collections::HashSet<String> = new_file_changes
            .iter()
            .map(|c| c.path.to_string_lossy().to_string())
            .collect();
        let existing_hashes = Self::batch_find_file_hashes(&current_segments, &new_entry_paths);

        let total_to_read = new_file_changes.len();
        let files_read = AtomicUsize::new(0);

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

                // Skip if file is already indexed with the same content hash.
                if !tombstone_paths.contains(&path_str) {
                    let hash = blake3::hash(&content);
                    let hash_16: [u8; 16] = hash.as_bytes()[..16].try_into().unwrap();
                    if existing_hashes.get(&path_str) == Some(&hash_16) {
                        tracing::debug!(path = %path_str, "skipping unchanged file");
                        return None;
                    }
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

                let current = files_read.fetch_add(1, Ordering::Relaxed) + 1;
                on_progress(ReindexProgress::PreparingFiles {
                    current,
                    total: total_to_read,
                });

                Some(InputFile {
                    path: path_str,
                    content,
                    mtime,
                })
            })
            .collect();

        // Build new segments BEFORE writing tombstones
        let mut updated_segments = current_segments.clone();
        let new_file_count = new_files.len();
        if !new_files.is_empty() {
            // Split into budget-sized batches
            let mut batches: Vec<Vec<InputFile>> = Vec::new();
            let mut batch: Vec<InputFile> = Vec::new();
            let mut batch_bytes: usize = 0;

            for file in new_files {
                let content_len = file.content.len();
                batch.push(file);
                batch_bytes += content_len;
                if batch_bytes > DEFAULT_COMPACTION_BUDGET {
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

            // Build segments in parallel with progress
            let files_done = AtomicUsize::new(0);
            let files_total = new_file_count;
            let segments_dir = &self.segments_dir;

            let results: Vec<Result<Arc<Segment>, IndexError>> = id_batches
                .into_par_iter()
                .map(|(seg_id, files)| {
                    let writer = SegmentWriter::new(segments_dir, seg_id);
                    writer
                        .build_with_progress(files, || {
                            let done = files_done.fetch_add(1, Ordering::Relaxed) + 1;
                            on_progress(ReindexProgress::BuildingSegment {
                                segment_id: seg_id.0,
                                files_done: done,
                                files_total,
                            });
                        })
                        .map(Arc::new)
                })
                .collect();

            let new_segments: Vec<Arc<Segment>> =
                results.into_iter().collect::<Result<Vec<_>, _>>()?;
            updated_segments.extend(new_segments);
        }

        // Write tombstones
        let tombstone_count: u32 = tombstone_updates.values().map(|ts| ts.len()).sum();
        if tombstone_count > 0 {
            on_progress(ReindexProgress::Tombstoning {
                count: tombstone_count,
            });
        }
        for (seg_idx, new_tombstones) in &tombstone_updates {
            let segment = &current_segments[*seg_idx];
            let mut existing = segment.load_tombstones()?;
            existing.merge(new_tombstones);
            existing.write_to(&segment.dir_path().join("tombstones.bin"))?;
            segment.set_cached_tombstones(existing);
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

        let mut total_entries: u64 = 0;
        let mut total_tombstoned: u64 = 0;

        for segment in snap.iter() {
            if segment.entry_count() == 0 {
                continue;
            }
            let tombstones = match segment.load_tombstones() {
                Ok(ts) => ts,
                Err(_) => continue,
            };
            total_entries += segment.entry_count() as u64;
            total_tombstoned += tombstones.len() as u64;

            // Any single segment with excessive tombstone ratio → compact.
            if tombstones.tombstone_ratio(segment.entry_count()) > DEFAULT_MAX_TOMBSTONE_RATIO {
                return true;
            }
        }

        // Too many segments, but only if the global tombstone ratio is high
        // enough that compaction would meaningfully reduce segment count.
        // A handful of tombstones across 490K files won't free enough space
        // to eliminate a segment — compacting would just reproduce the same layout.
        if snap.len() > DEFAULT_MAX_SEGMENTS
            && total_entries > 0
            && (total_tombstoned as f32 / total_entries as f32) > DEFAULT_MAX_TOMBSTONE_RATIO
        {
            return true;
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
        self.is_compacting.store(true, Ordering::Release);
        let _compacting_guard = CompactingGuard(&self.is_compacting);
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
            let reader = segment.metadata_reader();
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
    fn test_should_compact_too_many_clean_segments_skips() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let manager = SegmentManager::new(&base_dir).unwrap();

        // Add 11 segments (exceeds default threshold of 10) but no tombstones.
        // Compaction with the same budget would produce the same number of
        // segments, so it should NOT trigger.
        for i in 0..11 {
            manager
                .index_files(vec![InputFile {
                    path: format!("file_{i}.rs"),
                    content: format!("fn f_{i}() {{}}").into_bytes(),
                    mtime: 0,
                }])
                .unwrap();
        }

        assert!(!manager.should_compact());
    }

    #[test]
    fn test_should_compact_too_many_segments_few_tombstones_skips() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        let manager = SegmentManager::new(&base_dir).unwrap();

        // Add 11 segments, each with 2 files (22 total entries).
        for i in 0..11 {
            let files: Vec<InputFile> = (0..2)
                .map(|j| {
                    let name = format!("seg{i}_file{j}.rs");
                    let content = format!("fn f_{i}_{j}() {{}}").into_bytes();
                    fs::write(repo_dir.join(&name), &content).unwrap();
                    InputFile {
                        path: name,
                        content,
                        mtime: 0,
                    }
                })
                .collect();
            manager.index_files(files).unwrap();
        }

        // Delete 1 file out of 22 (~4.5% global tombstone ratio, below 30%).
        // The per-segment ratio for segment 0 is 50% (1/2), which triggers
        // the per-segment check — but the global ratio is too low to trigger
        // the segment-count check alone. This test verifies the segment-count
        // path specifically, so we need to ensure it does NOT trigger on low
        // global tombstone ratios.
        //
        // Note: this still returns true because the per-segment check (50% > 30%)
        // fires for the affected segment. That's correct — the two checks are
        // independent. This test documents that the segment-count + low-global-ratio
        // path does NOT contribute to the decision.
        assert_eq!(manager.state.snapshot().len(), 11);

        // With zero tombstones and >10 segments, should NOT compact.
        assert!(!manager.should_compact());

        // Now create a single tombstone. Per-segment ratio = 50% on that
        // segment, which triggers the per-segment check independently.
        let changes = vec![crate::changes::ChangeEvent {
            path: std::path::PathBuf::from("seg0_file0.rs"),
            kind: crate::changes::ChangeKind::Deleted,
        }];
        manager.apply_changes(&repo_dir, &changes).unwrap();

        // Should compact due to per-segment tombstone ratio (50% > 30%),
        // NOT due to the segment-count check.
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
        let reader = snap_after[0].metadata_reader();
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
            let reader = seg.metadata_reader();
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
            let reader = seg.metadata_reader();
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

    #[test]
    fn test_apply_changes_with_progress_reports_events() {
        use crate::reindex_progress::ReindexProgress;

        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path();
        let indexrs_dir = repo_dir.join(".indexrs");
        fs::create_dir_all(indexrs_dir.join("segments")).unwrap();

        // Write a source file.
        fs::write(repo_dir.join("hello.rs"), "fn hello() {}").unwrap();

        let manager = SegmentManager::new(&indexrs_dir).unwrap();

        let changes = vec![ChangeEvent {
            path: PathBuf::from("hello.rs"),
            kind: ChangeKind::Created,
        }];

        let events = std::sync::Mutex::new(Vec::new());
        manager
            .apply_changes_with_progress(repo_dir, &changes, |ev| {
                events.lock().unwrap().push(ev);
            })
            .unwrap();

        let events = events.into_inner().unwrap();
        // Must contain at least a BuildingSegment event.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReindexProgress::BuildingSegment { .. })),
            "expected BuildingSegment event, got: {events:?}"
        );
    }

    #[test]
    fn test_batch_find_files_in_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

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
        let paths: std::collections::HashSet<String> = ["a.rs", "c.rs", "missing.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();

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

    #[test]
    fn test_apply_changes_bulk_creates() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
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
        let base_dir = dir.path().join(".indexrs");
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

    #[test]
    fn test_apply_changes_large_batch_creates_multiple_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
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

    #[test]
    fn test_apply_changes_skips_directories() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
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
        let base_dir = dir.path().join(".indexrs");
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
        let base_dir = dir.path().join(".indexrs");
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
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path();
        let indexrs_dir = repo_dir.join(".indexrs");
        fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        fs::create_dir_all(repo_dir.join("a_dir")).unwrap();

        let manager = SegmentManager::new(&indexrs_dir).unwrap();

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
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        manager.apply_changes(&repo_dir, &[]).unwrap();
        assert_eq!(manager.snapshot().len(), 0);
    }

    #[test]
    fn test_apply_changes_skips_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
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

    #[test]
    fn test_apply_changes_with_progress_skips_unchanged_files() {
        use crate::reindex_progress::ReindexProgress;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Index a file initially.
        fs::write(repo_dir.join("stable.rs"), b"fn stable() {}").unwrap();
        let changes = vec![ChangeEvent {
            path: PathBuf::from("stable.rs"),
            kind: ChangeKind::Created,
        }];
        manager.apply_changes(&repo_dir, &changes).unwrap();
        assert_eq!(manager.snapshot().len(), 1);

        // Apply the same Created event again with progress callback.
        let events = std::sync::Mutex::new(Vec::new());
        manager
            .apply_changes_with_progress(&repo_dir, &changes, |ev| {
                events.lock().unwrap().push(format!("{ev:?}"));
            })
            .unwrap();

        // Should still be 1 segment.
        assert_eq!(
            manager.snapshot().len(),
            1,
            "unchanged file should not create a new segment (with_progress variant)"
        );
    }

    #[test]
    fn test_apply_changes_skips_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Index a file initially.
        fs::write(repo_dir.join("stable.rs"), b"fn stable() {}").unwrap();
        let changes = vec![ChangeEvent {
            path: PathBuf::from("stable.rs"),
            kind: ChangeKind::Created,
        }];
        manager.apply_changes(&repo_dir, &changes).unwrap();

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry_count(), 1);

        // Apply the same Created event again with identical content on disk.
        manager.apply_changes(&repo_dir, &changes).unwrap();

        // Should still be 1 segment — the duplicate was skipped.
        let snap = manager.snapshot();
        assert_eq!(
            snap.len(),
            1,
            "unchanged file should not create a new segment"
        );
    }

    #[test]
    fn test_batch_find_file_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs");

        let manager = SegmentManager::new(&base_dir).unwrap();

        let content = b"fn hello() {}";
        manager
            .index_files(vec![InputFile {
                path: "hello.rs".to_string(),
                content: content.to_vec(),
                mtime: 100,
            }])
            .unwrap();

        let snap = manager.snapshot();
        let paths: std::collections::HashSet<String> =
            ["hello.rs".to_string()].into_iter().collect();
        let hashes = SegmentManager::batch_find_file_hashes(&snap, &paths);

        assert_eq!(hashes.len(), 1);
        let expected_hash = blake3::hash(content);
        let expected_16: [u8; 16] = expected_hash.as_bytes()[..16].try_into().unwrap();
        assert_eq!(hashes["hello.rs"], expected_16);
    }
}
