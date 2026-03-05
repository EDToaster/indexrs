# Segment Manager and Background Compaction Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a `SegmentManager` that owns the full segment lifecycle -- scanning existing segments from disk, building new segments from files, applying incremental file changes with tombstoning, publishing segment lists via `IndexState` for lock-free reads, and compacting fragmented segments in the background.

**Architecture:** A single new module `segment_manager.rs` in `ferret-indexer-core` containing `SegmentManager`. It wraps an `IndexState` (from `index_state.rs`) for snapshot isolation, uses a `Mutex` to serialize writers, and maintains an `AtomicU32` counter for monotonically increasing segment IDs. The manager delegates to the existing `SegmentWriter` for building segments (both fresh and compacted), uses `TombstoneSet` for marking stale entries, and reads metadata via `Segment::get_metadata()` for path-based lookups during change application. Compaction reads all non-tombstoned entries from N segments, builds a single new segment via `SegmentWriter`, atomically swaps the segment list, then deletes old segment directories. The core compaction logic is synchronous and testable; `compact_background()` wraps it in `tokio::spawn`. This plan depends on `index_state.rs` existing (from the multi-segment-query plan HHC-43). If `IndexState` does not exist yet when this plan is executed, Task 1 creates a minimal version.

**Tech Stack:** Rust 2024, `std::sync::{Arc, Mutex, atomic::AtomicU32}`, `tokio::spawn` (for background compaction), existing `ferret-indexer-core` modules (segment, tombstone, index_state, intersection, metadata, content, types, error, changes), `tempfile` (dev)

---

## Task 1: Create `index_state.rs` if it does not exist, or verify it does

**Files:**
- Create (if missing): `ferret-indexer-core/src/index_state.rs`
- Modify (if creating): `ferret-indexer-core/src/lib.rs`

This task is **conditional**. It may have already been created by the multi-segment-query plan (HHC-43). Check first.

### Step 1: Check if `index_state.rs` exists

Run: `ls ferret-indexer-core/src/index_state.rs 2>/dev/null && echo "EXISTS" || echo "MISSING"`

**If EXISTS:** Skip to Step 5 (verify it compiles).

**If MISSING:** Continue to Step 2.

### Step 2: Create the module

Create `ferret-indexer-core/src/index_state.rs`:

```rust
//! Index state management with snapshot isolation.
//!
//! [`IndexState`] holds the current list of active segments as an
//! `Arc<Vec<Arc<Segment>>>`. Readers call [`snapshot()`](IndexState::snapshot)
//! to get a consistent, lock-free view. Writers call
//! [`publish()`](IndexState::publish) to atomically swap in a new segment list.
//!
//! The `SegmentList` type alias provides a convenient name for the snapshot type.

use std::sync::{Arc, Mutex};

use crate::segment::Segment;

/// A snapshot of the active segment list. Lock-free for readers via `Arc::clone()`.
pub type SegmentList = Arc<Vec<Arc<Segment>>>;

/// Manages the current set of active segments with snapshot isolation.
///
/// Readers call [`snapshot()`](Self::snapshot) to get a consistent `SegmentList`
/// (just an `Arc::clone()`, no locks). Writers call [`publish()`](Self::publish)
/// to atomically swap in a new segment list; a `Mutex` serializes writers.
///
/// # Concurrency Model
///
/// - **Readers**: Lock-free. `snapshot()` clones the outer `Arc`, giving a
///   consistent view even if the writer publishes a new list concurrently.
///   Old snapshots remain valid until all references are dropped.
///
/// - **Writers**: Serialized by an internal `Mutex`. Only one thread can call
///   `publish()` at a time. The actual swap is an `Arc` store, so readers
///   never block.
pub struct IndexState {
    /// The current segment list, wrapped in Arc for lock-free snapshot reads.
    /// The Mutex serializes writers; readers never take the lock.
    current: Mutex<SegmentList>,
}

impl IndexState {
    /// Create a new IndexState with an empty segment list.
    pub fn new() -> Self {
        IndexState {
            current: Mutex::new(Arc::new(Vec::new())),
        }
    }

    /// Take a snapshot of the current segment list.
    ///
    /// This is a cheap `Arc::clone()` -- no locks, no copies. The returned
    /// `SegmentList` is a frozen view that remains valid regardless of
    /// subsequent `publish()` calls.
    pub fn snapshot(&self) -> SegmentList {
        let guard = self.current.lock().unwrap();
        Arc::clone(&guard)
    }

    /// Atomically replace the segment list with a new one.
    ///
    /// Only one writer can publish at a time (serialized by internal Mutex).
    /// Existing snapshots are unaffected -- they hold their own `Arc` references.
    pub fn publish(&self, new_segments: Vec<Arc<Segment>>) {
        let mut guard = self.current.lock().unwrap();
        *guard = Arc::new(new_segments);
    }
}

impl Default for IndexState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_state_new_is_empty() {
        let state = IndexState::new();
        let snap = state.snapshot();
        assert!(snap.is_empty());
    }
}
```

### Step 3: Register in lib.rs

Add to `ferret-indexer-core/src/lib.rs` module declarations (alphabetically between `hybrid_detector` and `index_reader`):

```rust
pub mod index_state;
```

Add to re-exports:

```rust
pub use index_state::{IndexState, SegmentList};
```

### Step 4: Run test to verify

Run: `cargo test -p ferret-indexer-core -- test_index_state_new_is_empty -v`

Expected: PASS

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit (only if you created the file)

```bash
git add ferret-indexer-core/src/index_state.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(index_state): add IndexState with snapshot isolation for segment list"
```

---

## Task 2: Add `tombstone_ratio()` and `needs_tombstone()`/`needs_new_entry()` helpers to tombstone module

**Files:**
- Modify: `ferret-indexer-core/src/tombstone.rs`
- Modify: `ferret-indexer-core/src/lib.rs` (re-exports)

The current `TombstoneSet` does not have a `tombstone_ratio()` method or the change-event helper functions. The segment manager needs both.

### Step 1: Write the failing tests

Add to the `tests` module in `ferret-indexer-core/src/tombstone.rs`:

```rust
    #[test]
    fn test_tombstone_ratio_empty() {
        let ts = TombstoneSet::new();
        assert_eq!(ts.tombstone_ratio(100), 0.0);
        assert_eq!(ts.tombstone_ratio(0), 0.0);
    }

    #[test]
    fn test_tombstone_ratio_partial() {
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(0));
        ts.insert(FileId(1));
        let ratio = ts.tombstone_ratio(10);
        assert!((ratio - 0.2).abs() < f32::EPSILON);
    }

    #[test]
    fn test_tombstone_ratio_full() {
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(0));
        ts.insert(FileId(1));
        let ratio = ts.tombstone_ratio(2);
        assert!((ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_needs_tombstone() {
        use crate::tombstone::{needs_new_entry, needs_tombstone};
        use crate::changes::ChangeKind;

        assert!(!needs_tombstone(&ChangeKind::Created));
        assert!(needs_tombstone(&ChangeKind::Modified));
        assert!(needs_tombstone(&ChangeKind::Deleted));
        assert!(needs_tombstone(&ChangeKind::Renamed));
    }

    #[test]
    fn test_needs_new_entry() {
        use crate::tombstone::{needs_new_entry, needs_tombstone};
        use crate::changes::ChangeKind;

        assert!(needs_new_entry(&ChangeKind::Created));
        assert!(needs_new_entry(&ChangeKind::Modified));
        assert!(!needs_new_entry(&ChangeKind::Deleted));
        assert!(needs_new_entry(&ChangeKind::Renamed));
    }
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_tombstone_ratio -v`

Expected: FAIL -- `tombstone_ratio` method does not exist.

### Step 3: Implement tombstone_ratio and helper functions

Add to the `impl TombstoneSet` block in `ferret-indexer-core/src/tombstone.rs`:

```rust
    /// Compute the ratio of tombstoned files to total files in the segment.
    ///
    /// Returns `0.0` if `total_files` is zero.
    pub fn tombstone_ratio(&self, total_files: u32) -> f32 {
        if total_files == 0 {
            return 0.0;
        }
        self.count as f32 / total_files as f32
    }
```

Add these free functions **outside** the `impl TombstoneSet` block but before the `#[cfg(test)]` module. Also add the import for `ChangeKind` at the top of the file:

```rust
use crate::changes::ChangeKind;
```

```rust
/// Determine whether a change kind requires tombstoning the old file entry.
///
/// - `Created` -- no tombstone needed (new file, no old entry exists)
/// - `Modified` -- tombstone old entry (caller adds updated entry to new segment)
/// - `Deleted` -- tombstone old entry (no new entry needed)
/// - `Renamed` -- tombstone old entry (caller adds new metadata entry with new path)
pub fn needs_tombstone(kind: &ChangeKind) -> bool {
    match kind {
        ChangeKind::Created => false,
        ChangeKind::Modified | ChangeKind::Deleted | ChangeKind::Renamed => true,
    }
}

/// Determine whether a change kind requires adding a new entry to a new segment.
///
/// - `Created` -- yes, add new entry
/// - `Modified` -- yes, add updated entry
/// - `Deleted` -- no, file is gone
/// - `Renamed` -- yes, add entry with new path
pub fn needs_new_entry(kind: &ChangeKind) -> bool {
    match kind {
        ChangeKind::Created | ChangeKind::Modified | ChangeKind::Renamed => true,
        ChangeKind::Deleted => false,
    }
}
```

### Step 4: Update lib.rs re-exports

Update the tombstone re-export line in `ferret-indexer-core/src/lib.rs`:

```rust
pub use tombstone::{TombstoneSet, needs_new_entry, needs_tombstone};
```

### Step 5: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- tombstone -v`

Expected: All tombstone tests PASS.

### Step 6: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 7: Commit

```bash
git add ferret-indexer-core/src/tombstone.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(tombstone): add tombstone_ratio(), needs_tombstone(), needs_new_entry()"
```

---

## Task 3: Add `MetadataReader::iter_all()` for reading all entries from a segment

**Files:**
- Modify: `ferret-indexer-core/src/metadata.rs`

Compaction needs to iterate over every entry in a segment's metadata. The current `MetadataReader` only supports lookup by `FileId`. We add `iter_all()` to return all entries.

### Step 1: Write the failing test

Add to the `tests` module in `ferret-indexer-core/src/metadata.rs`:

```rust
    #[test]
    fn test_reader_iter_all() {
        let mut builder = MetadataBuilder::new();
        builder.add_file(make_entry(0, "a.rs", Language::Rust));
        builder.add_file(make_entry(1, "b.py", Language::Python));
        builder.add_file(make_entry(2, "c.go", Language::Go));

        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();
        let all: Vec<FileMetadata> = reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap();

        assert_eq!(all.len(), 3);
        assert_eq!(all[0].path, "a.rs");
        assert_eq!(all[1].path, "b.py");
        assert_eq!(all[2].path, "c.go");
    }

    #[test]
    fn test_reader_iter_all_empty() {
        let builder = MetadataBuilder::new();
        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();
        let all: Vec<FileMetadata> = reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap();
        assert!(all.is_empty());
    }
```

### Step 2: Run test to verify it fails

Run: `cargo test -p ferret-indexer-core -- test_reader_iter_all -v`

Expected: FAIL -- `iter_all` method does not exist.

### Step 3: Implement `iter_all()`

Add to the `impl<'a> MetadataReader<'a>` block in `ferret-indexer-core/src/metadata.rs`:

```rust
    /// Iterate over all file metadata entries in order of index position.
    ///
    /// Returns an iterator yielding `Result<FileMetadata, IndexError>` for
    /// each entry. Errors are returned if an entry's path data is out of
    /// bounds or contains invalid UTF-8.
    pub fn iter_all(&self) -> impl Iterator<Item = Result<FileMetadata, IndexError>> + '_ {
        (0..self.entry_count).map(move |i| self.read_entry(i))
    }
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_reader_iter_all -v`

Expected: PASS

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/metadata.rs
git commit -m "feat(metadata): add MetadataReader::iter_all() for iterating all entries"
```

---

## Task 4: Add `Segment::metadata_reader()` and `Segment::load_tombstones()` accessors

**Files:**
- Modify: `ferret-indexer-core/src/segment.rs`

The segment manager needs to iterate all metadata entries in a segment (for compaction) and load tombstones (for change application and compaction filtering). Add two new methods to `Segment`.

### Step 1: Write the failing tests

Add to the `tests` module in `ferret-indexer-core/src/segment.rs`:

```rust
    #[test]
    fn test_segment_metadata_reader() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![
            InputFile {
                path: "a.rs".to_string(),
                content: b"fn a() {}".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "b.rs".to_string(),
                content: b"fn b() {}".to_vec(),
                mtime: 0,
            },
        ];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        let reader = segment.metadata_reader().unwrap();
        let all: Vec<_> = reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].path, "a.rs");
        assert_eq!(all[1].path, "b.rs");
    }

    #[test]
    fn test_segment_load_tombstones_empty() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() {}".to_vec(),
            mtime: 0,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        let ts = segment.load_tombstones().unwrap();
        assert!(ts.is_empty());
    }

    #[test]
    fn test_segment_load_tombstones_after_write() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![
            InputFile {
                path: "a.rs".to_string(),
                content: b"fn a() {}".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "b.rs".to_string(),
                content: b"fn b() {}".to_vec(),
                mtime: 0,
            },
        ];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        // Write a tombstone manually
        let mut ts = crate::tombstone::TombstoneSet::new();
        ts.insert(FileId(0));
        ts.write_to(&segment.dir_path().join("tombstones.bin")).unwrap();

        // Load it back via the segment
        let loaded = segment.load_tombstones().unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(FileId(0)));
        assert!(!loaded.contains(FileId(1)));
    }
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_segment_metadata_reader -v`

Expected: FAIL -- `metadata_reader` does not exist.

### Step 3: Implement the methods

Add these imports at the top of `ferret-indexer-core/src/segment.rs` (if not already present):

```rust
use crate::tombstone::TombstoneSet;
```

Add to the `impl Segment` block:

```rust
    /// Create a `MetadataReader` for this segment's metadata.
    ///
    /// Useful for iterating all entries (e.g. during compaction).
    pub fn metadata_reader(&self) -> Result<MetadataReader<'_>, IndexError> {
        MetadataReader::new(&self.meta_mmap, &self.paths_mmap)
    }

    /// Load the tombstone set for this segment from disk.
    ///
    /// Reads `tombstones.bin` from the segment directory. Returns an empty
    /// `TombstoneSet` if the file is empty (no tombstones yet).
    pub fn load_tombstones(&self) -> Result<TombstoneSet, IndexError> {
        let path = self.dir_path.join("tombstones.bin");
        let data = std::fs::read(&path)?;
        if data.is_empty() {
            return Ok(TombstoneSet::new());
        }
        TombstoneSet::read_from(&path)
    }
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_segment_metadata_reader test_segment_load_tombstones -v`

Expected: All PASS.

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/segment.rs
git commit -m "feat(segment): add metadata_reader() and load_tombstones() accessors"
```

---

## Task 5: Create `SegmentManager` struct skeleton with `new()` and `next_segment_id()`

**Files:**
- Create: `ferret-indexer-core/src/segment_manager.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Write the failing tests

Create `ferret-indexer-core/src/segment_manager.rs`:

```rust
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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::changes::{ChangeEvent, ChangeKind};
use crate::error::IndexError;
use crate::index_state::{IndexState, SegmentList};
use crate::metadata::FileMetadata;
use crate::segment::{InputFile, Segment, SegmentWriter};
use crate::tombstone::{self, TombstoneSet};
use crate::types::{FileId, SegmentId};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");

        let manager = SegmentManager::new(&base_dir).unwrap();
        let snap = manager.snapshot();
        assert!(snap.is_empty());
    }

    #[test]
    fn test_next_segment_id_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");

        let manager = SegmentManager::new(&base_dir).unwrap();
        let id0 = manager.next_segment_id();
        let id1 = manager.next_segment_id();
        let id2 = manager.next_segment_id();

        assert_eq!(id0, SegmentId(0));
        assert_eq!(id1, SegmentId(1));
        assert_eq!(id2, SegmentId(2));
    }
}
```

### Step 2: Register the module in lib.rs

Add to `ferret-indexer-core/src/lib.rs` module declarations (alphabetically, after `segment`):

```rust
pub mod segment_manager;
```

Add to re-exports:

```rust
pub use segment_manager::SegmentManager;
```

### Step 3: Run test to verify it fails

Run: `cargo test -p ferret-indexer-core -- test_new_empty_dir -v`

Expected: FAIL -- `SegmentManager` struct does not exist.

### Step 4: Implement the struct skeleton

Add to `ferret-indexer-core/src/segment_manager.rs`, above the `#[cfg(test)]` module:

```rust
/// Default maximum number of segments before compaction is recommended.
const DEFAULT_MAX_SEGMENTS: usize = 10;

/// Default tombstone ratio threshold for compaction.
const DEFAULT_MAX_TOMBSTONE_RATIO: f32 = 0.30;

/// Segment lifecycle manager.
///
/// The primary entry point for all indexing operations. Owns the `IndexState`
/// (for snapshot isolation), a writer mutex (serializes indexing/compaction),
/// and a monotonic segment ID counter.
pub struct SegmentManager {
    /// Base directory for the index (e.g. `.ferret_index/`). Segments live
    /// under `<base_dir>/segments/`.
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
    /// * `base_dir` - The index root directory (e.g. `.ferret_index/`).
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
            if let Some(id_str) = name.strip_prefix("seg_") {
                if let Ok(id) = id_str.parse::<u32>() {
                    let segment = Segment::open(&entry.path(), SegmentId(id))?;
                    if id >= max_id {
                        max_id = id + 1;
                    }
                    segments.push(Arc::new(segment));
                }
            }
        }

        if !segments.is_empty() {
            state.publish(segments);
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
    /// Thread-safe via `AtomicU32::fetch_add`.
    pub fn next_segment_id(&self) -> SegmentId {
        SegmentId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Take a lock-free snapshot of the current segment list.
    ///
    /// Delegates to `IndexState::snapshot()`. The returned `SegmentList` is
    /// a frozen view that remains valid regardless of concurrent writes.
    pub fn snapshot(&self) -> SegmentList {
        self.state.snapshot()
    }
}
```

### Step 5: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- segment_manager -v`

Expected: All PASS.

### Step 6: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 7: Commit

```bash
git add ferret-indexer-core/src/segment_manager.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(segment_manager): add SegmentManager skeleton with new() and next_segment_id()"
```

---

## Task 6: Implement `add_segment()` and `index_files()`

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

### Step 1: Write the failing tests

Add to the `tests` module in `ferret-indexer-core/src/segment_manager.rs`:

```rust
    #[test]
    fn test_add_segment() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");
        let segments_dir = base_dir.join("segments");
        fs::create_dir_all(&segments_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Build a segment externally and add it
        let seg_id = manager.next_segment_id();
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
        let base_dir = dir.path().join(".ferret_index");

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
        let base_dir = dir.path().join(".ferret_index");

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
        let base_dir = dir.path().join(".ferret_index");

        let manager = SegmentManager::new(&base_dir).unwrap();
        manager.index_files(vec![]).unwrap();

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1); // empty segment is still added
        assert_eq!(snap[0].entry_count(), 0);
    }
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_add_segment -v`

Expected: FAIL -- `add_segment` method does not exist.

### Step 3: Implement `add_segment()` and `index_files()`

Add to the `impl SegmentManager` block:

```rust
    /// Add a pre-built segment to the active segment list.
    ///
    /// Acquires the writer lock, appends the segment to the current list,
    /// and publishes the new list atomically.
    pub fn add_segment(&self, segment: Arc<Segment>) {
        let _guard = self.write_lock.lock().unwrap();
        let mut segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();
        segments.push(segment);
        self.state.publish(segments);
    }

    /// Build a new segment from input files and add it to the index.
    ///
    /// Allocates a new segment ID, builds the segment via `SegmentWriter`,
    /// and atomically publishes it. The writer lock is held for the duration
    /// of the build.
    ///
    /// # Errors
    ///
    /// Returns `IndexError` if the segment build fails (I/O error, etc.).
    pub fn index_files(&self, files: Vec<InputFile>) -> Result<(), IndexError> {
        let _guard = self.write_lock.lock().unwrap();
        let seg_id = self.next_segment_id();
        let writer = SegmentWriter::new(&self.segments_dir, seg_id);
        let segment = writer.build(files)?;
        let segment = Arc::new(segment);

        let mut segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();
        segments.push(segment);
        self.state.publish(segments);

        Ok(())
    }
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- segment_manager -v`

Expected: All PASS.

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "feat(segment_manager): implement add_segment() and index_files()"
```

---

## Task 7: Implement `apply_changes()`

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

This is the core incremental update method. For each change event, it tombstones old entries in existing segments and builds a new segment with created/modified/renamed files.

### Step 1: Write the failing tests

Add to the `tests` module in `ferret-indexer-core/src/segment_manager.rs`:

```rust
    use std::path::PathBuf;

    #[test]
    fn test_apply_changes_create() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");
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
        let base_dir = dir.path().join(".ferret_index");
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
        let base_dir = dir.path().join(".ferret_index");
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
        let base_dir = dir.path().join(".ferret_index");
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
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_apply_changes -v`

Expected: FAIL -- `apply_changes` method does not exist.

### Step 3: Implement `apply_changes()`

Add a private helper method and the public `apply_changes()` to the `impl SegmentManager` block:

```rust
    /// Find all (segment_index, file_id) pairs for a given relative path
    /// across the current segments.
    ///
    /// Searches segments in order, checking metadata for path matches.
    /// This is used by `apply_changes()` to locate entries that need tombstoning.
    fn find_file_in_segments(
        segments: &[Arc<Segment>],
        path: &str,
    ) -> Vec<(usize, FileId)> {
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

        let _guard = self.write_lock.lock().unwrap();
        let current_segments: Vec<Arc<Segment>> =
            self.state.snapshot().as_ref().clone();

        // Track tombstones to write per segment index
        let mut tombstone_updates: std::collections::HashMap<usize, TombstoneSet> =
            std::collections::HashMap::new();

        // Collect files that need new entries
        let mut new_files: Vec<InputFile> = Vec::new();

        for change in changes {
            let path_str = change.path.to_string_lossy().to_string();

            // Tombstone old entries if needed
            if tombstone::needs_tombstone(&change.kind) {
                let locations =
                    Self::find_file_in_segments(&current_segments, &path_str);
                for (seg_idx, file_id) in locations {
                    tombstone_updates
                        .entry(seg_idx)
                        .or_insert_with(TombstoneSet::new)
                        .insert(file_id);
                }
            }

            // Read new content if needed
            if tombstone::needs_new_entry(&change.kind) {
                let full_path = repo_dir.join(&change.path);
                if full_path.exists() {
                    let content = fs::read(&full_path)?;
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

        // Write tombstones to affected segments
        for (seg_idx, new_tombstones) in &tombstone_updates {
            let segment = &current_segments[*seg_idx];
            let mut existing = segment.load_tombstones()?;
            existing.merge(new_tombstones);
            existing.write_to(&segment.dir_path().join("tombstones.bin"))?;
        }

        // Build new segment if there are files to add
        let mut updated_segments = current_segments;
        if !new_files.is_empty() {
            let seg_id = self.next_segment_id();
            let writer = SegmentWriter::new(&self.segments_dir, seg_id);
            let segment = writer.build(new_files)?;
            updated_segments.push(Arc::new(segment));
        }

        self.state.publish(updated_segments);
        Ok(())
    }
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- segment_manager -v`

Expected: All PASS.

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "feat(segment_manager): implement apply_changes() with tombstoning"
```

---

## Task 8: Implement `should_compact()` and `compact()`

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

### Step 1: Write the failing tests

Add to the `tests` module in `ferret-indexer-core/src/segment_manager.rs`:

```rust
    #[test]
    fn test_should_compact_empty() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");
        let manager = SegmentManager::new(&base_dir).unwrap();
        assert!(!manager.should_compact());
    }

    #[test]
    fn test_should_compact_too_many_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");
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
        let base_dir = dir.path().join(".ferret_index");
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
        let base_dir = dir.path().join(".ferret_index");
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
        let all: Vec<_> = reader
            .iter_all()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let paths: Vec<&str> = all.iter().map(|m| m.path.as_str()).collect();
        assert!(paths.contains(&"file_0.rs"));
        assert!(paths.contains(&"file_1.rs"));
        assert!(paths.contains(&"file_2.rs"));
    }

    #[test]
    fn test_compact_excludes_tombstoned() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");
        let repo_dir = dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        let manager = SegmentManager::new(&base_dir).unwrap();

        // Index 3 files
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
        let base_dir = dir.path().join(".ferret_index");
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
        let base_dir = dir.path().join(".ferret_index");
        let manager = SegmentManager::new(&base_dir).unwrap();

        // Compacting an empty index should be a no-op
        manager.compact().unwrap();

        let snap = manager.snapshot();
        assert!(snap.is_empty());
    }

    #[test]
    fn test_compact_single_segment_no_tombstones() {
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

        // Compacting a single segment with no tombstones is a no-op
        manager.compact().unwrap();

        let snap = manager.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].entry_count(), 1);
    }
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_should_compact -v`

Expected: FAIL -- `should_compact` does not exist.

### Step 3: Implement `should_compact()` and `compact()`

Add to the `impl SegmentManager` block:

```rust
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
        let _guard = self.write_lock.lock().unwrap();
        let current_segments: Vec<Arc<Segment>> =
            self.state.snapshot().as_ref().clone();

        // No-op if empty
        if current_segments.is_empty() {
            return Ok(());
        }

        // No-op if single segment with no tombstones
        if current_segments.len() == 1 {
            let ts = current_segments[0].load_tombstones()?;
            if ts.is_empty() {
                return Ok(());
            }
        }

        // Collect all non-tombstoned entries from all segments
        let mut merged_files: Vec<InputFile> = Vec::new();

        for segment in &current_segments {
            let tombstones = segment.load_tombstones()?;
            let reader = segment.metadata_reader()?;

            for entry_result in reader.iter_all() {
                let entry: FileMetadata = entry_result?;

                // Skip tombstoned entries
                if tombstones.contains(entry.file_id) {
                    continue;
                }

                // Read the original content from the content store
                let content = segment
                    .content_reader()
                    .read_content(entry.content_offset, entry.content_len)?;

                merged_files.push(InputFile {
                    path: entry.path,
                    content,
                    mtime: entry.mtime_epoch_secs,
                });
            }
        }

        // Build the merged segment
        let seg_id = self.next_segment_id();
        let writer = SegmentWriter::new(&self.segments_dir, seg_id);
        let merged_segment = writer.build(merged_files)?;

        // Collect old directory paths before we publish (so we know what to delete)
        let old_dirs: Vec<PathBuf> = current_segments
            .iter()
            .map(|s| s.dir_path().to_path_buf())
            .collect();

        // Publish the new segment list (just the merged segment)
        self.state.publish(vec![Arc::new(merged_segment)]);

        // Delete old segment directories (best-effort; errors are logged but not fatal)
        for old_dir in old_dirs {
            let _ = fs::remove_dir_all(&old_dir);
        }

        Ok(())
    }
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- segment_manager -v`

Expected: All PASS.

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "feat(segment_manager): implement should_compact() and compact()"
```

---

## Task 9: Implement `compact_background()` for async compaction

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

### Step 1: Write the test

Add to the `tests` module:

```rust
    #[tokio::test]
    async fn test_compact_background() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");

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
```

### Step 2: Run test to verify it fails

Run: `cargo test -p ferret-indexer-core -- test_compact_background -v`

Expected: FAIL -- `compact_background` does not exist.

### Step 3: Implement `compact_background()`

Add to the `impl SegmentManager` block:

```rust
    /// Run compaction in the background via `tokio::spawn`.
    ///
    /// Returns a `JoinHandle` that resolves to the compaction result.
    /// The caller can `await` or ignore it.
    ///
    /// # Panics
    ///
    /// The `SegmentManager` must be wrapped in an `Arc` for this method
    /// to work, since the spawned task needs a `'static` reference.
    pub fn compact_background(
        self: &Arc<Self>,
    ) -> tokio::task::JoinHandle<Result<(), IndexError>> {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.compact() })
    }
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_compact_background -v`

Expected: PASS

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "feat(segment_manager): add compact_background() for async compaction"
```

---

## Task 10: Implement `reopen()` for loading existing segments from disk

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs`

This test verifies that creating a new `SegmentManager` at a directory that already has segments correctly loads them.

### Step 1: Write the test

Add to the `tests` module:

```rust
    #[test]
    fn test_reopen_existing_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");

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
        let next = manager2.next_segment_id();
        assert_eq!(next, SegmentId(2));
    }

    #[test]
    fn test_reopen_after_compact() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index");

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
```

### Step 2: Run tests

Run: `cargo test -p ferret-indexer-core -- test_reopen -v`

Expected: PASS -- the `new()` method from Task 5 already scans existing segments.

### Step 3: Commit

```bash
git add ferret-indexer-core/src/segment_manager.rs
git commit -m "test(segment_manager): add reopen tests for persistence verification"
```

---

## Task 11: Final verification and cleanup

### Step 1: Run full test suite

Run: `cargo test --workspace`

Expected: All tests pass.

### Step 2: Run lints and formatting

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: No warnings, formatting OK.

### Step 3: Auto-format if needed

Run: `cargo fmt --all`

### Step 4: Verify the module structure

At this point, `ferret-indexer-core/src/segment_manager.rs` should contain:

- `SegmentManager` struct with fields: `base_dir`, `segments_dir`, `next_id` (AtomicU32), `state` (IndexState), `write_lock` (Mutex)
- `SegmentManager::new(base_dir)` -- creates manager, scans existing segments
- `SegmentManager::next_segment_id()` -- monotonically increasing via AtomicU32
- `SegmentManager::snapshot()` -- delegates to IndexState
- `SegmentManager::add_segment(segment)` -- adds pre-built segment to list
- `SegmentManager::index_files(files)` -- builds new segment and publishes
- `SegmentManager::apply_changes(repo_dir, changes)` -- tombstones old, builds new
- `SegmentManager::should_compact()` -- checks segment count > 10 or tombstone ratio > 30%
- `SegmentManager::compact()` -- merges all segments, removes old dirs
- `SegmentManager::compact_background()` -- wraps compact() in tokio::spawn

Supporting changes:

- `index_state.rs` -- `IndexState` with `snapshot()` and `publish()` (created if missing)
- `tombstone.rs` -- added `tombstone_ratio()`, `needs_tombstone()`, `needs_new_entry()`
- `metadata.rs` -- added `MetadataReader::iter_all()`
- `segment.rs` -- added `Segment::metadata_reader()` and `Segment::load_tombstones()`

### Step 5: Final commit if any cleanup was needed

```bash
git add -A
git commit -m "chore(segment_manager): final cleanup for segment manager implementation"
```

---

## Reference: Existing APIs Used

These are the exact module APIs that `SegmentManager` calls. The implementer should NOT modify any of these:

| Module | API Used |
|--------|----------|
| `segment.rs` | `SegmentWriter::new(base_dir, seg_id)`, `.build(files)` -> `Segment` |
| `segment.rs` | `Segment::open(dir, seg_id)`, `.segment_id()`, `.entry_count()`, `.dir_path()`, `.get_metadata(fid)`, `.content_reader()`, `.trigram_reader()` |
| `segment.rs` | `Segment::metadata_reader()` -> `MetadataReader` (added by Task 4) |
| `segment.rs` | `Segment::load_tombstones()` -> `TombstoneSet` (added by Task 4) |
| `tombstone.rs` | `TombstoneSet::new()`, `.insert(fid)`, `.contains(fid)`, `.len()`, `.is_empty()`, `.merge(&other)`, `.write_to(path)`, `.read_from(path)` |
| `tombstone.rs` | `TombstoneSet::tombstone_ratio(total)` (added by Task 2) |
| `tombstone.rs` | `needs_tombstone(&kind)`, `needs_new_entry(&kind)` (added by Task 2) |
| `index_state.rs` | `IndexState::new()`, `.snapshot()`, `.publish(segments)` |
| `metadata.rs` | `MetadataReader::iter_all()` (added by Task 3) |
| `content.rs` | `ContentStoreReader::read_content(offset, len)` |
| `types.rs` | `FileId(u32)`, `SegmentId(u32)` |
| `changes.rs` | `ChangeEvent { path, kind }`, `ChangeKind::{Created, Modified, Deleted, Renamed}` |
| `error.rs` | `IndexError` |

## Reference: On-Disk Layout

```
.ferret_index/
  segments/
    seg_0000/
      trigrams.bin
      meta.bin
      paths.bin
      content.zst
      tombstones.bin    <- updated by apply_changes()
    seg_0001/
      ...
```

After compaction:

```
.ferret_index/
  segments/
    seg_0003/           <- merged segment (old seg_0000..0002 deleted)
      trigrams.bin
      meta.bin
      paths.bin
      content.zst
      tombstones.bin    <- empty (no tombstones in freshly compacted segment)
```
