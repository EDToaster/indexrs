//! File metadata index for indexrs.
//!
//! This module provides the structured index mapping `file_id` to file metadata,
//! with a path string pool for efficient storage. It supports both in-memory
//! building via [`MetadataBuilder`] and zero-copy reading from binary format
//! via [`MetadataReader`].
//!
//! ## Binary Format
//!
//! **meta.bin** contains a fixed-size header followed by fixed-size entries:
//!
//! ```text
//! [Header]  (10 bytes)
//!   magic: u32 = 0x4D455441  ("META")
//!   version: u16 = 1
//!   entry_count: u32
//!
//! [Entries]  (58 bytes each, indexed by file_id)
//!   file_id: u32
//!   path_offset: u32     (into paths.bin)
//!   path_len: u32
//!   content_hash: [u8; 16]
//!   language: u16
//!   size_bytes: u32
//!   mtime_epoch_secs: u64
//!   line_count: u32
//!   content_offset: u64
//!   content_len: u32
//! ```
//!
//! **paths.bin** is a contiguous buffer of UTF-8 path strings with no separators;
//! the offset and length from each meta.bin entry index into this buffer.

use std::collections::HashMap;
use std::io::Write;

use serde::{Deserialize, Serialize};

use crate::error::IndexError;
use crate::types::{FileId, Language};

/// Magic number for meta.bin header: "META" in ASCII as little-endian u32.
pub(crate) const META_MAGIC: u32 = 0x4D45_5441;

/// Current format version.
pub(crate) const META_VERSION: u16 = 1;

/// Size of the meta.bin header in bytes: magic(4) + version(2) + entry_count(4).
const HEADER_SIZE: usize = 10;

/// Size of a single entry in meta.bin in bytes.
const ENTRY_SIZE: usize = 58;

/// Entry for one indexed file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    /// Unique file identifier.
    pub file_id: FileId,
    /// Relative path from the repository root.
    pub path: String,
    /// Blake3 content hash truncated to 16 bytes.
    pub content_hash: [u8; 16],
    /// Detected programming language.
    pub language: Language,
    /// File size in bytes.
    pub size_bytes: u32,
    /// Last modification time as seconds since the Unix epoch.
    pub mtime_epoch_secs: u64,
    /// Number of lines in the file.
    pub line_count: u32,
    /// Byte offset into the content store.
    pub content_offset: u64,
    /// Compressed length in the content store.
    pub content_len: u32,
}

/// Builder for creating the metadata index in memory.
///
/// Files are added via [`add_file`](MetadataBuilder::add_file) and can be
/// looked up by file ID or path. The builder can serialize the index to
/// the binary on-disk format via [`write_to`](MetadataBuilder::write_to).
pub struct MetadataBuilder {
    entries: Vec<FileMetadata>,
    path_to_index: HashMap<String, usize>,
    next_file_id: u32,
}

impl MetadataBuilder {
    /// Create a new empty metadata builder.
    pub fn new() -> Self {
        MetadataBuilder {
            entries: Vec::new(),
            path_to_index: HashMap::new(),
            next_file_id: 0,
        }
    }

    /// Add a file metadata entry to the index.
    ///
    /// The entry's `file_id` is used as-is. The caller is responsible for
    /// assigning sequential IDs via [`next_file_id`](MetadataBuilder::next_file_id).
    pub fn add_file(&mut self, metadata: FileMetadata) {
        let index = self.entries.len();
        self.path_to_index.insert(metadata.path.clone(), index);
        if metadata.file_id.0 >= self.next_file_id {
            self.next_file_id = metadata.file_id.0 + 1;
        }
        self.entries.push(metadata);
    }

    /// Return the next available file ID.
    ///
    /// This is one greater than the highest file ID that has been added,
    /// or `FileId(0)` if no files have been added.
    pub fn next_file_id(&self) -> FileId {
        FileId(self.next_file_id)
    }

    /// Look up a file metadata entry by its file ID.
    ///
    /// Returns `None` if no entry with the given ID exists.
    pub fn get(&self, file_id: FileId) -> Option<&FileMetadata> {
        self.entries.iter().find(|e| e.file_id == file_id)
    }

    /// Look up a file metadata entry by its path.
    ///
    /// Returns `None` if no entry with the given path exists.
    pub fn get_by_path(&self, path: &str) -> Option<&FileMetadata> {
        self.path_to_index.get(path).map(|&idx| &self.entries[idx])
    }

    /// Return the number of file entries in the index.
    pub fn file_count(&self) -> usize {
        self.entries.len()
    }

    /// Iterate over all file metadata entries.
    pub fn iter(&self) -> impl Iterator<Item = &FileMetadata> {
        self.entries.iter()
    }

    /// Write the metadata index to the binary on-disk format.
    ///
    /// Writes fixed-size entries to `meta_writer` and path strings to
    /// `paths_writer`. All multi-byte integers are little-endian.
    pub fn write_to(
        &self,
        meta_writer: &mut impl Write,
        paths_writer: &mut impl Write,
    ) -> std::io::Result<()> {
        // Write header
        meta_writer.write_all(&META_MAGIC.to_le_bytes())?;
        meta_writer.write_all(&META_VERSION.to_le_bytes())?;
        let entry_count: u32 = self.entries.len().try_into().map_err(|_| {
            std::io::Error::other(format!(
                "entry count {} exceeds u32::MAX",
                self.entries.len()
            ))
        })?;
        meta_writer.write_all(&entry_count.to_le_bytes())?;

        // Track path pool offset
        let mut path_offset: u32 = 0;

        // Write entries
        for entry in &self.entries {
            let path_bytes = entry.path.as_bytes();
            let path_len: u32 = path_bytes.len().try_into().map_err(|_| {
                std::io::Error::other(format!("path too long for u32: {}", entry.path))
            })?;

            meta_writer.write_all(&entry.file_id.0.to_le_bytes())?;
            meta_writer.write_all(&path_offset.to_le_bytes())?;
            meta_writer.write_all(&path_len.to_le_bytes())?;
            meta_writer.write_all(&entry.content_hash)?;
            meta_writer.write_all(&entry.language.to_u16().to_le_bytes())?;
            meta_writer.write_all(&entry.size_bytes.to_le_bytes())?;
            meta_writer.write_all(&entry.mtime_epoch_secs.to_le_bytes())?;
            meta_writer.write_all(&entry.line_count.to_le_bytes())?;
            meta_writer.write_all(&entry.content_offset.to_le_bytes())?;
            meta_writer.write_all(&entry.content_len.to_le_bytes())?;

            // Write path to paths pool
            paths_writer.write_all(path_bytes)?;

            path_offset += path_len;
        }

        Ok(())
    }
}

impl Default for MetadataBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Reader for memory-mapped metadata stored in the binary on-disk format.
///
/// Operates over byte slices (typically from `mmap`) and performs zero-copy
/// reads of the fixed-size entry table, resolving path strings from a
/// separate paths buffer.
#[derive(Debug)]
pub struct MetadataReader<'a> {
    data: &'a [u8],
    paths: &'a [u8],
    entry_count: u32,
}

impl<'a> MetadataReader<'a> {
    /// Create a new reader over the given meta.bin and paths.bin byte slices.
    ///
    /// Validates the magic number, format version, and that the data is large
    /// enough to contain the declared number of entries.
    pub fn new(meta_data: &'a [u8], paths_data: &'a [u8]) -> Result<Self, IndexError> {
        if meta_data.len() < HEADER_SIZE {
            return Err(IndexError::IndexCorruption(
                "meta.bin too small for header".to_string(),
            ));
        }

        let magic = u32::from_le_bytes(meta_data[0..4].try_into().unwrap());
        if magic != META_MAGIC {
            return Err(IndexError::IndexCorruption(format!(
                "invalid meta.bin magic: expected 0x{META_MAGIC:08X}, got 0x{magic:08X}"
            )));
        }

        let version = u16::from_le_bytes(meta_data[4..6].try_into().unwrap());
        if version != META_VERSION {
            return Err(IndexError::UnsupportedVersion {
                version: version as u32,
            });
        }

        let entry_count = u32::from_le_bytes(meta_data[6..10].try_into().unwrap());

        let expected_size = HEADER_SIZE + (entry_count as usize) * ENTRY_SIZE;
        if meta_data.len() < expected_size {
            return Err(IndexError::IndexCorruption(format!(
                "meta.bin too small: expected at least {expected_size} bytes for {entry_count} entries, got {}",
                meta_data.len()
            )));
        }

        Ok(MetadataReader {
            data: meta_data,
            paths: paths_data,
            entry_count,
        })
    }

    /// Create a reader without re-validating the header.
    ///
    /// # Safety (logical, not memory)
    ///
    /// The caller must guarantee that `meta_data` has already been validated
    /// by a prior call to [`MetadataReader::new()`]. This is the case for
    /// `Segment`, which validates during `open()` and stores the mmaps.
    pub(crate) fn new_unchecked(
        meta_data: &'a [u8],
        paths_data: &'a [u8],
        entry_count: u32,
    ) -> Self {
        MetadataReader {
            data: meta_data,
            paths: paths_data,
            entry_count,
        }
    }

    /// Return the number of entries in the metadata index.
    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }

    /// Look up a file metadata entry by file ID.
    ///
    /// Tries O(1) direct indexing first (works when file IDs are sequential
    /// starting from 0, which is the common case). Falls back to a linear
    /// scan for non-contiguous IDs.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::IndexCorruption`] if the entry's path data is
    /// out of bounds or contains invalid UTF-8.
    pub fn get(&self, file_id: FileId) -> Result<Option<FileMetadata>, IndexError> {
        // Fast path: try direct indexing (O(1) when IDs are sequential).
        if file_id.0 < self.entry_count {
            let entry = self.read_entry(file_id.0)?;
            if entry.file_id == file_id {
                return Ok(Some(entry));
            }
        }

        // Slow path: linear scan for non-contiguous IDs.
        for i in 0..self.entry_count {
            let entry = self.read_entry(i)?;
            if entry.file_id == file_id {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// Look up only the `size_bytes` field for a file by its ID.
    ///
    /// This is a lightweight alternative to [`get()`](Self::get) that avoids
    /// deserializing the full entry (no path string allocation). Useful for
    /// sorting candidates by file size before verification.
    ///
    /// Uses O(1) direct indexing when file IDs are sequential (the common case).
    pub fn get_size_bytes(&self, file_id: FileId) -> Result<Option<u32>, IndexError> {
        // Byte offset of size_bytes within a 58-byte entry: bytes [30..34]
        const SIZE_BYTES_OFFSET: usize = 30;

        // Fast path: direct indexing (O(1) when IDs are sequential)
        if file_id.0 < self.entry_count {
            let entry_offset = HEADER_SIZE + (file_id.0 as usize) * ENTRY_SIZE;
            let entry_data = &self.data[entry_offset..entry_offset + ENTRY_SIZE];
            let stored_id = u32::from_le_bytes(entry_data[0..4].try_into().unwrap());
            if stored_id == file_id.0 {
                let size = u32::from_le_bytes(
                    entry_data[SIZE_BYTES_OFFSET..SIZE_BYTES_OFFSET + 4]
                        .try_into()
                        .unwrap(),
                );
                return Ok(Some(size));
            }
        }

        // Slow path: linear scan for non-contiguous IDs
        for i in 0..self.entry_count {
            let entry_offset = HEADER_SIZE + (i as usize) * ENTRY_SIZE;
            let entry_data = &self.data[entry_offset..entry_offset + ENTRY_SIZE];
            let stored_id = u32::from_le_bytes(entry_data[0..4].try_into().unwrap());
            if stored_id == file_id.0 {
                let size = u32::from_le_bytes(
                    entry_data[SIZE_BYTES_OFFSET..SIZE_BYTES_OFFSET + 4]
                        .try_into()
                        .unwrap(),
                );
                return Ok(Some(size));
            }
        }

        Ok(None)
    }

    /// Iterate over all file metadata entries in order of index position.
    ///
    /// Returns an iterator yielding `Result<FileMetadata, IndexError>` for
    /// each entry. Errors are returned if an entry's path data is out of
    /// bounds or contains invalid UTF-8.
    pub fn iter_all(&self) -> impl Iterator<Item = Result<FileMetadata, IndexError>> + '_ {
        (0..self.entry_count).map(move |i| self.read_entry(i))
    }

    /// Read the entry at the given index (0-based).
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::IndexCorruption`] if the path offset/length is
    /// out of bounds in the paths pool, or if the path bytes are not valid UTF-8.
    fn read_entry(&self, index: u32) -> Result<FileMetadata, IndexError> {
        let offset = HEADER_SIZE + (index as usize) * ENTRY_SIZE;
        let entry_data = &self.data[offset..offset + ENTRY_SIZE];

        let file_id = FileId(u32::from_le_bytes(entry_data[0..4].try_into().unwrap()));
        let path_offset = u32::from_le_bytes(entry_data[4..8].try_into().unwrap()) as usize;
        let path_len = u32::from_le_bytes(entry_data[8..12].try_into().unwrap()) as usize;

        let mut content_hash = [0u8; 16];
        content_hash.copy_from_slice(&entry_data[12..28]);

        let language =
            Language::from_u16(u16::from_le_bytes(entry_data[28..30].try_into().unwrap()));
        let size_bytes = u32::from_le_bytes(entry_data[30..34].try_into().unwrap());
        let mtime_epoch_secs = u64::from_le_bytes(entry_data[34..42].try_into().unwrap());
        let line_count = u32::from_le_bytes(entry_data[42..46].try_into().unwrap());
        let content_offset = u64::from_le_bytes(entry_data[46..54].try_into().unwrap());
        let content_len = u32::from_le_bytes(entry_data[54..58].try_into().unwrap());

        let path_end = path_offset.checked_add(path_len).ok_or_else(|| {
            IndexError::IndexCorruption(format!(
                "path offset overflow: offset={path_offset}, len={path_len}"
            ))
        })?;
        if path_end > self.paths.len() {
            return Err(IndexError::IndexCorruption(format!(
                "path data out of bounds: offset={path_offset}, len={path_len}, pool_size={}",
                self.paths.len()
            )));
        }
        let path = std::str::from_utf8(&self.paths[path_offset..path_end])
            .map_err(|e| IndexError::IndexCorruption(format!("invalid UTF-8 in path: {e}")))?
            .to_string();

        Ok(FileMetadata {
            file_id,
            path,
            content_hash,
            language,
            size_bytes,
            mtime_epoch_secs,
            line_count,
            content_offset,
            content_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a test FileMetadata entry.
    fn make_entry(id: u32, path: &str, lang: Language) -> FileMetadata {
        FileMetadata {
            file_id: FileId(id),
            path: path.to_string(),
            content_hash: [
                id as u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
                0x0D, 0x0E, 0x0F,
            ],
            language: lang,
            size_bytes: 1000 + id,
            mtime_epoch_secs: 1_700_000_000 + id as u64,
            line_count: 50 + id,
            content_offset: 4096 * id as u64,
            content_len: 500 + id,
        }
    }

    // ---- MetadataBuilder tests ----

    #[test]
    fn test_builder_add_and_get() {
        let mut builder = MetadataBuilder::new();
        let entry0 = make_entry(0, "src/main.rs", Language::Rust);
        let entry1 = make_entry(1, "src/lib.rs", Language::Rust);
        let entry2 = make_entry(2, "README.md", Language::Markdown);

        builder.add_file(entry0);
        builder.add_file(entry1);
        builder.add_file(entry2);

        let got = builder.get(FileId(0)).unwrap();
        assert_eq!(got.path, "src/main.rs");
        assert_eq!(got.language, Language::Rust);
        assert_eq!(got.size_bytes, 1000);

        let got = builder.get(FileId(1)).unwrap();
        assert_eq!(got.path, "src/lib.rs");

        let got = builder.get(FileId(2)).unwrap();
        assert_eq!(got.path, "README.md");
        assert_eq!(got.language, Language::Markdown);
    }

    #[test]
    fn test_builder_get_by_path() {
        let mut builder = MetadataBuilder::new();
        builder.add_file(make_entry(0, "src/main.rs", Language::Rust));
        builder.add_file(make_entry(1, "src/lib.rs", Language::Rust));

        let got = builder.get_by_path("src/main.rs").unwrap();
        assert_eq!(got.file_id, FileId(0));

        let got = builder.get_by_path("src/lib.rs").unwrap();
        assert_eq!(got.file_id, FileId(1));
    }

    #[test]
    fn test_builder_not_found() {
        let mut builder = MetadataBuilder::new();
        builder.add_file(make_entry(0, "src/main.rs", Language::Rust));

        assert!(builder.get(FileId(99)).is_none());
        assert!(builder.get_by_path("nonexistent.rs").is_none());
    }

    #[test]
    fn test_builder_file_count() {
        let mut builder = MetadataBuilder::new();
        assert_eq!(builder.file_count(), 0);

        builder.add_file(make_entry(0, "a.rs", Language::Rust));
        assert_eq!(builder.file_count(), 1);

        builder.add_file(make_entry(1, "b.rs", Language::Rust));
        assert_eq!(builder.file_count(), 2);

        builder.add_file(make_entry(2, "c.py", Language::Python));
        assert_eq!(builder.file_count(), 3);
    }

    #[test]
    fn test_builder_empty() {
        let builder = MetadataBuilder::new();
        assert_eq!(builder.file_count(), 0);
        assert!(builder.get(FileId(0)).is_none());
        assert!(builder.get_by_path("anything").is_none());
        assert_eq!(builder.next_file_id(), FileId(0));
    }

    #[test]
    fn test_builder_next_file_id() {
        let mut builder = MetadataBuilder::new();
        assert_eq!(builder.next_file_id(), FileId(0));

        builder.add_file(make_entry(0, "a.rs", Language::Rust));
        assert_eq!(builder.next_file_id(), FileId(1));

        builder.add_file(make_entry(1, "b.rs", Language::Rust));
        assert_eq!(builder.next_file_id(), FileId(2));

        // Non-sequential IDs: add file_id 5
        builder.add_file(make_entry(5, "c.rs", Language::Rust));
        assert_eq!(builder.next_file_id(), FileId(6));
    }

    #[test]
    fn test_builder_iter() {
        let mut builder = MetadataBuilder::new();
        builder.add_file(make_entry(0, "a.rs", Language::Rust));
        builder.add_file(make_entry(1, "b.py", Language::Python));

        let paths: Vec<&str> = builder.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "b.py"]);
    }

    // ---- Binary serialization tests ----

    #[test]
    fn test_binary_size() {
        let mut builder = MetadataBuilder::new();
        builder.add_file(make_entry(0, "src/main.rs", Language::Rust));
        builder.add_file(make_entry(1, "src/lib.rs", Language::Rust));
        builder.add_file(make_entry(2, "README.md", Language::Markdown));

        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let expected_meta_size = HEADER_SIZE + 3 * ENTRY_SIZE;
        assert_eq!(meta_buf.len(), expected_meta_size);
        assert_eq!(expected_meta_size, 10 + 3 * 58);

        // paths.bin should contain all path strings concatenated
        let expected_paths_len = "src/main.rs".len() + "src/lib.rs".len() + "README.md".len();
        assert_eq!(paths_buf.len(), expected_paths_len);
    }

    #[test]
    fn test_write_empty() {
        let builder = MetadataBuilder::new();
        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        assert_eq!(meta_buf.len(), HEADER_SIZE);
        assert_eq!(paths_buf.len(), 0);

        // Verify header contents
        let magic = u32::from_le_bytes(meta_buf[0..4].try_into().unwrap());
        assert_eq!(magic, META_MAGIC);
        let version = u16::from_le_bytes(meta_buf[4..6].try_into().unwrap());
        assert_eq!(version, META_VERSION);
        let count = u32::from_le_bytes(meta_buf[6..10].try_into().unwrap());
        assert_eq!(count, 0);
    }

    // ---- MetadataReader tests ----

    #[test]
    fn test_roundtrip() {
        let mut builder = MetadataBuilder::new();
        let entries = vec![
            make_entry(0, "src/main.rs", Language::Rust),
            make_entry(1, "src/lib.rs", Language::Rust),
            make_entry(2, "README.md", Language::Markdown),
        ];
        for entry in &entries {
            builder.add_file(entry.clone());
        }

        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();
        assert_eq!(reader.entry_count(), 3);

        for original in &entries {
            let read_back = reader.get(original.file_id).unwrap().unwrap();
            assert_eq!(read_back.file_id, original.file_id);
            assert_eq!(read_back.path, original.path);
            assert_eq!(read_back.content_hash, original.content_hash);
            assert_eq!(read_back.language, original.language);
            assert_eq!(read_back.size_bytes, original.size_bytes);
            assert_eq!(read_back.mtime_epoch_secs, original.mtime_epoch_secs);
            assert_eq!(read_back.line_count, original.line_count);
            assert_eq!(read_back.content_offset, original.content_offset);
            assert_eq!(read_back.content_len, original.content_len);
        }
    }

    #[test]
    fn test_roundtrip_empty() {
        let builder = MetadataBuilder::new();
        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();
        assert_eq!(reader.entry_count(), 0);
        assert!(reader.get(FileId(0)).unwrap().is_none());
    }

    #[test]
    fn test_roundtrip_paths() {
        let mut builder = MetadataBuilder::new();
        let paths = [
            "src/main.rs",
            "src/deep/nested/dir/file.py",
            "Cargo.toml",
            "tests/integration/test_search.rs",
        ];
        for (i, path) in paths.iter().enumerate() {
            builder.add_file(make_entry(i as u32, path, Language::Rust));
        }

        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();

        for (i, expected_path) in paths.iter().enumerate() {
            let entry = reader.get(FileId(i as u32)).unwrap().unwrap();
            assert_eq!(entry.path, *expected_path);
        }
    }

    #[test]
    fn test_reader_not_found() {
        let mut builder = MetadataBuilder::new();
        builder.add_file(make_entry(0, "a.rs", Language::Rust));

        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();
        assert!(reader.get(FileId(99)).unwrap().is_none());
    }

    #[test]
    fn test_reader_invalid_magic() {
        let mut data = vec![0u8; HEADER_SIZE];
        // Write wrong magic
        data[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        data[4..6].copy_from_slice(&META_VERSION.to_le_bytes());
        data[6..10].copy_from_slice(&0u32.to_le_bytes());

        let result = MetadataReader::new(&data, &[]);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("invalid meta.bin magic"));
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    #[test]
    fn test_reader_invalid_version() {
        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&META_MAGIC.to_le_bytes());
        data[4..6].copy_from_slice(&99u16.to_le_bytes()); // bad version
        data[6..10].copy_from_slice(&0u32.to_le_bytes());

        let result = MetadataReader::new(&data, &[]);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::UnsupportedVersion { version } => {
                assert_eq!(version, 99);
            }
            other => panic!("expected UnsupportedVersion, got: {other}"),
        }
    }

    #[test]
    fn test_reader_too_small() {
        // Header says 1 entry but data is only header-sized
        let mut data = vec![0u8; HEADER_SIZE];
        data[0..4].copy_from_slice(&META_MAGIC.to_le_bytes());
        data[4..6].copy_from_slice(&META_VERSION.to_le_bytes());
        data[6..10].copy_from_slice(&1u32.to_le_bytes()); // claims 1 entry

        let result = MetadataReader::new(&data, &[]);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("too small"));
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    #[test]
    fn test_roundtrip_all_languages() {
        let languages = [
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::JavaScript,
            Language::Go,
            Language::C,
            Language::Cpp,
            Language::Java,
            Language::Ruby,
            Language::Shell,
            Language::Markdown,
            Language::Yaml,
            Language::Toml,
            Language::Json,
            Language::Xml,
            Language::Html,
            Language::Css,
            Language::Scss,
            Language::Sass,
            Language::Sql,
            Language::Protobuf,
            Language::Dockerfile,
            Language::Hcl,
            Language::Kotlin,
            Language::Swift,
            Language::Scala,
            Language::Elixir,
            Language::Erlang,
            Language::Haskell,
            Language::OCaml,
            Language::Lua,
            Language::Perl,
            Language::R,
            Language::Dart,
            Language::Zig,
            Language::Nix,
            Language::Unknown,
        ];

        let mut builder = MetadataBuilder::new();
        for (i, &lang) in languages.iter().enumerate() {
            builder.add_file(make_entry(i as u32, &format!("file_{i}.txt"), lang));
        }

        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();
        assert_eq!(reader.entry_count(), languages.len() as u32);

        for (i, &expected_lang) in languages.iter().enumerate() {
            let entry = reader.get(FileId(i as u32)).unwrap().unwrap();
            assert_eq!(
                entry.language, expected_lang,
                "language mismatch at index {i}"
            );
        }
    }

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

    #[test]
    fn test_roundtrip_large_values() {
        let mut builder = MetadataBuilder::new();
        builder.add_file(FileMetadata {
            file_id: FileId(u32::MAX - 1),
            path: "large.rs".to_string(),
            content_hash: [0xFF; 16],
            language: Language::Rust,
            size_bytes: u32::MAX,
            mtime_epoch_secs: u64::MAX,
            line_count: u32::MAX,
            content_offset: u64::MAX,
            content_len: u32::MAX,
        });

        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();
        let entry = reader.get(FileId(u32::MAX - 1)).unwrap().unwrap();
        assert_eq!(entry.size_bytes, u32::MAX);
        assert_eq!(entry.mtime_epoch_secs, u64::MAX);
        assert_eq!(entry.line_count, u32::MAX);
        assert_eq!(entry.content_offset, u64::MAX);
        assert_eq!(entry.content_len, u32::MAX);
        assert_eq!(entry.content_hash, [0xFF; 16]);
    }

    #[test]
    fn test_reader_get_size_bytes() {
        let mut builder = MetadataBuilder::new();
        builder.add_file(make_entry(0, "small.rs", Language::Rust)); // size_bytes = 1000
        builder.add_file(make_entry(1, "medium.rs", Language::Rust)); // size_bytes = 1001
        builder.add_file(make_entry(2, "large.rs", Language::Rust)); // size_bytes = 1002

        let mut meta_buf = Vec::new();
        let mut paths_buf = Vec::new();
        builder.write_to(&mut meta_buf, &mut paths_buf).unwrap();

        let reader = MetadataReader::new(&meta_buf, &paths_buf).unwrap();

        assert_eq!(reader.get_size_bytes(FileId(0)).unwrap(), Some(1000));
        assert_eq!(reader.get_size_bytes(FileId(1)).unwrap(), Some(1001));
        assert_eq!(reader.get_size_bytes(FileId(2)).unwrap(), Some(1002));
        assert_eq!(reader.get_size_bytes(FileId(99)).unwrap(), None);
    }
}
