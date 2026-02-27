//! Tombstone bitmap for tracking deleted and updated files in a segment.
//!
//! When a file is deleted or updated, rather than rewriting the entire segment,
//! we mark the old file ID as "tombstoned." During search, tombstoned file IDs
//! are skipped when intersecting posting lists, effectively hiding stale results
//! without the cost of a full segment rebuild.
//!
//! [`TombstoneSet`] uses a compact bitmap representation backed by a `Vec<u64>`,
//! where each bit corresponds to a [`FileId`]. It supports persistence to and
//! from a `tombstone.bin` binary file with magic number and version header,
//! using the same atomic temp-file-then-rename pattern as other index writers.
//!
//! ## Binary Format
//!
//! ```text
//! [Header]  (14 bytes)
//!   magic: u32 = 0x544F4D42  ("TOMB")
//!   version: u16 = 1
//!   max_file_id: u32          (highest file_id that can be represented + 1)
//!   tombstone_count: u32      (number of set bits / tombstoned files)
//!
//! [Bitmap]  (ceil(max_file_id / 64) * 8 bytes)
//!   Little-endian u64 words. Bit `i` of word `i/64` is set if `FileId(i)` is tombstoned.
//! ```

use std::fs;
use std::path::Path;

use crate::changes::ChangeKind;
use crate::error::IndexError;
use crate::types::FileId;

/// Magic number for tombstone.bin: "TOMB" in ASCII as little-endian u32.
const TOMB_MAGIC: u32 = 0x544F_4D42;

/// Current format version for tombstone.bin.
const TOMB_VERSION: u16 = 1;

/// Size of the header in bytes: magic(4) + version(2) + max_file_id(4) + tombstone_count(4).
const HEADER_SIZE: usize = 14;

/// A set of tombstoned (deleted/updated) file IDs, stored as a compact bitmap.
///
/// Each bit position corresponds to a `FileId`. A set bit means the file has
/// been tombstoned and should be excluded from search results.
///
/// # Examples
///
/// ```
/// use indexrs_core::tombstone::TombstoneSet;
/// use indexrs_core::types::FileId;
///
/// let mut ts = TombstoneSet::new();
/// assert!(!ts.contains(FileId(5)));
///
/// ts.insert(FileId(5));
/// assert!(ts.contains(FileId(5)));
/// assert_eq!(ts.len(), 1);
///
/// ts.remove(FileId(5));
/// assert!(!ts.contains(FileId(5)));
/// assert_eq!(ts.len(), 0);
/// ```
#[derive(Debug, Clone)]
pub struct TombstoneSet {
    /// Bitmap words. Bit `i` of `words[i / 64]` is set if `FileId(i)` is tombstoned.
    words: Vec<u64>,
    /// Number of tombstoned file IDs (cached for O(1) access).
    count: u32,
}

impl TombstoneSet {
    /// Create a new, empty tombstone set.
    pub fn new() -> Self {
        TombstoneSet {
            words: Vec::new(),
            count: 0,
        }
    }

    /// Create a new tombstone set pre-allocated to hold file IDs up to `capacity - 1`.
    ///
    /// The capacity is in terms of file IDs, not bitmap words.
    pub fn with_capacity(capacity: u32) -> Self {
        let num_words = word_count(capacity);
        TombstoneSet {
            words: vec![0u64; num_words],
            count: 0,
        }
    }

    /// Mark a file ID as tombstoned.
    ///
    /// If the file ID is already tombstoned, this is a no-op.
    pub fn insert(&mut self, file_id: FileId) {
        let (word_idx, bit_idx) = word_and_bit(file_id);

        // Grow the bitmap if needed
        if word_idx >= self.words.len() {
            self.words.resize(word_idx + 1, 0);
        }

        let mask = 1u64 << bit_idx;
        if self.words[word_idx] & mask == 0 {
            self.words[word_idx] |= mask;
            self.count += 1;
        }
    }

    /// Remove a file ID from the tombstone set (un-tombstone it).
    ///
    /// If the file ID is not tombstoned, this is a no-op.
    pub fn remove(&mut self, file_id: FileId) {
        let (word_idx, bit_idx) = word_and_bit(file_id);

        if word_idx >= self.words.len() {
            return;
        }

        let mask = 1u64 << bit_idx;
        if self.words[word_idx] & mask != 0 {
            self.words[word_idx] &= !mask;
            self.count -= 1;
        }
    }

    /// Check whether a file ID is tombstoned.
    pub fn contains(&self, file_id: FileId) -> bool {
        let (word_idx, bit_idx) = word_and_bit(file_id);

        if word_idx >= self.words.len() {
            return false;
        }

        self.words[word_idx] & (1u64 << bit_idx) != 0
    }

    /// Return the number of tombstoned file IDs.
    pub fn len(&self) -> u32 {
        self.count
    }

    /// Return `true` if no file IDs are tombstoned.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Return the maximum file ID that can be represented without growing the bitmap,
    /// or 0 if the bitmap is empty.
    ///
    /// This is the number of bits in the bitmap (i.e., `words.len() * 64`).
    pub fn capacity(&self) -> u32 {
        (self.words.len() as u32) * 64
    }

    /// Compute the ratio of tombstoned files to total files in the segment.
    ///
    /// Returns `0.0` if `total_files` is zero.
    pub fn tombstone_ratio(&self, total_files: u32) -> f32 {
        if total_files == 0 {
            return 0.0;
        }
        self.count as f32 / total_files as f32
    }

    /// Clear all tombstones.
    pub fn clear(&mut self) {
        for word in &mut self.words {
            *word = 0;
        }
        self.count = 0;
    }

    /// Iterate over all tombstoned file IDs, in ascending order.
    pub fn iter(&self) -> TombstoneIter<'_> {
        TombstoneIter {
            words: &self.words,
            word_idx: 0,
            current_word: self.words.first().copied().unwrap_or(0),
        }
    }

    /// Write the tombstone set to a file at the given path.
    ///
    /// Uses atomic temp-file-then-rename for crash safety. All multi-byte
    /// integers are little-endian.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::Io`] if any file I/O operation fails.
    pub fn write_to(&self, path: &Path) -> Result<(), IndexError> {
        let max_file_id = self.capacity();
        let bitmap_bytes = self.words.len() * 8;
        let total_size = HEADER_SIZE + bitmap_bytes;
        let mut buf = Vec::with_capacity(total_size);

        // Write header
        buf.extend_from_slice(&TOMB_MAGIC.to_le_bytes());
        buf.extend_from_slice(&TOMB_VERSION.to_le_bytes());
        buf.extend_from_slice(&max_file_id.to_le_bytes());
        buf.extend_from_slice(&self.count.to_le_bytes());

        // Write bitmap words
        for &word in &self.words {
            buf.extend_from_slice(&word.to_le_bytes());
        }

        // Atomic write: temp file then rename
        let parent = path.parent().unwrap_or(Path::new("."));
        let temp_path = parent.join(format!(".tombstone.bin.tmp.{}", std::process::id()));

        fs::write(&temp_path, &buf)?;
        fs::rename(&temp_path, path)?;

        Ok(())
    }

    /// Read a tombstone set from a file at the given path.
    ///
    /// Validates the magic number, format version, and that the file is large
    /// enough to contain the declared bitmap.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::IndexCorruption`] if the header is invalid or the
    /// file is too small, or [`IndexError::UnsupportedVersion`] if the format
    /// version is not supported.
    pub fn read_from(path: &Path) -> Result<Self, IndexError> {
        let data = fs::read(path)?;

        if data.len() < HEADER_SIZE {
            return Err(IndexError::IndexCorruption(
                "tombstone.bin too small for header".to_string(),
            ));
        }

        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if magic != TOMB_MAGIC {
            return Err(IndexError::IndexCorruption(format!(
                "invalid tombstone.bin magic: expected 0x{TOMB_MAGIC:08X}, got 0x{magic:08X}"
            )));
        }

        let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
        if version != TOMB_VERSION {
            return Err(IndexError::UnsupportedVersion {
                version: version as u32,
            });
        }

        let max_file_id = u32::from_le_bytes(data[6..10].try_into().unwrap());
        let tombstone_count = u32::from_le_bytes(data[10..14].try_into().unwrap());

        let num_words = word_count(max_file_id);
        let expected_size = HEADER_SIZE + num_words * 8;

        if data.len() < expected_size {
            return Err(IndexError::IndexCorruption(format!(
                "tombstone.bin too small: expected at least {expected_size} bytes for max_file_id={max_file_id}, got {}",
                data.len()
            )));
        }

        let mut words = Vec::with_capacity(num_words);
        for i in 0..num_words {
            let offset = HEADER_SIZE + i * 8;
            let word = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
            words.push(word);
        }

        // Verify the stored count matches the actual popcount
        let actual_count: u32 = words.iter().map(|w| w.count_ones()).sum();
        if actual_count != tombstone_count {
            return Err(IndexError::IndexCorruption(format!(
                "tombstone count mismatch: header says {tombstone_count}, bitmap has {actual_count} set bits"
            )));
        }

        Ok(TombstoneSet {
            words,
            count: tombstone_count,
        })
    }

    /// Merge another tombstone set into this one (union).
    ///
    /// After merging, this set contains all file IDs that were in either set.
    pub fn merge(&mut self, other: &TombstoneSet) {
        // Grow if needed
        if other.words.len() > self.words.len() {
            self.words.resize(other.words.len(), 0);
        }

        for (i, &other_word) in other.words.iter().enumerate() {
            self.words[i] |= other_word;
        }

        // Recompute count since we can't cheaply track it during OR
        self.count = self.words.iter().map(|w| w.count_ones()).sum();
    }
}

impl Default for TombstoneSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Iterator over tombstoned file IDs in ascending order.
pub struct TombstoneIter<'a> {
    words: &'a [u64],
    word_idx: usize,
    current_word: u64,
}

impl Iterator for TombstoneIter<'_> {
    type Item = FileId;

    fn next(&mut self) -> Option<FileId> {
        loop {
            if self.current_word != 0 {
                let bit = self.current_word.trailing_zeros();
                self.current_word &= self.current_word - 1; // clear lowest set bit
                let file_id = (self.word_idx as u32) * 64 + bit;
                return Some(FileId(file_id));
            }

            self.word_idx += 1;
            if self.word_idx >= self.words.len() {
                return None;
            }
            self.current_word = self.words[self.word_idx];
        }
    }
}

/// Compute the word index and bit index for a file ID.
fn word_and_bit(file_id: FileId) -> (usize, u32) {
    let id = file_id.0;
    ((id / 64) as usize, id % 64)
}

/// Compute the number of u64 words needed to represent file IDs up to `max_file_id - 1`.
fn word_count(max_file_id: u32) -> usize {
    if max_file_id == 0 {
        return 0;
    }
    (max_file_id as usize).div_ceil(64)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Task 1: Basic construction and operations ----

    #[test]
    fn test_new_is_empty() {
        let ts = TombstoneSet::new();
        assert!(ts.is_empty());
        assert_eq!(ts.len(), 0);
        assert_eq!(ts.capacity(), 0);
    }

    #[test]
    fn test_with_capacity() {
        let ts = TombstoneSet::with_capacity(100);
        assert!(ts.is_empty());
        assert_eq!(ts.len(), 0);
        // capacity should be at least 100, rounded up to next multiple of 64
        assert!(ts.capacity() >= 100);
        assert_eq!(ts.capacity(), 128); // ceil(100/64) * 64 = 128
    }

    #[test]
    fn test_with_capacity_zero() {
        let ts = TombstoneSet::with_capacity(0);
        assert!(ts.is_empty());
        assert_eq!(ts.capacity(), 0);
    }

    #[test]
    fn test_insert_and_contains() {
        let mut ts = TombstoneSet::new();

        assert!(!ts.contains(FileId(5)));
        ts.insert(FileId(5));
        assert!(ts.contains(FileId(5)));
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn test_insert_multiple() {
        let mut ts = TombstoneSet::new();

        ts.insert(FileId(0));
        ts.insert(FileId(1));
        ts.insert(FileId(63));
        ts.insert(FileId(64));
        ts.insert(FileId(1000));

        assert!(ts.contains(FileId(0)));
        assert!(ts.contains(FileId(1)));
        assert!(ts.contains(FileId(63)));
        assert!(ts.contains(FileId(64)));
        assert!(ts.contains(FileId(1000)));
        assert!(!ts.contains(FileId(2)));
        assert!(!ts.contains(FileId(999)));
        assert_eq!(ts.len(), 5);
    }

    #[test]
    fn test_insert_idempotent() {
        let mut ts = TombstoneSet::new();

        ts.insert(FileId(10));
        assert_eq!(ts.len(), 1);

        ts.insert(FileId(10));
        assert_eq!(ts.len(), 1); // still 1, not 2
        assert!(ts.contains(FileId(10)));
    }

    #[test]
    fn test_remove() {
        let mut ts = TombstoneSet::new();

        ts.insert(FileId(5));
        assert!(ts.contains(FileId(5)));
        assert_eq!(ts.len(), 1);

        ts.remove(FileId(5));
        assert!(!ts.contains(FileId(5)));
        assert_eq!(ts.len(), 0);
    }

    #[test]
    fn test_remove_not_present() {
        let mut ts = TombstoneSet::new();
        // Should not panic
        ts.remove(FileId(999));
        assert_eq!(ts.len(), 0);
    }

    #[test]
    fn test_remove_from_empty() {
        let mut ts = TombstoneSet::new();
        ts.remove(FileId(0));
        assert_eq!(ts.len(), 0);
    }

    #[test]
    fn test_contains_beyond_capacity() {
        let ts = TombstoneSet::with_capacity(64);
        // File ID beyond allocated capacity should return false, not panic
        assert!(!ts.contains(FileId(1000)));
    }

    #[test]
    fn test_clear() {
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(1));
        ts.insert(FileId(100));
        ts.insert(FileId(999));
        assert_eq!(ts.len(), 3);

        ts.clear();
        assert!(ts.is_empty());
        assert_eq!(ts.len(), 0);
        assert!(!ts.contains(FileId(1)));
        assert!(!ts.contains(FileId(100)));
        assert!(!ts.contains(FileId(999)));
    }

    #[test]
    fn test_auto_grow_on_insert() {
        let mut ts = TombstoneSet::new();
        assert_eq!(ts.capacity(), 0);

        ts.insert(FileId(200));
        assert!(ts.capacity() >= 201);
        assert!(ts.contains(FileId(200)));
    }

    // ---- Task 2: Iterator ----

    #[test]
    fn test_iter_empty() {
        let ts = TombstoneSet::new();
        let ids: Vec<FileId> = ts.iter().collect();
        assert!(ids.is_empty());
    }

    #[test]
    fn test_iter_single() {
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(42));

        let ids: Vec<FileId> = ts.iter().collect();
        assert_eq!(ids, vec![FileId(42)]);
    }

    #[test]
    fn test_iter_ascending_order() {
        let mut ts = TombstoneSet::new();
        // Insert in non-sorted order
        ts.insert(FileId(100));
        ts.insert(FileId(0));
        ts.insert(FileId(50));
        ts.insert(FileId(200));
        ts.insert(FileId(63));
        ts.insert(FileId(64));

        let ids: Vec<FileId> = ts.iter().collect();
        assert_eq!(
            ids,
            vec![
                FileId(0),
                FileId(50),
                FileId(63),
                FileId(64),
                FileId(100),
                FileId(200)
            ]
        );
    }

    #[test]
    fn test_iter_across_word_boundaries() {
        let mut ts = TombstoneSet::new();
        // Insert at word boundaries
        ts.insert(FileId(0)); // word 0, bit 0
        ts.insert(FileId(63)); // word 0, bit 63
        ts.insert(FileId(64)); // word 1, bit 0
        ts.insert(FileId(127)); // word 1, bit 63
        ts.insert(FileId(128)); // word 2, bit 0

        let ids: Vec<FileId> = ts.iter().collect();
        assert_eq!(
            ids,
            vec![FileId(0), FileId(63), FileId(64), FileId(127), FileId(128)]
        );
    }

    #[test]
    fn test_iter_count_matches_len() {
        let mut ts = TombstoneSet::new();
        for i in (0..500).step_by(7) {
            ts.insert(FileId(i));
        }

        let count = ts.iter().count();
        assert_eq!(count, ts.len() as usize);
    }

    // ---- Task 3: Binary persistence (write + read roundtrip) ----

    #[test]
    fn test_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");
        let ts = TombstoneSet::new();

        ts.write_to(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_roundtrip_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let ts = TombstoneSet::new();
        ts.write_to(&path).unwrap();

        let loaded = TombstoneSet::read_from(&path).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.len(), 0);
    }

    #[test]
    fn test_roundtrip_single() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let mut ts = TombstoneSet::new();
        ts.insert(FileId(42));
        ts.write_to(&path).unwrap();

        let loaded = TombstoneSet::read_from(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(FileId(42)));
        assert!(!loaded.contains(FileId(0)));
    }

    #[test]
    fn test_roundtrip_many() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let mut ts = TombstoneSet::new();
        let ids: Vec<u32> = vec![0, 1, 2, 63, 64, 65, 127, 128, 500, 999, 1000, 5000];
        for &id in &ids {
            ts.insert(FileId(id));
        }
        ts.write_to(&path).unwrap();

        let loaded = TombstoneSet::read_from(&path).unwrap();
        assert_eq!(loaded.len(), ids.len() as u32);
        for &id in &ids {
            assert!(loaded.contains(FileId(id)), "missing FileId({id})");
        }
        // Verify non-inserted IDs are absent
        assert!(!loaded.contains(FileId(3)));
        assert!(!loaded.contains(FileId(66)));
        assert!(!loaded.contains(FileId(501)));
    }

    #[test]
    fn test_roundtrip_preserves_iter_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let mut ts = TombstoneSet::new();
        ts.insert(FileId(200));
        ts.insert(FileId(5));
        ts.insert(FileId(100));
        ts.write_to(&path).unwrap();

        let loaded = TombstoneSet::read_from(&path).unwrap();
        let ids: Vec<FileId> = loaded.iter().collect();
        assert_eq!(ids, vec![FileId(5), FileId(100), FileId(200)]);
    }

    #[test]
    fn test_write_header_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let mut ts = TombstoneSet::new();
        ts.insert(FileId(5));
        ts.insert(FileId(100));
        ts.write_to(&path).unwrap();

        let data = std::fs::read(&path).unwrap();
        assert!(data.len() >= HEADER_SIZE);

        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        assert_eq!(magic, TOMB_MAGIC);

        let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
        assert_eq!(version, TOMB_VERSION);

        let max_file_id = u32::from_le_bytes(data[6..10].try_into().unwrap());
        assert!(max_file_id >= 101); // must cover file_id 100

        let tombstone_count = u32::from_le_bytes(data[10..14].try_into().unwrap());
        assert_eq!(tombstone_count, 2);
    }

    #[test]
    fn test_write_no_temp_file_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let ts = TombstoneSet::new();
        ts.write_to(&path).unwrap();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "tombstone.bin");
    }

    #[test]
    fn test_write_file_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let mut ts = TombstoneSet::with_capacity(128);
        ts.insert(FileId(0));
        ts.write_to(&path).unwrap();

        let data = std::fs::read(&path).unwrap();
        // Header (14) + 2 words (128 bits = 2 * 8 bytes = 16)
        assert_eq!(data.len(), HEADER_SIZE + 2 * 8);
    }

    // ---- Task 4: Error handling on read ----

    #[test]
    fn test_read_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.bin");

        let result = TombstoneSet::read_from(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), IndexError::Io(_)));
    }

    #[test]
    fn test_read_too_small() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        std::fs::write(&path, &[0u8; 5]).unwrap();
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
    fn test_read_invalid_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        data[4..6].copy_from_slice(&TOMB_VERSION.to_le_bytes());
        data[6..10].copy_from_slice(&0u32.to_le_bytes());
        data[10..14].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        let result = TombstoneSet::read_from(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("invalid tombstone.bin magic"));
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    #[test]
    fn test_read_invalid_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&TOMB_MAGIC.to_le_bytes());
        data[4..6].copy_from_slice(&99u16.to_le_bytes());
        data[6..10].copy_from_slice(&0u32.to_le_bytes());
        data[10..14].copy_from_slice(&0u32.to_le_bytes());
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
    fn test_read_truncated_bitmap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        // Header claims max_file_id=128 (needs 2 words = 16 bytes) but we only provide header
        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&TOMB_MAGIC.to_le_bytes());
        data[4..6].copy_from_slice(&TOMB_VERSION.to_le_bytes());
        data[6..10].copy_from_slice(&128u32.to_le_bytes());
        data[10..14].copy_from_slice(&0u32.to_le_bytes());
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
    fn test_read_count_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        // Write a valid file but with wrong count in header
        let mut data = vec![0u8; HEADER_SIZE + 8]; // 1 word
        data[0..4].copy_from_slice(&TOMB_MAGIC.to_le_bytes());
        data[4..6].copy_from_slice(&TOMB_VERSION.to_le_bytes());
        data[6..10].copy_from_slice(&64u32.to_le_bytes()); // max_file_id = 64
        data[10..14].copy_from_slice(&5u32.to_le_bytes()); // claims 5 tombstones
        // But bitmap word is 0 (no bits set)
        data[14..22].copy_from_slice(&0u64.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        let result = TombstoneSet::read_from(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("count mismatch"));
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    // ---- Task 5: Merge operation ----

    #[test]
    fn test_merge_both_empty() {
        let mut a = TombstoneSet::new();
        let b = TombstoneSet::new();
        a.merge(&b);
        assert!(a.is_empty());
    }

    #[test]
    fn test_merge_into_empty() {
        let mut a = TombstoneSet::new();
        let mut b = TombstoneSet::new();
        b.insert(FileId(10));
        b.insert(FileId(20));

        a.merge(&b);
        assert_eq!(a.len(), 2);
        assert!(a.contains(FileId(10)));
        assert!(a.contains(FileId(20)));
    }

    #[test]
    fn test_merge_from_empty() {
        let mut a = TombstoneSet::new();
        a.insert(FileId(5));
        let b = TombstoneSet::new();

        a.merge(&b);
        assert_eq!(a.len(), 1);
        assert!(a.contains(FileId(5)));
    }

    #[test]
    fn test_merge_disjoint() {
        let mut a = TombstoneSet::new();
        a.insert(FileId(1));
        a.insert(FileId(3));

        let mut b = TombstoneSet::new();
        b.insert(FileId(2));
        b.insert(FileId(4));

        a.merge(&b);
        assert_eq!(a.len(), 4);
        assert!(a.contains(FileId(1)));
        assert!(a.contains(FileId(2)));
        assert!(a.contains(FileId(3)));
        assert!(a.contains(FileId(4)));
    }

    #[test]
    fn test_merge_overlapping() {
        let mut a = TombstoneSet::new();
        a.insert(FileId(1));
        a.insert(FileId(2));
        a.insert(FileId(3));

        let mut b = TombstoneSet::new();
        b.insert(FileId(2));
        b.insert(FileId(3));
        b.insert(FileId(4));

        a.merge(&b);
        assert_eq!(a.len(), 4); // {1, 2, 3, 4}
        assert!(a.contains(FileId(1)));
        assert!(a.contains(FileId(2)));
        assert!(a.contains(FileId(3)));
        assert!(a.contains(FileId(4)));
    }

    #[test]
    fn test_merge_grows_bitmap() {
        let mut a = TombstoneSet::new();
        a.insert(FileId(0));

        let mut b = TombstoneSet::new();
        b.insert(FileId(1000));

        a.merge(&b);
        assert_eq!(a.len(), 2);
        assert!(a.contains(FileId(0)));
        assert!(a.contains(FileId(1000)));
    }

    // ---- Task 6: Edge cases and large bitmaps ----

    #[test]
    fn test_file_id_zero() {
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(0));
        assert!(ts.contains(FileId(0)));
        assert_eq!(ts.len(), 1);

        ts.remove(FileId(0));
        assert!(!ts.contains(FileId(0)));
        assert_eq!(ts.len(), 0);
    }

    #[test]
    fn test_large_file_id() {
        let mut ts = TombstoneSet::new();
        let large_id = FileId(100_000);
        ts.insert(large_id);
        assert!(ts.contains(large_id));
        assert_eq!(ts.len(), 1);

        // Verify no other IDs are set
        assert!(!ts.contains(FileId(0)));
        assert!(!ts.contains(FileId(99_999)));
        assert!(!ts.contains(FileId(100_001)));
    }

    #[test]
    fn test_word_boundary_ids() {
        // Test IDs at exact word boundaries (multiples of 64)
        let mut ts = TombstoneSet::new();
        for i in 0..10 {
            ts.insert(FileId(i * 64));
            ts.insert(FileId(i * 64 + 63));
        }
        assert_eq!(ts.len(), 20);

        for i in 0..10 {
            assert!(ts.contains(FileId(i * 64)));
            assert!(ts.contains(FileId(i * 64 + 63)));
            if i * 64 + 1 != (i + 1) * 64 - 1 {
                // Avoid checking 63 if it was already inserted
                assert!(!ts.contains(FileId(i * 64 + 1)));
            }
        }
    }

    #[test]
    fn test_consecutive_ids() {
        let mut ts = TombstoneSet::new();
        for i in 0..256 {
            ts.insert(FileId(i));
        }
        assert_eq!(ts.len(), 256);

        for i in 0..256 {
            assert!(ts.contains(FileId(i)));
        }
        assert!(!ts.contains(FileId(256)));
    }

    #[test]
    fn test_roundtrip_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tombstone.bin");

        let mut ts = TombstoneSet::new();
        // Insert every 3rd ID up to 10000
        let mut expected = Vec::new();
        for i in (0..10_000).step_by(3) {
            ts.insert(FileId(i));
            expected.push(FileId(i));
        }
        ts.write_to(&path).unwrap();

        let loaded = TombstoneSet::read_from(&path).unwrap();
        assert_eq!(loaded.len(), ts.len());

        let loaded_ids: Vec<FileId> = loaded.iter().collect();
        assert_eq!(loaded_ids, expected);
    }

    #[test]
    fn test_default_trait() {
        let ts = TombstoneSet::default();
        assert!(ts.is_empty());
        assert_eq!(ts.len(), 0);
    }

    #[test]
    fn test_clone() {
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(10));
        ts.insert(FileId(20));

        let cloned = ts.clone();
        assert_eq!(cloned.len(), 2);
        assert!(cloned.contains(FileId(10)));
        assert!(cloned.contains(FileId(20)));

        // Mutating original should not affect clone
        ts.insert(FileId(30));
        assert_eq!(cloned.len(), 2);
        assert!(!cloned.contains(FileId(30)));
    }

    #[test]
    fn test_debug_format() {
        let ts = TombstoneSet::new();
        let debug = format!("{ts:?}");
        assert!(debug.contains("TombstoneSet"));
    }

    // ---- tombstone_ratio and helper function tests ----

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
        use crate::changes::ChangeKind;

        assert!(!needs_tombstone(&ChangeKind::Created));
        assert!(needs_tombstone(&ChangeKind::Modified));
        assert!(needs_tombstone(&ChangeKind::Deleted));
        assert!(needs_tombstone(&ChangeKind::Renamed));
    }

    #[test]
    fn test_needs_new_entry() {
        use crate::changes::ChangeKind;

        assert!(needs_new_entry(&ChangeKind::Created));
        assert!(needs_new_entry(&ChangeKind::Modified));
        assert!(!needs_new_entry(&ChangeKind::Deleted));
        assert!(needs_new_entry(&ChangeKind::Renamed));
    }
}
