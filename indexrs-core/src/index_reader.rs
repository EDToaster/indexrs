//! Memory-mapped trigram index reader.
//!
//! [`TrigramIndexReader`] opens and memory-maps a `trigrams.bin` file written
//! by [`TrigramIndexWriter`](crate::index_writer::TrigramIndexWriter),
//! providing O(log n) trigram lookup via binary search on the sorted trigram table.
//!
//! # Usage
//!
//! ```no_run
//! use indexrs_core::index_reader::TrigramIndexReader;
//! use indexrs_core::types::Trigram;
//! use std::path::Path;
//!
//! let reader = TrigramIndexReader::open(Path::new("trigrams.bin")).unwrap();
//! let file_ids = reader.lookup_file_ids(Trigram::from_bytes(b'f', b'n', b' ')).unwrap();
//! ```

use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

use crate::codec::{decode_delta_varint, decode_positional_postings};
use crate::error::IndexError;
use crate::index_writer::{HEADER_SIZE, TABLE_ENTRY_SIZE, TRIG_MAGIC, TRIG_VERSION};
use crate::types::{FileId, Trigram};

/// Memory-mapped reader for the trigram index binary format.
///
/// Provides O(log n) trigram lookup by binary-searching the sorted trigram table,
/// then decoding posting lists on demand from the memory-mapped data.
pub struct TrigramIndexReader {
    mmap: Mmap,
    trigram_count: u32,
    /// Byte offset where the trigram table starts (immediately after header).
    table_offset: usize,
    /// Byte offset where the file posting lists section starts.
    file_postings_offset: usize,
    /// Byte offset where the positional posting lists section starts.
    pos_postings_offset: usize,
}

impl std::fmt::Debug for TrigramIndexReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrigramIndexReader")
            .field("trigram_count", &self.trigram_count)
            .field("mmap_size", &self.mmap.len())
            .finish_non_exhaustive()
    }
}

impl TrigramIndexReader {
    /// Open and memory-map a trigrams.bin file.
    ///
    /// Validates the magic number and format version on open. Returns
    /// [`IndexError::IndexCorruption`] if the header is invalid or the file
    /// is too small, or [`IndexError::UnsupportedVersion`] if the format
    /// version is not supported.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let file = File::open(path)?;
        // SAFETY: We treat the mmap as read-only. The file must not be modified
        // externally while mapped; segments are immutable once written.
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < HEADER_SIZE {
            return Err(IndexError::IndexCorruption(
                "trigrams.bin too small for header".to_string(),
            ));
        }

        let magic = u32::from_le_bytes(mmap[0..4].try_into().unwrap());
        if magic != TRIG_MAGIC {
            return Err(IndexError::IndexCorruption(format!(
                "invalid trigrams.bin magic: expected 0x{TRIG_MAGIC:08X}, got 0x{magic:08X}"
            )));
        }

        let version = u16::from_le_bytes(mmap[4..6].try_into().unwrap());
        if version != TRIG_VERSION {
            return Err(IndexError::UnsupportedVersion {
                version: version as u32,
            });
        }

        let trigram_count = u32::from_le_bytes(mmap[6..10].try_into().unwrap());

        let table_offset = HEADER_SIZE;
        let table_size = trigram_count as usize * TABLE_ENTRY_SIZE;
        let file_postings_offset = table_offset + table_size;

        // Validate that the file is large enough for the table
        if mmap.len() < file_postings_offset {
            return Err(IndexError::IndexCorruption(format!(
                "trigrams.bin too small: expected at least {} bytes for {} trigrams, got {}",
                file_postings_offset,
                trigram_count,
                mmap.len()
            )));
        }

        // Calculate the positional postings section offset. The file layout is:
        //   [header][trigram table][file postings][positional postings]
        //
        // The writer writes file postings sequentially in trigram-sort order,
        // so the last table entry has the largest file_list_offset. We find the
        // end of the file postings section by locating the last entry's data
        // and decoding exactly file_list_len varint values to determine its
        // byte size.
        let pos_postings_offset = if trigram_count == 0 {
            file_postings_offset
        } else {
            // Find the entry with the largest file_list_offset.
            // Since the writer writes entries in trigram-sort order and offsets
            // are sequential, this is the last entry.
            let mut max_file_offset = 0u32;
            let mut max_file_len = 0u32;
            for i in 0..trigram_count as usize {
                let entry_start = table_offset + i * TABLE_ENTRY_SIZE;
                let flo = u32::from_le_bytes(
                    mmap[entry_start + 3..entry_start + 7].try_into().unwrap(),
                );
                let fll = u32::from_le_bytes(
                    mmap[entry_start + 7..entry_start + 11].try_into().unwrap(),
                );
                if flo >= max_file_offset {
                    max_file_offset = flo;
                    max_file_len = fll;
                }
            }

            // Decode the file posting at max_file_offset to find its byte size.
            let data_start = file_postings_offset + max_file_offset as usize;
            // Read varints until we've decoded max_file_len values
            let remaining = &mmap[data_start..];
            let mut cursor = std::io::Cursor::new(remaining);
            let mut prev = 0u32;
            for _ in 0..max_file_len {
                use integer_encoding::VarIntReader;
                let delta: u32 = cursor.read_varint().map_err(|_| {
                    IndexError::IndexCorruption(
                        "failed to decode file posting list to determine section boundary"
                            .to_string(),
                    )
                })?;
                prev += delta;
            }
            let _ = prev; // suppress unused warning

            data_start + cursor.position() as usize
        };

        Ok(TrigramIndexReader {
            mmap,
            trigram_count,
            table_offset,
            file_postings_offset,
            pos_postings_offset,
        })
    }

    /// Look up a single trigram. Returns the file_ids that contain it.
    ///
    /// Uses binary search on the sorted trigram table for O(log n) lookup,
    /// then decodes the delta-varint-encoded file ID list on demand.
    ///
    /// Returns an empty vector if the trigram is not found in the index.
    pub fn lookup_file_ids(&self, trigram: Trigram) -> Result<Vec<FileId>, IndexError> {
        let idx = match self.binary_search_trigram(trigram) {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };

        let entry_start = self.table_offset + idx * TABLE_ENTRY_SIZE;
        let file_list_offset = u32::from_le_bytes(
            self.mmap[entry_start + 3..entry_start + 7].try_into().unwrap(),
        ) as usize;
        let file_list_len = u32::from_le_bytes(
            self.mmap[entry_start + 7..entry_start + 11].try_into().unwrap(),
        );

        if file_list_len == 0 {
            return Ok(Vec::new());
        }

        // Determine the byte extent of this entry's file posting data.
        // We know the start offset; the end is the start of the next entry's
        // data (or the end of the file postings section for the last entry).
        let data_start = self.file_postings_offset + file_list_offset;
        let data_end = if idx + 1 < self.trigram_count as usize {
            let next_entry_start = self.table_offset + (idx + 1) * TABLE_ENTRY_SIZE;
            let next_offset = u32::from_le_bytes(
                self.mmap[next_entry_start + 3..next_entry_start + 7]
                    .try_into()
                    .unwrap(),
            ) as usize;
            self.file_postings_offset + next_offset
        } else {
            self.pos_postings_offset
        };

        if data_start > self.mmap.len() || data_end > self.mmap.len() {
            return Err(IndexError::IndexCorruption(
                "file posting data out of bounds".to_string(),
            ));
        }

        let encoded = &self.mmap[data_start..data_end];
        let raw_ids = decode_delta_varint(encoded);
        Ok(raw_ids.into_iter().map(FileId).collect())
    }

    /// Look up a single trigram. Returns (file_id, offset) pairs.
    ///
    /// Uses binary search on the sorted trigram table, then decodes the
    /// positional posting list on demand.
    ///
    /// Returns an empty vector if the trigram is not found in the index.
    pub fn lookup_positions(&self, trigram: Trigram) -> Result<Vec<(FileId, u32)>, IndexError> {
        let idx = match self.binary_search_trigram(trigram) {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };

        let entry_start = self.table_offset + idx * TABLE_ENTRY_SIZE;
        let pos_list_offset = u32::from_le_bytes(
            self.mmap[entry_start + 11..entry_start + 15].try_into().unwrap(),
        ) as usize;
        let pos_list_len = u32::from_le_bytes(
            self.mmap[entry_start + 15..entry_start + 19].try_into().unwrap(),
        );

        if pos_list_len == 0 {
            return Ok(Vec::new());
        }

        // Determine the byte extent of this entry's positional posting data.
        let data_start = self.pos_postings_offset + pos_list_offset;
        let data_end = if idx + 1 < self.trigram_count as usize {
            let next_entry_start = self.table_offset + (idx + 1) * TABLE_ENTRY_SIZE;
            let next_offset = u32::from_le_bytes(
                self.mmap[next_entry_start + 11..next_entry_start + 15]
                    .try_into()
                    .unwrap(),
            ) as usize;
            self.pos_postings_offset + next_offset
        } else {
            self.mmap.len()
        };

        if data_start > self.mmap.len() || data_end > self.mmap.len() {
            return Err(IndexError::IndexCorruption(
                "positional posting data out of bounds".to_string(),
            ));
        }

        let encoded = &self.mmap[data_start..data_end];
        let raw_positions = decode_positional_postings(encoded);
        Ok(raw_positions
            .into_iter()
            .map(|(fid, off)| (FileId(fid), off))
            .collect())
    }

    /// Number of distinct trigrams in the index.
    pub fn trigram_count(&self) -> u32 {
        self.trigram_count
    }

    /// Binary search the sorted trigram table for the given trigram.
    ///
    /// Returns the index into the trigram table if found, or `None` if not present.
    fn binary_search_trigram(&self, trigram: Trigram) -> Option<usize> {
        let target = trigram.to_u32();
        let count = self.trigram_count as usize;

        if count == 0 {
            return None;
        }

        let mut lo = 0usize;
        let mut hi = count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry_start = self.table_offset + mid * TABLE_ENTRY_SIZE;
            let bytes = &self.mmap[entry_start..entry_start + 3];
            let t = Trigram::from_bytes(bytes[0], bytes[1], bytes[2]);
            let val = t.to_u32();

            match val.cmp(&target) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_writer::TrigramIndexWriter;
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

    /// Write Appendix A index and open reader.
    fn write_and_open() -> (tempfile::TempDir, TrigramIndexReader) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        let builder = build_appendix_a();
        TrigramIndexWriter::write(&builder, &path).unwrap();
        let reader = TrigramIndexReader::open(&path).unwrap();
        (dir, reader)
    }

    #[test]
    fn test_open_and_trigram_count() {
        let (_dir, reader) = write_and_open();
        assert_eq!(reader.trigram_count(), 17);
    }

    #[test]
    fn test_lookup_file_ids_shared_trigram() {
        let (_dir, reader) = write_and_open();

        // "fn " appears in both files
        let fids = reader
            .lookup_file_ids(Trigram::from_bytes(b'f', b'n', b' '))
            .unwrap();
        assert_eq!(fids, vec![FileId(0), FileId(1)]);
    }

    #[test]
    fn test_lookup_file_ids_file0_only() {
        let (_dir, reader) = write_and_open();

        // "mai" appears only in file 0
        let fids = reader
            .lookup_file_ids(Trigram::from_bytes(b'm', b'a', b'i'))
            .unwrap();
        assert_eq!(fids, vec![FileId(0)]);
    }

    #[test]
    fn test_lookup_file_ids_file1_only() {
        let (_dir, reader) = write_and_open();

        // "par" appears only in file 1
        let fids = reader
            .lookup_file_ids(Trigram::from_bytes(b'p', b'a', b'r'))
            .unwrap();
        assert_eq!(fids, vec![FileId(1)]);
    }

    #[test]
    fn test_lookup_file_ids_absent_trigram() {
        let (_dir, reader) = write_and_open();

        // "xyz" does not appear in either file
        let fids = reader
            .lookup_file_ids(Trigram::from_bytes(b'x', b'y', b'z'))
            .unwrap();
        assert!(fids.is_empty());
    }

    #[test]
    fn test_lookup_positions_shared_trigram() {
        let (_dir, reader) = write_and_open();

        // "fn " -> [(FileId(0), 0), (FileId(1), 0)]
        let positions = reader
            .lookup_positions(Trigram::from_bytes(b'f', b'n', b' '))
            .unwrap();
        assert_eq!(
            positions,
            vec![(FileId(0), 0), (FileId(1), 0)]
        );
    }

    #[test]
    fn test_lookup_positions_file0_only() {
        let (_dir, reader) = write_and_open();

        // "mai" -> [(FileId(0), 3)]
        let positions = reader
            .lookup_positions(Trigram::from_bytes(b'm', b'a', b'i'))
            .unwrap();
        assert_eq!(positions, vec![(FileId(0), 3)]);
    }

    #[test]
    fn test_lookup_positions_absent_trigram() {
        let (_dir, reader) = write_and_open();

        let positions = reader
            .lookup_positions(Trigram::from_bytes(b'x', b'y', b'z'))
            .unwrap();
        assert!(positions.is_empty());
    }

    #[test]
    fn test_roundtrip_all_file_postings() {
        let builder = build_appendix_a();
        let (_dir, reader) = write_and_open();

        // Verify every trigram's file posting list matches the builder
        for (trigram, expected_fids) in builder.file_postings() {
            let actual_fids = reader.lookup_file_ids(*trigram).unwrap();
            assert_eq!(
                actual_fids, *expected_fids,
                "file posting mismatch for trigram {trigram}"
            );
        }
    }

    #[test]
    fn test_roundtrip_all_positional_postings() {
        let builder = build_appendix_a();
        let (_dir, reader) = write_and_open();

        // Verify every trigram's positional posting list matches the builder
        for (trigram, expected_positions) in builder.positional_postings() {
            let actual_positions = reader.lookup_positions(*trigram).unwrap();
            let expected: Vec<(FileId, u32)> = expected_positions
                .iter()
                .map(|(fid, off)| (*fid, *off))
                .collect();
            assert_eq!(
                actual_positions, expected,
                "positional posting mismatch for trigram {trigram}"
            );
        }
    }

    #[test]
    fn test_open_invalid_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");

        // Write a file with wrong magic
        let mut data = vec![0u8; 10];
        data[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        data[4..6].copy_from_slice(&TRIG_VERSION.to_le_bytes());
        data[6..10].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        let result = TrigramIndexReader::open(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("invalid trigrams.bin magic"));
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    #[test]
    fn test_open_invalid_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");

        let mut data = vec![0u8; 10];
        data[0..4].copy_from_slice(&TRIG_MAGIC.to_le_bytes());
        data[4..6].copy_from_slice(&99u16.to_le_bytes());
        data[6..10].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        let result = TrigramIndexReader::open(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::UnsupportedVersion { version } => {
                assert_eq!(version, 99);
            }
            other => panic!("expected UnsupportedVersion, got: {other}"),
        }
    }

    #[test]
    fn test_open_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        let builder = PostingListBuilder::new();
        TrigramIndexWriter::write(&builder, &path).unwrap();

        let reader = TrigramIndexReader::open(&path).unwrap();
        assert_eq!(reader.trigram_count(), 0);

        let fids = reader
            .lookup_file_ids(Trigram::from_bytes(b'a', b'b', b'c'))
            .unwrap();
        assert!(fids.is_empty());
    }

    #[test]
    fn test_open_file_too_small() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");

        // Write only 5 bytes (less than header)
        std::fs::write(&path, &[0u8; 5]).unwrap();

        let result = TrigramIndexReader::open(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("too small"));
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }
}
