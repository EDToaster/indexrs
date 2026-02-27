//! Writer for serializing the trigram index to the on-disk binary format.
//!
//! [`TrigramIndexWriter`] takes a finalized [`PostingListBuilder`] and writes
//! it to a `trigrams.bin` file. The binary format is designed for efficient
//! memory-mapped reading with O(log n) trigram lookup via binary search.
//!
//! ## Binary Format
//!
//! ```text
//! [Header]  (10 bytes)
//!   magic: u32 = 0x54524947  ("TRIG")
//!   version: u16 = 1
//!   trigram_count: u32
//!
//! [Trigram Table]  (19 bytes per entry, sorted by trigram value)
//!   trigram: [u8; 3]
//!   file_list_offset: u32
//!   file_list_len: u32
//!   pos_list_offset: u32
//!   pos_list_len: u32
//!
//! [File Posting Lists]
//!   Delta-encoded, varint-compressed file_id sequences.
//!
//! [Positional Posting Lists]
//!   Grouped-by-file_id, delta-encoded offset sequences.
//! ```
//!
//! The writer uses atomic rename (write to temp file, then rename) for crash safety.

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::codec::{encode_delta_varint, encode_positional_postings};
use crate::error::IndexError;
use crate::posting::PostingListBuilder;
use crate::types::Trigram;

/// Magic number for trigrams.bin header: "TRIG" in ASCII as little-endian u32.
pub(crate) const TRIG_MAGIC: u32 = 0x5452_4947;

/// Current format version for trigrams.bin.
pub(crate) const TRIG_VERSION: u16 = 1;

/// Size of the header in bytes: magic(4) + version(2) + trigram_count(4).
pub(crate) const HEADER_SIZE: usize = 10;

/// Size of a single trigram table entry in bytes:
/// trigram(3) + file_list_offset(4) + file_list_len(4) + pos_list_offset(4) + pos_list_len(4).
pub(crate) const TABLE_ENTRY_SIZE: usize = 19;

/// Writer for the trigram index binary format.
///
/// Serializes a [`PostingListBuilder`] to a `trigrams.bin` file at a given path.
pub struct TrigramIndexWriter;

impl TrigramIndexWriter {
    /// Write the PostingListBuilder to a trigrams.bin file at the given path.
    ///
    /// The trigram table is sorted by trigram value (via [`Trigram::to_u32`]) to
    /// enable O(log n) binary search lookups. File posting lists and positional
    /// posting lists are encoded using delta-varint compression.
    ///
    /// For crash safety, the data is first written to a temporary file in the
    /// same directory, then atomically renamed to the target path.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::Io`] if any file I/O operation fails.
    pub fn write(builder: &PostingListBuilder, path: &Path) -> Result<(), IndexError> {
        // Collect and sort trigrams by their u32 value for binary search
        let mut trigrams: Vec<Trigram> = builder.file_postings().keys().copied().collect();
        trigrams.sort_by_key(|t| t.to_u32());

        let trigram_count = trigrams.len() as u32;

        // Encode all file posting lists, recording offsets and counts
        let mut file_postings_buf = Vec::new();
        let mut file_posting_entries: Vec<(u32, u32)> = Vec::with_capacity(trigrams.len()); // (offset, len)

        for trigram in &trigrams {
            let file_ids = &builder.file_postings()[trigram];
            let raw_ids: Vec<u32> = file_ids.iter().map(|fid| fid.0).collect();
            let encoded = encode_delta_varint(&raw_ids);

            let offset = file_postings_buf.len() as u32;
            let len = file_ids.len() as u32;
            file_postings_buf.extend_from_slice(&encoded);
            file_posting_entries.push((offset, len));
        }

        // Encode all positional posting lists, recording offsets and counts
        let mut pos_postings_buf = Vec::new();
        let mut pos_posting_entries: Vec<(u32, u32)> = Vec::with_capacity(trigrams.len()); // (offset, len)

        for trigram in &trigrams {
            let positions = &builder.positional_postings()[trigram];
            let raw_positions: Vec<(u32, u32)> =
                positions.iter().map(|(fid, off)| (fid.0, *off)).collect();
            let encoded = encode_positional_postings(&raw_positions);

            let offset = pos_postings_buf.len() as u32;
            let len = positions.len() as u32;
            pos_postings_buf.extend_from_slice(&encoded);
            pos_posting_entries.push((offset, len));
        }

        // Build the complete binary output
        let table_size = trigrams.len() * TABLE_ENTRY_SIZE;
        let total_size =
            HEADER_SIZE + table_size + file_postings_buf.len() + pos_postings_buf.len();
        let mut buf = Vec::with_capacity(total_size);

        // Write header
        buf.write_all(&TRIG_MAGIC.to_le_bytes())?;
        buf.write_all(&TRIG_VERSION.to_le_bytes())?;
        buf.write_all(&trigram_count.to_le_bytes())?;

        // Write trigram table
        for (i, trigram) in trigrams.iter().enumerate() {
            let (file_offset, file_len) = file_posting_entries[i];
            let (pos_offset, pos_len) = pos_posting_entries[i];

            buf.write_all(&trigram.0)?;
            buf.write_all(&file_offset.to_le_bytes())?;
            buf.write_all(&file_len.to_le_bytes())?;
            buf.write_all(&pos_offset.to_le_bytes())?;
            buf.write_all(&pos_len.to_le_bytes())?;
        }

        // Write file posting lists section
        buf.write_all(&file_postings_buf)?;

        // Write positional posting lists section
        buf.write_all(&pos_postings_buf)?;

        // Write to temp file, then atomic rename for crash safety
        let parent = path.parent().unwrap_or(Path::new("."));
        let temp_path = parent.join(format!(
            ".trigrams.bin.tmp.{}",
            std::process::id()
        ));

        fs::write(&temp_path, &buf)?;
        fs::rename(&temp_path, path)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::posting::PostingListBuilder;
    use crate::types::FileId;

    /// Build the Appendix A posting list builder (2 files).
    fn build_appendix_a() -> PostingListBuilder {
        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(0), b"fn main() {}");
        builder.add_file(FileId(1), b"fn parse() {}");
        builder.finalize();
        builder
    }

    #[test]
    fn test_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        let builder = build_appendix_a();

        TrigramIndexWriter::write(&builder, &path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_write_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        let builder = build_appendix_a();

        TrigramIndexWriter::write(&builder, &path).unwrap();
        let data = std::fs::read(&path).unwrap();

        // Check header
        assert!(data.len() >= HEADER_SIZE);
        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        assert_eq!(magic, TRIG_MAGIC);
        let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
        assert_eq!(version, TRIG_VERSION);
        let trigram_count = u32::from_le_bytes(data[6..10].try_into().unwrap());
        assert_eq!(trigram_count, 17); // Appendix A has 17 distinct trigrams
    }

    #[test]
    fn test_write_trigram_table_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        let builder = build_appendix_a();

        TrigramIndexWriter::write(&builder, &path).unwrap();
        let data = std::fs::read(&path).unwrap();

        let trigram_count =
            u32::from_le_bytes(data[6..10].try_into().unwrap()) as usize;

        // Verify trigrams in the table are sorted by u32 value
        let mut prev_val = 0u32;
        for i in 0..trigram_count {
            let entry_offset = HEADER_SIZE + i * TABLE_ENTRY_SIZE;
            let trigram_bytes = &data[entry_offset..entry_offset + 3];
            let t = Trigram::from_bytes(trigram_bytes[0], trigram_bytes[1], trigram_bytes[2]);
            let val = t.to_u32();
            if i > 0 {
                assert!(
                    val > prev_val,
                    "trigram table not sorted at index {i}: {prev_val} >= {val}"
                );
            }
            prev_val = val;
        }
    }

    #[test]
    fn test_write_file_size_reasonable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        let builder = build_appendix_a();

        TrigramIndexWriter::write(&builder, &path).unwrap();
        let data = std::fs::read(&path).unwrap();

        // Minimum size: header + 17 table entries
        let min_size = HEADER_SIZE + 17 * TABLE_ENTRY_SIZE;
        assert!(
            data.len() >= min_size,
            "file too small: {} < {}",
            data.len(),
            min_size
        );
    }

    #[test]
    fn test_write_empty_builder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        let builder = PostingListBuilder::new();

        TrigramIndexWriter::write(&builder, &path).unwrap();
        let data = std::fs::read(&path).unwrap();

        assert_eq!(data.len(), HEADER_SIZE);
        let trigram_count = u32::from_le_bytes(data[6..10].try_into().unwrap());
        assert_eq!(trigram_count, 0);
    }

    #[test]
    fn test_write_atomic_no_temp_file_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        let builder = build_appendix_a();

        TrigramIndexWriter::write(&builder, &path).unwrap();

        // No temp file should remain
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0] == "trigrams.bin");
    }
}
