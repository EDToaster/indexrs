//! Symbol index with trigram-indexed names.
//!
//! Provides binary persistence for symbol definitions extracted by the symbol
//! extractor, with trigram indexing over symbol names for fast lookup.
//!
//! ## Binary Format
//!
//! **symbols.bin** contains a fixed-size header, fixed-size entries, and a name
//! string pool:
//!
//! ```text
//! [Header]  (10 bytes)
//!   magic: u32 = 0x5359_4D53  ("SYMS")
//!   version: u16 = 1
//!   symbol_count: u32
//!
//! [Entries]  (23 bytes each, indexed by symbol_id)
//!   file_id: u32
//!   name_offset: u32
//!   name_len: u32
//!   line: u32
//!   column: u16
//!   kind: u8
//!   parent_symbol: u32    (reserved; always u32::MAX)
//!
//! [Name Pool]
//!   Contiguous UTF-8 strings (no separators; offsets from entries)
//! ```
//!
//! **sym_trigrams.bin** is a standard TRIG-format trigram index over symbol
//! names, reusing the existing posting list and trigram writer/reader
//! infrastructure. Symbol IDs are treated as file IDs for posting list purposes.

use std::fs;
use std::io::Write;
use std::path::Path;

use memmap2::Mmap;

use rayon::prelude::*;

use crate::error::IndexError;
use crate::index_reader::TrigramIndexReader;
use crate::index_state::SegmentList;
use crate::index_writer::TrigramIndexWriter;
use crate::posting::PostingListBuilder;
use crate::trigram::{ascii_fold_byte, extract_unique_trigrams_folded};
use crate::types::{FileId, Language, SegmentId, SymbolKind};

/// Magic number for symbols.bin: "SYMS" as little-endian u32.
const SYMS_MAGIC: u32 = 0x5359_4D53;

/// Current format version for symbols.bin.
const SYMS_VERSION: u16 = 1;

/// Size of the symbols.bin header in bytes: magic(4) + version(2) + symbol_count(4).
const HEADER_SIZE: usize = 10;

/// Size of a single symbol entry in bytes:
/// file_id(4) + name_offset(4) + name_len(4) + line(4) + column(2) + kind(1) + parent_symbol(4).
const ENTRY_SIZE: usize = 23;

/// A symbol record for writing to the index.
#[derive(Debug, Clone)]
pub struct SymbolRecord {
    /// The file this symbol belongs to.
    pub file_id: FileId,
    /// The symbol name.
    pub name: String,
    /// The kind of symbol.
    pub kind: SymbolKind,
    /// 0-based line number.
    pub line: u32,
    /// 0-based column offset.
    pub column: u16,
}

/// A symbol entry read back from the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolHit {
    /// The symbol's sequential ID within the index.
    pub symbol_id: u32,
    /// The file this symbol belongs to.
    pub file_id: FileId,
    /// The symbol name.
    pub name: String,
    /// The kind of symbol.
    pub kind: SymbolKind,
    /// 0-based line number.
    pub line: u32,
    /// 0-based column offset.
    pub column: u16,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Writer for the symbol index binary format.
///
/// Writes two files:
/// 1. `symbols.bin` — header + entries + name string pool
/// 2. `sym_trigrams.bin` — TRIG-format trigram index over symbol names
pub struct SymbolIndexWriter;

impl SymbolIndexWriter {
    /// Write symbol records to the given directory.
    ///
    /// Creates `symbols.bin` and `sym_trigrams.bin` in `dir` using atomic
    /// temp-file-then-rename for crash safety.
    pub fn write(records: &[SymbolRecord], dir: &Path) -> Result<(), IndexError> {
        let symbol_count: u32 = records.len().try_into().map_err(|_| {
            IndexError::IndexCorruption("symbol count exceeds u32::MAX".to_string())
        })?;

        // Build the name pool and collect entry data
        let total_name_bytes: usize = records.iter().map(|r| r.name.len()).sum();
        let mut name_pool = Vec::with_capacity(total_name_bytes);
        let mut entries: Vec<(u32, u32, u32, u32, u16, u8, u32)> =
            Vec::with_capacity(records.len());

        for record in records {
            let name_offset: u32 = name_pool.len().try_into().map_err(|_| {
                IndexError::IndexCorruption("name pool offset exceeds u32::MAX".to_string())
            })?;
            let name_len: u32 = record.name.len().try_into().map_err(|_| {
                IndexError::IndexCorruption("symbol name length exceeds u32::MAX".to_string())
            })?;
            name_pool.extend_from_slice(record.name.as_bytes());
            entries.push((
                record.file_id.0,
                name_offset,
                name_len,
                record.line,
                record.column,
                record.kind.to_u8(),
                u32::MAX, // parent_symbol reserved
            ));
        }

        // Write symbols.bin via temp file
        let symbols_path = dir.join("symbols.bin");
        let temp_symbols = dir.join(format!(".symbols.bin.tmp.{}", std::process::id()));
        {
            let entries_size = records.len() * ENTRY_SIZE;
            let total_size = HEADER_SIZE + entries_size + name_pool.len();
            let mut buf = Vec::with_capacity(total_size);

            // Header
            buf.write_all(&SYMS_MAGIC.to_le_bytes())?;
            buf.write_all(&SYMS_VERSION.to_le_bytes())?;
            buf.write_all(&symbol_count.to_le_bytes())?;

            // Entries
            for &(file_id, name_offset, name_len, line, column, kind, parent) in &entries {
                buf.write_all(&file_id.to_le_bytes())?;
                buf.write_all(&name_offset.to_le_bytes())?;
                buf.write_all(&name_len.to_le_bytes())?;
                buf.write_all(&line.to_le_bytes())?;
                buf.write_all(&column.to_le_bytes())?;
                buf.write_all(&[kind])?;
                buf.write_all(&parent.to_le_bytes())?;
            }

            // Name pool
            buf.write_all(&name_pool)?;

            let mut f = fs::File::create(&temp_symbols)?;
            std::io::Write::write_all(&mut f, &buf)?;
            f.sync_all()?;
        }
        fs::rename(&temp_symbols, &symbols_path)?;

        // Build trigram index over symbol names
        let mut posting_builder = PostingListBuilder::file_only();
        for (symbol_id, record) in records.iter().enumerate() {
            posting_builder.add_file(FileId(symbol_id as u32), record.name.as_bytes());
        }
        posting_builder.finalize();

        let sym_trigrams_path = dir.join("sym_trigrams.bin");
        TrigramIndexWriter::write(&posting_builder, &sym_trigrams_path)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Memory-mapped reader for the symbol index.
///
/// Opens `symbols.bin` (mmap) and `sym_trigrams.bin` (via `TrigramIndexReader`),
/// providing O(1) entry lookup by symbol ID and trigram-accelerated name search.
pub struct SymbolIndexReader {
    mmap: Mmap,
    symbol_count: u32,
    trigram_reader: TrigramIndexReader,
    /// Byte offset where the name pool starts in the mmap.
    name_pool_offset: usize,
}

impl std::fmt::Debug for SymbolIndexReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymbolIndexReader")
            .field("symbol_count", &self.symbol_count)
            .finish_non_exhaustive()
    }
}

impl SymbolIndexReader {
    /// Open a symbol index from a segment directory.
    ///
    /// Loads `symbols.bin` via mmap and `sym_trigrams.bin` via `TrigramIndexReader`.
    /// Validates the magic number and format version.
    pub fn open(dir: &Path) -> Result<Self, IndexError> {
        let symbols_path = dir.join("symbols.bin");
        let file = fs::File::open(&symbols_path)?;
        // SAFETY: We treat the mmap as read-only. The file is immutable once written.
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < HEADER_SIZE {
            return Err(IndexError::IndexCorruption(
                "symbols.bin too small for header".to_string(),
            ));
        }

        let magic = u32::from_le_bytes(mmap[0..4].try_into().unwrap());
        if magic != SYMS_MAGIC {
            return Err(IndexError::IndexCorruption(format!(
                "invalid symbols.bin magic: expected 0x{SYMS_MAGIC:08X}, got 0x{magic:08X}"
            )));
        }

        let version = u16::from_le_bytes(mmap[4..6].try_into().unwrap());
        if version != SYMS_VERSION {
            return Err(IndexError::UnsupportedVersion {
                version: version as u32,
            });
        }

        let symbol_count = u32::from_le_bytes(mmap[6..10].try_into().unwrap());
        let name_pool_offset = HEADER_SIZE + symbol_count as usize * ENTRY_SIZE;

        if mmap.len() < name_pool_offset {
            return Err(IndexError::IndexCorruption(format!(
                "symbols.bin too small: expected at least {} bytes for {} symbols, got {}",
                name_pool_offset,
                symbol_count,
                mmap.len()
            )));
        }

        // sym_trigrams.bin is always written by SymbolIndexWriter alongside symbols.bin;
        // the outer SegmentWriter wraps both in a single atomic temp-dir rename.
        let trigram_reader = TrigramIndexReader::open(&dir.join("sym_trigrams.bin"))?;

        Ok(SymbolIndexReader {
            mmap,
            symbol_count,
            trigram_reader,
            name_pool_offset,
        })
    }

    /// Number of symbols in this index.
    pub fn symbol_count(&self) -> u32 {
        self.symbol_count
    }

    /// Read a symbol entry by its sequential ID.
    pub fn get(&self, symbol_id: u32) -> Result<SymbolHit, IndexError> {
        if symbol_id >= self.symbol_count {
            return Err(IndexError::IndexCorruption(format!(
                "symbol_id {} out of range (max {})",
                symbol_id, self.symbol_count
            )));
        }

        let entry_offset = HEADER_SIZE + symbol_id as usize * ENTRY_SIZE;
        let entry = &self.mmap[entry_offset..entry_offset + ENTRY_SIZE];

        let file_id = u32::from_le_bytes(entry[0..4].try_into().unwrap());
        let name_offset = u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize;
        let name_len = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let line = u32::from_le_bytes(entry[12..16].try_into().unwrap());
        let column = u16::from_le_bytes(entry[16..18].try_into().unwrap());
        let kind_byte = entry[18];
        // entry[19..23] is parent_symbol (reserved)

        let kind = SymbolKind::from_u8(kind_byte).ok_or_else(|| {
            IndexError::IndexCorruption(format!("invalid SymbolKind byte: {kind_byte}"))
        })?;

        let pool_start = self.name_pool_offset + name_offset;
        let pool_end = pool_start + name_len;
        if pool_end > self.mmap.len() {
            return Err(IndexError::IndexCorruption(
                "symbol name extends beyond mmap".to_string(),
            ));
        }

        let name = std::str::from_utf8(&self.mmap[pool_start..pool_end])
            .map_err(|e| IndexError::IndexCorruption(format!("invalid UTF-8 in symbol name: {e}")))?
            .to_string();

        Ok(SymbolHit {
            symbol_id,
            file_id: FileId(file_id),
            name,
            kind,
            line,
            column,
        })
    }

    /// Search for symbols by name substring using trigram lookup.
    ///
    /// For queries shorter than 3 characters, falls back to a linear scan
    /// of all symbols.
    pub fn search(&self, query: &str) -> Result<Vec<SymbolHit>, IndexError> {
        self.search_filtered(query, None, None)
    }

    /// Search for symbols by name substring with optional kind and file_id filters.
    ///
    /// For queries shorter than 3 characters, falls back to a linear scan
    /// of all symbols. Trigram lookup uses ASCII-folded case-insensitive matching.
    pub fn search_filtered(
        &self,
        query: &str,
        kind: Option<SymbolKind>,
        file_id: Option<FileId>,
    ) -> Result<Vec<SymbolHit>, IndexError> {
        let candidate_ids = if query.len() < 3 {
            // Linear scan fallback for short queries
            (0..self.symbol_count).collect::<Vec<_>>()
        } else {
            // Trigram-accelerated lookup
            let trigrams = extract_unique_trigrams_folded(query.as_bytes());
            if trigrams.is_empty() {
                return Ok(Vec::new());
            }

            // Look up posting lists and intersect (smallest first)
            let mut posting_lists: Vec<Vec<FileId>> = Vec::with_capacity(trigrams.len());
            for trigram in &trigrams {
                let fids = self.trigram_reader.lookup_file_ids(*trigram)?;
                if fids.is_empty() {
                    return Ok(Vec::new());
                }
                posting_lists.push(fids);
            }
            posting_lists.sort_by_key(|l| l.len());

            let mut result = posting_lists[0].clone();
            for list in &posting_lists[1..] {
                result = intersect_two_sorted(&result, list);
                if result.is_empty() {
                    return Ok(Vec::new());
                }
            }

            result.into_iter().map(|fid| fid.0).collect()
        };

        // Fold query for case-insensitive verification
        let folded_query: Vec<u8> = query.bytes().map(ascii_fold_byte).collect();

        let mut hits = Vec::new();
        for symbol_id in candidate_ids {
            let hit = self.get(symbol_id)?;

            // Verify name contains query (case-insensitive)
            if !query.is_empty() {
                let folded_name: Vec<u8> = hit.name.bytes().map(ascii_fold_byte).collect();
                if memchr::memmem::find(&folded_name, &folded_query).is_none() {
                    continue;
                }
            }

            // Apply optional filters
            if let Some(ref k) = kind
                && hit.kind != *k
            {
                continue;
            }
            if let Some(ref fid) = file_id
                && hit.file_id != *fid
            {
                continue;
            }

            hits.push(hit);
        }

        Ok(hits)
    }
}

/// Two-pointer sorted merge intersection of two sorted `FileId` slices.
fn intersect_two_sorted(a: &[FileId], b: &[FileId]) -> Vec<FileId> {
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Multi-segment symbol search
// ---------------------------------------------------------------------------

/// Options for multi-segment symbol search.
#[derive(Debug, Clone)]
pub struct SymbolSearchOptions {
    /// Filter by symbol kind.
    pub kind: Option<SymbolKind>,
    /// Filter by file language.
    pub language: Option<Language>,
    /// Filter by path substring (matches if path contains this string).
    pub path_filter: Option<String>,
    /// Maximum number of results to return.
    pub max_results: usize,
    /// Number of results to skip (for pagination).
    pub offset: usize,
}

impl Default for SymbolSearchOptions {
    fn default() -> Self {
        SymbolSearchOptions {
            kind: None,
            language: None,
            path_filter: None,
            max_results: 100,
            offset: 0,
        }
    }
}

/// A symbol match from a multi-segment search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SymbolMatch {
    /// The symbol name.
    pub name: String,
    /// The kind of symbol.
    pub kind: SymbolKind,
    /// The file path containing this symbol.
    pub path: String,
    /// 0-based line number.
    pub line: u32,
    /// 0-based column offset.
    pub column: u16,
    /// The file ID within the segment.
    pub file_id: FileId,
    /// The segment this symbol was found in.
    pub segment_id: SegmentId,
    /// Relevance score: exact=1.0, prefix=0.8, substring=0.5.
    pub score: f64,
}

/// Search for symbols across multiple segments.
///
/// Follows the same pattern as `multi_search::search_segments`:
/// 1. Iterate segments from newest to oldest
/// 2. For each segment: load symbol reader, load tombstones, search
/// 3. Skip tombstoned file IDs
/// 4. Get file metadata for path/language filtering
/// 5. Score matches (exact=1.0, prefix=0.8, substring=0.5)
/// 6. Dedup by (path, name, line) -- newest segment wins
/// 7. Sort by score descending
/// 8. Paginate with offset/max_results
pub fn search_symbols(
    snapshot: &SegmentList,
    query: &str,
    options: &SymbolSearchOptions,
) -> Result<Vec<SymbolMatch>, IndexError> {
    let folded_query: Vec<u8> = query.bytes().map(ascii_fold_byte).collect();

    // Sort segments by ID descending (newest first) for dedup ordering
    let mut segments_by_id: Vec<_> = snapshot.iter().collect();
    segments_by_id.sort_by(|a, b| b.segment_id().0.cmp(&a.segment_id().0));

    // Phase 1: Search each segment in parallel
    let per_segment_results: Vec<Result<Vec<SymbolMatch>, IndexError>> = segments_by_id
        .par_iter()
        .map(|segment| {
            let symbol_reader = match segment.symbol_reader() {
                Some(r) => r,
                None => return Ok(Vec::new()),
            };

            let tombstones = segment.load_tombstones()?;
            let hits = symbol_reader.search_filtered(query, options.kind, None)?;

            let mut matches = Vec::new();
            for hit in hits {
                if tombstones.contains(hit.file_id) {
                    continue;
                }

                let meta = match segment.get_metadata(hit.file_id)? {
                    Some(m) => m,
                    None => continue,
                };

                if let Some(ref lang) = options.language
                    && meta.language != *lang
                {
                    continue;
                }

                if let Some(ref pattern) = options.path_filter
                    && !meta.path.contains(pattern.as_str())
                {
                    continue;
                }

                let folded_name: Vec<u8> = hit.name.bytes().map(ascii_fold_byte).collect();
                let score = if folded_name == folded_query {
                    1.0
                } else if folded_name.starts_with(&folded_query) {
                    0.8
                } else {
                    0.5
                };

                matches.push(SymbolMatch {
                    name: hit.name,
                    kind: hit.kind,
                    path: meta.path,
                    line: hit.line,
                    column: hit.column,
                    file_id: hit.file_id,
                    segment_id: segment.segment_id(),
                    score,
                });
            }
            Ok(matches)
        })
        .collect();

    // Phase 2: Sequential merge with dedup.
    // IMPORTANT: per_segment_results is in newest-first order (from the sort above),
    // and rayon's collect() preserves source order. The HashSet first-insert-wins
    // semantics therefore give "newest segment wins" dedup.
    let mut all_matches: Vec<SymbolMatch> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, u32)> =
        std::collections::HashSet::new();

    for result in per_segment_results {
        let matches = result?;
        for m in matches {
            let dedup_key = (m.path.clone(), m.name.clone(), m.line);
            if seen.insert(dedup_key) {
                all_matches.push(m);
            }
        }
    }

    // Sort by score descending, then by name for stability
    all_matches.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });

    // Paginate
    let start = options.offset.min(all_matches.len());
    let end = (start + options.max_results).min(all_matches.len());
    Ok(all_matches[start..end].to_vec())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{InputFile, SegmentWriter};
    use crate::types::SegmentId;
    use std::sync::Arc;

    /// Helper: write symbols and open reader
    fn write_and_open(records: &[SymbolRecord]) -> (tempfile::TempDir, SymbolIndexReader) {
        let dir = tempfile::tempdir().unwrap();
        SymbolIndexWriter::write(records, dir.path()).unwrap();
        let reader = SymbolIndexReader::open(dir.path()).unwrap();
        (dir, reader)
    }

    // -----------------------------------------------------------------------
    // Test 1: Round-trip test
    // -----------------------------------------------------------------------

    #[test]
    fn test_symbol_index_roundtrip() {
        let records = vec![
            SymbolRecord {
                file_id: FileId(0),
                name: "process".to_string(),
                kind: SymbolKind::Function,
                line: 10,
                column: 4,
            },
            SymbolRecord {
                file_id: FileId(1),
                name: "Config".to_string(),
                kind: SymbolKind::Struct,
                line: 20,
                column: 0,
            },
            SymbolRecord {
                file_id: FileId(0),
                name: "process_data".to_string(),
                kind: SymbolKind::Function,
                line: 30,
                column: 4,
            },
        ];

        let (_dir, reader) = write_and_open(&records);

        assert_eq!(reader.symbol_count(), 3);

        let hit0 = reader.get(0).unwrap();
        assert_eq!(hit0.symbol_id, 0);
        assert_eq!(hit0.file_id, FileId(0));
        assert_eq!(hit0.name, "process");
        assert_eq!(hit0.kind, SymbolKind::Function);
        assert_eq!(hit0.line, 10);
        assert_eq!(hit0.column, 4);

        let hit1 = reader.get(1).unwrap();
        assert_eq!(hit1.name, "Config");
        assert_eq!(hit1.kind, SymbolKind::Struct);
        assert_eq!(hit1.file_id, FileId(1));
        assert_eq!(hit1.line, 20);
        assert_eq!(hit1.column, 0);

        let hit2 = reader.get(2).unwrap();
        assert_eq!(hit2.name, "process_data");
        assert_eq!(hit2.kind, SymbolKind::Function);
        assert_eq!(hit2.line, 30);
    }

    // -----------------------------------------------------------------------
    // Test 2: Trigram lookup
    // -----------------------------------------------------------------------

    #[test]
    fn test_symbol_index_trigram_lookup() {
        let records = vec![
            SymbolRecord {
                file_id: FileId(0),
                name: "process".to_string(),
                kind: SymbolKind::Function,
                line: 0,
                column: 0,
            },
            SymbolRecord {
                file_id: FileId(1),
                name: "Config".to_string(),
                kind: SymbolKind::Struct,
                line: 0,
                column: 0,
            },
            SymbolRecord {
                file_id: FileId(0),
                name: "process_data".to_string(),
                kind: SymbolKind::Function,
                line: 0,
                column: 0,
            },
        ];

        let (_dir, reader) = write_and_open(&records);

        let hits = reader.search("process").unwrap();
        assert_eq!(hits.len(), 2);
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"process"));
        assert!(names.contains(&"process_data"));
    }

    // -----------------------------------------------------------------------
    // Test 3: Case-insensitive search
    // -----------------------------------------------------------------------

    #[test]
    fn test_symbol_index_case_insensitive() {
        let records = vec![SymbolRecord {
            file_id: FileId(0),
            name: "Config".to_string(),
            kind: SymbolKind::Struct,
            line: 0,
            column: 0,
        }];

        let (_dir, reader) = write_and_open(&records);

        let hits = reader.search("config").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Config");

        let hits_upper = reader.search("CONFIG").unwrap();
        assert_eq!(hits_upper.len(), 1);
        assert_eq!(hits_upper[0].name, "Config");
    }

    // -----------------------------------------------------------------------
    // Test 4: Empty index
    // -----------------------------------------------------------------------

    #[test]
    fn test_symbol_index_empty() {
        let (_dir, reader) = write_and_open(&[]);

        assert_eq!(reader.symbol_count(), 0);
        let hits = reader.search("anything").unwrap();
        assert!(hits.is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 5: Short query fallback to linear scan
    // -----------------------------------------------------------------------

    #[test]
    fn test_symbol_index_short_query() {
        let records = vec![
            SymbolRecord {
                file_id: FileId(0),
                name: "fn".to_string(),
                kind: SymbolKind::Function,
                line: 0,
                column: 0,
            },
            SymbolRecord {
                file_id: FileId(1),
                name: "Config".to_string(),
                kind: SymbolKind::Struct,
                line: 0,
                column: 0,
            },
        ];

        let (_dir, reader) = write_and_open(&records);

        // Query "fn" is < 3 chars, should fall back to linear scan
        let hits = reader.search("fn").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "fn");

        // Single char query
        let hits_c = reader.search("C").unwrap();
        assert_eq!(hits_c.len(), 1);
        assert_eq!(hits_c[0].name, "Config");
    }

    // -----------------------------------------------------------------------
    // Test 6: Kind filter
    // -----------------------------------------------------------------------

    #[test]
    fn test_symbol_index_kind_filter() {
        let records = vec![
            SymbolRecord {
                file_id: FileId(0),
                name: "process".to_string(),
                kind: SymbolKind::Function,
                line: 0,
                column: 0,
            },
            SymbolRecord {
                file_id: FileId(1),
                name: "Process".to_string(),
                kind: SymbolKind::Struct,
                line: 0,
                column: 0,
            },
        ];

        let (_dir, reader) = write_and_open(&records);

        // Search for "process" filtered to Function only
        let hits = reader
            .search_filtered("process", Some(SymbolKind::Function), None)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "process");
        assert_eq!(hits[0].kind, SymbolKind::Function);

        // Search for "process" filtered to Struct only
        let hits_struct = reader
            .search_filtered("process", Some(SymbolKind::Struct), None)
            .unwrap();
        assert_eq!(hits_struct.len(), 1);
        assert_eq!(hits_struct[0].name, "Process");
        assert_eq!(hits_struct[0].kind, SymbolKind::Struct);
    }

    // -----------------------------------------------------------------------
    // Test 7: Segment integration test
    // -----------------------------------------------------------------------

    #[test]
    fn test_symbol_index_segment_integration() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let rust_code = br#"
fn process() {}
fn process_data() {}
struct Config {
    value: i32,
}
"#;

        let files = vec![InputFile {
            path: "src/main.rs".to_string(),
            content: rust_code.to_vec(),
            mtime: 1700000000,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        let seg_dir = segment.dir_path();
        assert!(seg_dir.join("symbols.bin").exists());
        assert!(seg_dir.join("sym_trigrams.bin").exists());

        // Verify we can search symbols via the segment
        let sym_reader = segment.symbol_reader().expect("symbol reader should exist");
        assert!(sym_reader.symbol_count() > 0);

        let hits = sym_reader.search("process").unwrap();
        assert!(
            hits.len() >= 2,
            "should find at least process and process_data"
        );
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert!(names.contains(&"process"));
        assert!(names.contains(&"process_data"));

        // Config should also be found
        let config_hits = sym_reader.search("Config").unwrap();
        assert!(!config_hits.is_empty(), "should find Config struct");
    }

    // -----------------------------------------------------------------------
    // Additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_symbol_index_file_id_filter() {
        let records = vec![
            SymbolRecord {
                file_id: FileId(0),
                name: "alpha".to_string(),
                kind: SymbolKind::Function,
                line: 0,
                column: 0,
            },
            SymbolRecord {
                file_id: FileId(1),
                name: "beta_alpha".to_string(),
                kind: SymbolKind::Function,
                line: 0,
                column: 0,
            },
        ];

        let (_dir, reader) = write_and_open(&records);

        let hits = reader
            .search_filtered("alpha", None, Some(FileId(0)))
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "alpha");
    }

    #[test]
    fn test_symbol_index_out_of_range() {
        let records = vec![SymbolRecord {
            file_id: FileId(0),
            name: "test".to_string(),
            kind: SymbolKind::Function,
            line: 0,
            column: 0,
        }];

        let (_dir, reader) = write_and_open(&records);
        let result = reader.get(99);
        assert!(result.is_err());
    }

    #[test]
    fn test_symbol_index_binary_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![SymbolRecord {
            file_id: FileId(0),
            name: "test_symbol".to_string(),
            kind: SymbolKind::Function,
            line: 0,
            column: 0,
        }];

        SymbolIndexWriter::write(&records, dir.path()).unwrap();

        assert!(dir.path().join("symbols.bin").exists());
        assert!(dir.path().join("sym_trigrams.bin").exists());
    }

    #[test]
    fn test_symbol_index_all_kinds() {
        let kinds = vec![
            SymbolKind::Function,
            SymbolKind::Struct,
            SymbolKind::Trait,
            SymbolKind::Enum,
            SymbolKind::Interface,
            SymbolKind::Class,
            SymbolKind::Method,
            SymbolKind::Constant,
            SymbolKind::Variable,
            SymbolKind::Type,
            SymbolKind::Module,
        ];

        let records: Vec<SymbolRecord> = kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| SymbolRecord {
                file_id: FileId(0),
                name: format!("symbol_{}", i),
                kind: *kind,
                line: i as u32,
                column: 0,
            })
            .collect();

        let (_dir, reader) = write_and_open(&records);
        assert_eq!(reader.symbol_count(), kinds.len() as u32);

        for (i, kind) in kinds.iter().enumerate() {
            let hit = reader.get(i as u32).unwrap();
            assert_eq!(hit.kind, *kind);
        }
    }

    #[test]
    fn test_symbol_search_multi_segment() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files0 = vec![InputFile {
            path: "src/main.rs".to_string(),
            content: b"fn process() {}\nfn helper() {}\n".to_vec(),
            mtime: 100,
        }];

        let files1 = vec![InputFile {
            path: "src/lib.rs".to_string(),
            content: b"fn process_data() {}\nstruct Config {}\n".to_vec(),
            mtime: 200,
        }];

        let seg0 = Arc::new(
            SegmentWriter::new(&base_dir, SegmentId(0))
                .build(files0)
                .unwrap(),
        );
        let seg1 = Arc::new(
            SegmentWriter::new(&base_dir, SegmentId(1))
                .build(files1)
                .unwrap(),
        );

        let snapshot: crate::index_state::SegmentList = Arc::new(vec![seg0, seg1]);

        let options = SymbolSearchOptions {
            kind: None,
            language: None,
            path_filter: None,
            max_results: 100,
            offset: 0,
        };

        let results = search_symbols(&snapshot, "process", &options).unwrap();
        assert!(results.len() >= 2);
        let names: Vec<&str> = results.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"process"));
        assert!(names.contains(&"process_data"));
    }

    #[test]
    fn test_search_symbols_multi_segment_parallel_dedup() {
        use crate::segment::{InputFile, SegmentWriter};
        use crate::types::SegmentId;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Segment 0 (older): has func_a, func_b
        let files_0 = vec![InputFile {
            path: "src/a.rs".to_string(),
            content: b"fn func_a() {}\nfn func_b() {}\n".to_vec(),
            mtime: 1700000000,
        }];
        let seg0 = SegmentWriter::new(&base_dir, SegmentId(0))
            .build(files_0)
            .unwrap();

        // Segment 1 (newer): has func_a (updated), func_c
        let files_1 = vec![InputFile {
            path: "src/a.rs".to_string(),
            content: b"fn func_a() { /* v2 */ }\nfn func_c() {}\n".to_vec(),
            mtime: 1700000100,
        }];
        let seg1 = SegmentWriter::new(&base_dir, SegmentId(1))
            .build(files_1)
            .unwrap();

        let snapshot: crate::index_state::SegmentList =
            Arc::new(vec![Arc::new(seg0), Arc::new(seg1)]);

        let options = SymbolSearchOptions {
            max_results: 100,
            ..Default::default()
        };

        let results = search_symbols(&snapshot, "func", &options).unwrap();

        // func_a should come from segment 1 (newest wins dedup)
        let func_a: Vec<_> = results.iter().filter(|m| m.name == "func_a").collect();
        assert_eq!(
            func_a.len(),
            1,
            "func_a should appear exactly once (deduped)"
        );
        assert_eq!(
            func_a[0].segment_id,
            SegmentId(1),
            "func_a should come from newest segment"
        );

        // func_b from seg 0, func_c from seg 1 — both should appear
        assert!(results.iter().any(|m| m.name == "func_b"));
        assert!(results.iter().any(|m| m.name == "func_c"));
    }

    #[test]
    fn test_symbol_writer_large_batch() {
        let dir = tempfile::tempdir().unwrap();

        let records: Vec<SymbolRecord> = (0..1000)
            .map(|i| SymbolRecord {
                file_id: FileId(i / 10),
                name: format!("symbol_{i}"),
                kind: SymbolKind::Function,
                line: i,
                column: 0,
            })
            .collect();

        SymbolIndexWriter::write(&records, dir.path()).unwrap();

        let reader = SymbolIndexReader::open(dir.path()).unwrap();
        assert_eq!(reader.symbol_count(), 1000);

        let hit = reader.get(0).unwrap();
        assert_eq!(hit.name, "symbol_0");
        let hit = reader.get(999).unwrap();
        assert_eq!(hit.name, "symbol_999");
    }
}
