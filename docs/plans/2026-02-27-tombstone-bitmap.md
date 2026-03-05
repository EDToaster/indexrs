# Tombstone Bitmap Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a per-segment tombstone bitmap (`TombstoneSet`) for marking deleted and updated files, with binary persistence to `tombstones.bin`, enabling incremental index updates without full segment rewrites.

**Architecture:** Single new module `tombstone.rs` in `ferret-indexer-core` containing `TombstoneSet` -- a set of `FileId`s marked as deleted within a segment. Uses `HashSet<FileId>` internally (simple, no new deps). Persists to `tombstones.bin` using the project's standard binary format pattern: 10-byte header (`magic + version + count`) followed by sorted `u32` file IDs, all little-endian. Writers use atomic temp-file-then-rename for crash safety. Includes free functions mapping `ChangeEvent` kinds to tombstone operations.

**Tech Stack:** Rust 2024, `std::collections::HashSet`, `std::io::Write`, `tempfile` (dev), little-endian binary format

---

## Task 1: Create tombstone module with TombstoneSet struct and basic tests

**File:** `ferret-indexer-core/src/tombstone.rs` (NEW)

Create the module with the `TombstoneSet` struct and its core in-memory API. Write tests first, then implement.

```rust
//! Tombstone bitmap for marking deleted/updated files within a segment.
//!
//! When a file is modified, deleted, or renamed, its old `FileId` is
//! tombstoned in the segment that originally indexed it. Search skips
//! tombstoned entries. When the tombstone ratio gets high enough, the
//! segment is a candidate for compaction.
//!
//! ## Binary Format (`tombstones.bin`)
//!
//! ```text
//! [Header]  (10 bytes)
//!   magic: u32 = 0x544F4D42  ("TOMB")
//!   version: u16 = 1
//!   count: u32              (number of tombstoned file_ids)
//!
//! [Tombstoned IDs]  (4 bytes each, sorted ascending)
//!   file_id: u32            (little-endian)
//! ```

use std::collections::HashSet;

use crate::types::FileId;

/// Magic number for tombstones.bin: "TOMB" in ASCII as little-endian u32.
const TOMB_MAGIC: u32 = 0x544F_4D42;

/// Current format version for tombstones.bin.
const TOMB_VERSION: u16 = 1;

/// Size of the tombstones.bin header in bytes: magic(4) + version(2) + count(4).
const HEADER_SIZE: usize = 10;

/// A set of tombstoned (deleted/superseded) file IDs within a single segment.
///
/// When files are modified, deleted, or renamed, their old `FileId` entries
/// are added to the tombstone set. During search, tombstoned file IDs are
/// skipped. When [`tombstone_ratio`](TombstoneSet::tombstone_ratio) exceeds
/// a threshold, the segment should be compacted.
#[derive(Debug, Clone)]
pub struct TombstoneSet {
    tombstoned: HashSet<FileId>,
}
```

Tests to write in the same file under `#[cfg(test)] mod tests`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_empty() {
        let ts = TombstoneSet::new();
        assert!(ts.is_empty());
        assert_eq!(ts.len(), 0);
    }

    #[test]
    fn test_insert_and_contains() {
        let mut ts = TombstoneSet::new();
        assert!(!ts.contains(FileId(0)));

        ts.insert(FileId(0));
        assert!(ts.contains(FileId(0)));
        assert!(!ts.contains(FileId(1)));
        assert_eq!(ts.len(), 1);
        assert!(!ts.is_empty());
    }

    #[test]
    fn test_insert_idempotent() {
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(42));
        ts.insert(FileId(42));
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn test_multiple_inserts() {
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(0));
        ts.insert(FileId(5));
        ts.insert(FileId(10));
        assert_eq!(ts.len(), 3);
        assert!(ts.contains(FileId(0)));
        assert!(ts.contains(FileId(5)));
        assert!(ts.contains(FileId(10)));
        assert!(!ts.contains(FileId(1)));
    }

    #[test]
    fn test_tombstone_ratio() {
        let mut ts = TombstoneSet::new();
        // 0 of 100 = 0.0
        assert_eq!(ts.tombstone_ratio(100), 0.0);
        // 0 of 0 = 0.0 (edge case, no division by zero)
        assert_eq!(ts.tombstone_ratio(0), 0.0);

        ts.insert(FileId(0));
        ts.insert(FileId(1));
        // 2 of 10 = 0.2
        let ratio = ts.tombstone_ratio(10);
        assert!((ratio - 0.2).abs() < f32::EPSILON);
        // 2 of 2 = 1.0
        let ratio = ts.tombstone_ratio(2);
        assert!((ratio - 1.0).abs() < f32::EPSILON);
    }
}
```

Implement the methods:

```rust
impl TombstoneSet {
    /// Create a new empty tombstone set.
    pub fn new() -> Self {
        TombstoneSet {
            tombstoned: HashSet::new(),
        }
    }

    /// Check whether a file ID has been tombstoned.
    pub fn contains(&self, file_id: FileId) -> bool {
        self.tombstoned.contains(&file_id)
    }

    /// Mark a file ID as tombstoned (deleted or superseded).
    pub fn insert(&mut self, file_id: FileId) {
        self.tombstoned.insert(file_id);
    }

    /// Return the number of tombstoned file IDs.
    pub fn len(&self) -> usize {
        self.tombstoned.len()
    }

    /// Check whether the tombstone set is empty.
    pub fn is_empty(&self) -> bool {
        self.tombstoned.is_empty()
    }

    /// Compute the ratio of tombstoned files to total files in the segment.
    ///
    /// Returns `0.0` if `total_files` is zero.
    pub fn tombstone_ratio(&self, total_files: u32) -> f32 {
        if total_files == 0 {
            return 0.0;
        }
        self.tombstoned.len() as f32 / total_files as f32
    }
}

impl Default for TombstoneSet {
    fn default() -> Self {
        Self::new()
    }
}
```

**Test:** `cargo test -p ferret-indexer-core -- tombstone::tests` -- all pass.

---

## Task 2: Wire up the module in lib.rs

**File:** `ferret-indexer-core/src/lib.rs` (UPDATE)

Add the module declaration and re-export:

```rust
pub mod tombstone;
```

Add to the re-exports section at the bottom:

```rust
pub use tombstone::TombstoneSet;
```

Place `pub mod tombstone;` alphabetically among the existing module declarations (after `pub mod trigram;`, before `pub mod types;`). Place `pub use tombstone::TombstoneSet;` alphabetically among the re-exports (after the `trigram` re-exports, before the `types` re-exports).

**Test:** `cargo check --workspace` -- no errors. `cargo test -p ferret-indexer-core -- tombstone` -- all pass.

---

## Task 3: Implement binary serialization (write_to)

**File:** `ferret-indexer-core/src/tombstone.rs` (UPDATE)

Add `write_to` method that writes the tombstone set to a `tombstones.bin` file using atomic rename. Add the necessary imports at the top of the file.

Add these imports (add to existing import block):

```rust
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::IndexError;
```

Add the method to the `impl TombstoneSet` block:

```rust
    /// Write the tombstone set to a binary file at the given path.
    ///
    /// Format: 10-byte header + sorted list of tombstoned file_id u32s (LE).
    /// Uses atomic temp-file-then-rename for crash safety.
    pub fn write_to(&self, path: &Path) -> Result<(), IndexError> {
        let mut ids: Vec<u32> = self.tombstoned.iter().map(|fid| fid.0).collect();
        ids.sort();

        let count: u32 = ids.len().try_into().map_err(|_| {
            IndexError::IndexCorruption(format!(
                "tombstone count {} exceeds u32::MAX",
                ids.len()
            ))
        })?;

        let total_size = HEADER_SIZE + ids.len() * 4;
        let mut buf = Vec::with_capacity(total_size);

        // Header
        buf.write_all(&TOMB_MAGIC.to_le_bytes())?;
        buf.write_all(&TOMB_VERSION.to_le_bytes())?;
        buf.write_all(&count.to_le_bytes())?;

        // Sorted file IDs
        for id in &ids {
            buf.write_all(&id.to_le_bytes())?;
        }

        // Atomic write: temp file then rename
        let parent = path.parent().unwrap_or(Path::new("."));
        let temp_path = parent.join(format!(
            ".tombstones.bin.tmp.{}",
            std::process::id()
        ));
        fs::write(&temp_path, &buf)?;
        fs::rename(&temp_path, path)?;

        Ok(())
    }
```

Write these tests:

```rust
    #[test]
    fn test_write_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstones.bin");

        let ts = TombstoneSet::new();
        ts.write_to(&path).unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), HEADER_SIZE); // header only, no IDs

        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        assert_eq!(magic, TOMB_MAGIC);
        let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
        assert_eq!(version, TOMB_VERSION);
        let count = u32::from_le_bytes(data[6..10].try_into().unwrap());
        assert_eq!(count, 0);
    }

    #[test]
    fn test_write_with_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstones.bin");

        let mut ts = TombstoneSet::new();
        ts.insert(FileId(10));
        ts.insert(FileId(3));
        ts.insert(FileId(7));
        ts.write_to(&path).unwrap();

        let data = std::fs::read(&path).unwrap();
        // 10 header + 3 * 4 bytes = 22 bytes
        assert_eq!(data.len(), 22);

        let count = u32::from_le_bytes(data[6..10].try_into().unwrap());
        assert_eq!(count, 3);

        // IDs must be sorted: 3, 7, 10
        let id0 = u32::from_le_bytes(data[10..14].try_into().unwrap());
        let id1 = u32::from_le_bytes(data[14..18].try_into().unwrap());
        let id2 = u32::from_le_bytes(data[18..22].try_into().unwrap());
        assert_eq!(id0, 3);
        assert_eq!(id1, 7);
        assert_eq!(id2, 10);
    }

    #[test]
    fn test_write_atomic_no_temp_file_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstones.bin");

        let mut ts = TombstoneSet::new();
        ts.insert(FileId(1));
        ts.write_to(&path).unwrap();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "tombstones.bin");
    }
```

**Test:** `cargo test -p ferret-indexer-core -- tombstone` -- all pass.

---

## Task 4: Implement binary deserialization (read_from)

**File:** `ferret-indexer-core/src/tombstone.rs` (UPDATE)

Add `read_from` method that loads a tombstone set from a `tombstones.bin` file, validating magic and version.

```rust
    /// Read a tombstone set from a binary file at the given path.
    ///
    /// Validates the magic number and format version. Returns
    /// [`IndexError::IndexCorruption`] if the header is invalid or the
    /// file is too small, or [`IndexError::UnsupportedVersion`] if the
    /// format version is not recognized.
    pub fn read_from(path: &Path) -> Result<Self, IndexError> {
        let data = fs::read(path)?;

        if data.len() < HEADER_SIZE {
            return Err(IndexError::IndexCorruption(
                "tombstones.bin too small for header".to_string(),
            ));
        }

        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if magic != TOMB_MAGIC {
            return Err(IndexError::IndexCorruption(format!(
                "invalid tombstones.bin magic: expected 0x{TOMB_MAGIC:08X}, got 0x{magic:08X}"
            )));
        }

        let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
        if version != TOMB_VERSION {
            return Err(IndexError::UnsupportedVersion {
                version: version as u32,
            });
        }

        let count = u32::from_le_bytes(data[6..10].try_into().unwrap()) as usize;

        let expected_size = HEADER_SIZE + count * 4;
        if data.len() < expected_size {
            return Err(IndexError::IndexCorruption(format!(
                "tombstones.bin too small: expected at least {expected_size} bytes \
                 for {count} entries, got {}",
                data.len()
            )));
        }

        let mut tombstoned = HashSet::with_capacity(count);
        for i in 0..count {
            let offset = HEADER_SIZE + i * 4;
            let file_id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            tombstoned.insert(FileId(file_id));
        }

        Ok(TombstoneSet { tombstoned })
    }
```

Write these tests:

```rust
    #[test]
    fn test_roundtrip_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstones.bin");

        let ts = TombstoneSet::new();
        ts.write_to(&path).unwrap();

        let loaded = TombstoneSet::read_from(&path).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.len(), 0);
    }

    #[test]
    fn test_roundtrip_with_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstones.bin");

        let mut ts = TombstoneSet::new();
        ts.insert(FileId(0));
        ts.insert(FileId(42));
        ts.insert(FileId(100));
        ts.insert(FileId(u32::MAX - 1));
        ts.write_to(&path).unwrap();

        let loaded = TombstoneSet::read_from(&path).unwrap();
        assert_eq!(loaded.len(), 4);
        assert!(loaded.contains(FileId(0)));
        assert!(loaded.contains(FileId(42)));
        assert!(loaded.contains(FileId(100)));
        assert!(loaded.contains(FileId(u32::MAX - 1)));
        assert!(!loaded.contains(FileId(1)));
    }

    #[test]
    fn test_read_invalid_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstones.bin");

        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        data[4..6].copy_from_slice(&TOMB_VERSION.to_le_bytes());
        data[6..10].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        let result = TombstoneSet::read_from(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("invalid tombstones.bin magic"));
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    #[test]
    fn test_read_invalid_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstones.bin");

        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&TOMB_MAGIC.to_le_bytes());
        data[4..6].copy_from_slice(&99u16.to_le_bytes());
        data[6..10].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        let result = TombstoneSet::read_from(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::UnsupportedVersion { version } => {
                assert_eq!(version, 99);
            }
            other => panic!("expected UnsupportedVersion, got: {other}"),
        }
    }

    #[test]
    fn test_read_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstones.bin");

        // Header claims 2 entries but file only has header
        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&TOMB_MAGIC.to_le_bytes());
        data[4..6].copy_from_slice(&TOMB_VERSION.to_le_bytes());
        data[6..10].copy_from_slice(&2u32.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        let result = TombstoneSet::read_from(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("too small"));
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    #[test]
    fn test_read_file_too_small_for_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstones.bin");

        std::fs::write(&path, &[0u8; 5]).unwrap();

        let result = TombstoneSet::read_from(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("too small for header"));
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    #[test]
    fn test_read_nonexistent_file() {
        let result = TombstoneSet::read_from(Path::new("/nonexistent/tombstones.bin"));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), IndexError::Io(_)));
    }
```

**Test:** `cargo test -p ferret-indexer-core -- tombstone` -- all pass.

---

## Task 5: Add change-handling helper functions

**File:** `ferret-indexer-core/src/tombstone.rs` (UPDATE)

Add free functions that map `ChangeEvent` semantics to tombstone operations. These functions determine *what tombstone action* to take given a change kind, but the caller is responsible for actually looking up file IDs and creating new segment entries.

Add this import:

```rust
use crate::changes::ChangeKind;
```

Add the functions and their doc-comments:

```rust
/// Determine whether a change kind requires tombstoning the old file entry.
///
/// - `Created` -- no tombstone needed (new file, no old entry exists)
/// - `Modified` -- tombstone old entry (caller adds updated entry to new segment)
/// - `Deleted` -- tombstone old entry (no new entry needed)
/// - `Renamed` -- tombstone old entry (caller adds new metadata entry with new path)
///
/// This is a pure helper; the caller must resolve the file's `FileId` from
/// the metadata index and call [`TombstoneSet::insert`] if this returns `true`.
pub fn needs_tombstone(kind: &ChangeKind) -> bool {
    match kind {
        ChangeKind::Created => false,
        ChangeKind::Modified => true,
        ChangeKind::Deleted => true,
        ChangeKind::Renamed => true,
    }
}

/// Determine whether a change kind requires adding a new entry to a new segment.
///
/// - `Created` -- yes, add new entry
/// - `Modified` -- yes, add updated entry (new content, new file_id in new segment)
/// - `Deleted` -- no, file is gone
/// - `Renamed` -- yes, add new metadata entry (content may be unchanged; use
///   content_hash to detect and potentially reuse content store data)
pub fn needs_new_entry(kind: &ChangeKind) -> bool {
    match kind {
        ChangeKind::Created => true,
        ChangeKind::Modified => true,
        ChangeKind::Deleted => false,
        ChangeKind::Renamed => true,
    }
}
```

Write these tests:

```rust
    #[test]
    fn test_needs_tombstone() {
        assert!(!needs_tombstone(&ChangeKind::Created));
        assert!(needs_tombstone(&ChangeKind::Modified));
        assert!(needs_tombstone(&ChangeKind::Deleted));
        assert!(needs_tombstone(&ChangeKind::Renamed));
    }

    #[test]
    fn test_needs_new_entry() {
        assert!(needs_new_entry(&ChangeKind::Created));
        assert!(needs_new_entry(&ChangeKind::Modified));
        assert!(!needs_new_entry(&ChangeKind::Deleted));
        assert!(needs_new_entry(&ChangeKind::Renamed));
    }
```

**Test:** `cargo test -p ferret-indexer-core -- tombstone` -- all pass.

---

## Task 6: Update lib.rs re-exports and run full verification

**File:** `ferret-indexer-core/src/lib.rs` (UPDATE)

Update the re-exports to include the change-handling functions:

```rust
pub use tombstone::{TombstoneSet, needs_new_entry, needs_tombstone};
```

Run the full verification suite:

```bash
cargo fmt --all                          # Auto-format
cargo clippy --workspace -- -D warnings  # Lint check (CI-strict)
cargo test --workspace                   # All tests pass
cargo check --workspace                  # Type-check
```

All four commands must succeed with zero warnings.

---
