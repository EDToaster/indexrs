//! Segment storage layout and writer.
//!
//! A segment is an immutable unit of the index, stored as a directory containing:
//! - `trigrams.bin` — trigram posting lists
//! - `meta.bin` — file metadata entries
//! - `paths.bin` — path string pool
//! - `content.zst` — zstd-compressed file contents
//! - `tombstones.bin` — bitmap of deleted file_ids (empty initially)
//!
//! Segments live under `.ferret_index/segments/seg_NNNN/` where NNNN is the segment ID
//! zero-padded to 4 digits.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use memmap2::Mmap;
use rayon::prelude::*;

use crate::content::{ContentStoreReader, ContentStoreWriter};
use crate::error::IndexError;
use crate::index_reader::TrigramIndexReader;
use crate::index_writer::TrigramIndexWriter;
use crate::metadata::{FileMetadata, MetadataBuilder, MetadataReader};
use crate::posting::PostingListBuilder;
use crate::tombstone::TombstoneSet;
use crate::types::{FileId, Language, SegmentId};

/// An input file to be indexed into a segment.
///
/// Callers provide path, raw content, and modification time. The segment writer
/// handles hashing, language detection, line counting, and trigram extraction.
#[derive(Debug, Clone)]
pub struct InputFile {
    /// Relative path from the repository root (e.g. "src/main.rs").
    pub path: String,
    /// Raw file content bytes.
    pub content: Vec<u8>,
    /// Last modification time as seconds since the Unix epoch.
    pub mtime: u64,
}

/// A pre-processed file for compaction, carrying both raw content (for trigram
/// extraction) and pre-compressed bytes (to avoid re-compressing).
///
/// Used by [`SegmentWriter::build_from_compact`] to skip the blake3 hash,
/// zstd compress, language detection, and line counting that
/// [`SegmentWriter::build`] performs — all of which are redundant when
/// merging existing segments whose metadata is already known.
#[derive(Debug, Clone)]
pub struct CompactInputFile {
    /// Relative path from the repository root.
    pub path: String,
    /// Raw (decompressed) file content — needed for trigram extraction.
    pub raw_content: Vec<u8>,
    /// Pre-compressed zstd bytes from the source segment's content store.
    pub compressed: Vec<u8>,
    /// Blake3 content hash (truncated to 16 bytes) from source metadata.
    pub content_hash: [u8; 16],
    /// Detected language from source metadata.
    pub language: Language,
    /// Original uncompressed size from source metadata.
    pub size_bytes: u32,
    /// Line count from source metadata.
    pub line_count: u32,
    /// Last modification time as seconds since the Unix epoch.
    pub mtime: u64,
}

/// A loaded, immutable index segment.
///
/// Contains all readers needed to query the segment's trigram index, file
/// metadata, and compressed content. Segments are opened from disk via
/// [`Segment::open`] or returned from [`SegmentWriter::build`].
///
/// # Lifetime of Metadata Access
///
/// The `MetadataReader` borrows `&[u8]` slices, so `Segment` owns the
/// underlying `Mmap`s and creates ephemeral `MetadataReader` instances on
/// demand via [`get_metadata`](Self::get_metadata).
pub struct Segment {
    segment_id: SegmentId,
    dir_path: PathBuf,
    trigram_reader: TrigramIndexReader,
    content_reader: ContentStoreReader,
    meta_mmap: Mmap,
    paths_mmap: Mmap,
    entry_count: u32,
    /// Size of each metadata entry (58 for v1, 74 for v2).
    meta_entry_size: usize,
    highlight_reader: Option<crate::highlight::HighlightStoreReader>,
    cached_tombstones: RwLock<Option<TombstoneSet>>,
    #[cfg(feature = "symbols")]
    symbol_reader: Option<crate::symbol_index::SymbolIndexReader>,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("segment_id", &self.segment_id)
            .field("dir_path", &self.dir_path)
            .field("entry_count", &self.entry_count)
            .finish_non_exhaustive()
    }
}

impl Segment {
    /// Open an existing segment from disk.
    ///
    /// Loads and validates all segment files:
    /// - `trigrams.bin` via `TrigramIndexReader::open()`
    /// - `meta.bin` + `paths.bin` via memory mapping
    /// - `content.zst` via `ContentStoreReader::open()`
    ///
    /// # Arguments
    ///
    /// * `dir_path` - Path to the segment directory (e.g. `.ferret_index/segments/seg_0000/`).
    /// * `segment_id` - The segment's ID.
    ///
    /// # Errors
    ///
    /// Returns `IndexError::Io` if any file cannot be opened, or
    /// `IndexError::IndexCorruption` if validation fails.
    pub fn open(dir_path: &Path, segment_id: SegmentId) -> Result<Self, IndexError> {
        let trigram_reader = TrigramIndexReader::open(&dir_path.join("trigrams.bin"))?;

        let content_reader =
            ContentStoreReader::open(&dir_path.join("content.zst")).map_err(IndexError::Io)?;

        // Memory-map meta.bin and paths.bin
        let meta_file = fs::File::open(dir_path.join("meta.bin"))?;
        // SAFETY: We treat the mmap as read-only. The file must not be modified
        // externally while mapped; this invariant is maintained by the segment
        // lifecycle (segments are immutable once written).
        let meta_mmap = unsafe { Mmap::map(&meta_file)? };

        let paths_file = fs::File::open(dir_path.join("paths.bin"))?;
        // SAFETY: Same invariant as above.
        let paths_mmap = unsafe { Mmap::map(&paths_file)? };

        // Validate the metadata header and extract entry count + entry size
        let reader = MetadataReader::new(&meta_mmap, &paths_mmap)?;
        let entry_count = reader.entry_count();
        let meta_entry_size = reader.entry_size();

        let highlight_reader = {
            let highlights_path = dir_path.join("highlights.zst");
            if highlights_path.exists() {
                Some(
                    crate::highlight::HighlightStoreReader::open(&highlights_path)
                        .map_err(IndexError::Io)?,
                )
            } else {
                None
            }
        };

        #[cfg(feature = "symbols")]
        let symbol_reader = {
            let symbols_path = dir_path.join("symbols.bin");
            if symbols_path.exists() {
                Some(crate::symbol_index::SymbolIndexReader::open(dir_path)?)
            } else {
                None
            }
        };

        Ok(Segment {
            segment_id,
            dir_path: dir_path.to_path_buf(),
            trigram_reader,
            content_reader,
            meta_mmap,
            paths_mmap,
            entry_count,
            meta_entry_size,
            highlight_reader,
            cached_tombstones: RwLock::new(None),
            #[cfg(feature = "symbols")]
            symbol_reader,
        })
    }

    /// The segment's ID.
    pub fn segment_id(&self) -> SegmentId {
        self.segment_id
    }

    /// Path to the segment directory on disk.
    pub fn dir_path(&self) -> &Path {
        &self.dir_path
    }

    /// Number of file entries in this segment.
    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }

    /// Access the trigram index reader for this segment.
    pub fn trigram_reader(&self) -> &TrigramIndexReader {
        &self.trigram_reader
    }

    /// Access the content store reader for this segment.
    pub fn content_reader(&self) -> &ContentStoreReader {
        &self.content_reader
    }

    /// Access the symbol index reader for this segment, if available.
    ///
    /// Returns `None` if the segment was built without symbol extraction
    /// (i.e., the `symbols` feature was not enabled during build).
    #[cfg(feature = "symbols")]
    pub fn symbol_reader(&self) -> Option<&crate::symbol_index::SymbolIndexReader> {
        self.symbol_reader.as_ref()
    }

    /// Access the highlight store reader for this segment, if available.
    ///
    /// Returns `None` if the segment was built without highlight data
    /// (e.g., old segments or segments with no supported languages).
    pub fn highlight_reader(&self) -> Option<&crate::highlight::HighlightStoreReader> {
        self.highlight_reader.as_ref()
    }

    /// Create a `MetadataReader` for this segment's metadata.
    ///
    /// Useful for iterating all entries (e.g. during compaction).
    /// This skips header re-validation since the header was already
    /// validated during [`Segment::open()`].
    pub fn metadata_reader(&self) -> MetadataReader<'_> {
        MetadataReader::new_unchecked(
            &self.meta_mmap,
            &self.paths_mmap,
            self.entry_count,
            self.meta_entry_size,
        )
    }

    /// Look up file metadata by file ID.
    ///
    /// Creates an ephemeral `MetadataReader` from the stored memory maps.
    /// Returns `Ok(None)` if the file ID does not exist in this segment.
    pub fn get_metadata(&self, file_id: FileId) -> Result<Option<FileMetadata>, IndexError> {
        let reader = MetadataReader::new_unchecked(
            &self.meta_mmap,
            &self.paths_mmap,
            self.entry_count,
            self.meta_entry_size,
        );
        reader.get(file_id)
    }

    /// Look up only the `size_bytes` field for a file by its ID.
    ///
    /// Lightweight alternative to [`get_metadata()`](Self::get_metadata) --
    /// avoids deserializing the full entry. Used for candidate ordering
    /// (sort by file size before verification).
    pub fn get_size_bytes(&self, file_id: FileId) -> Result<Option<u32>, IndexError> {
        let reader = MetadataReader::new_unchecked(
            &self.meta_mmap,
            &self.paths_mmap,
            self.entry_count,
            self.meta_entry_size,
        );
        reader.get_size_bytes(file_id)
    }

    /// Return all file IDs in this segment.
    ///
    /// Used when trigram-based candidate filtering is not possible (e.g.,
    /// regex patterns with no literal prefix), requiring a full scan.
    pub fn all_file_ids(&self) -> Result<Vec<FileId>, IndexError> {
        Ok((0..self.entry_count).map(FileId).collect())
    }

    /// Load the tombstone set for this segment, using a cached copy if available.
    ///
    /// On first call, reads `tombstones.bin` from disk and caches the result.
    /// Subsequent calls return a clone of the cached value without disk I/O.
    ///
    /// The cache is updated via [`set_cached_tombstones`](Self::set_cached_tombstones)
    /// when tombstones are mutated externally (e.g., during `apply_changes`).
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::Io`] if the file cannot be read, or
    /// [`IndexError::IndexCorruption`] if the file is non-empty but malformed.
    pub fn load_tombstones(&self) -> Result<TombstoneSet, IndexError> {
        // Fast path: read lock
        {
            let guard = self
                .cached_tombstones
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ts) = guard.as_ref() {
                return Ok(ts.clone());
            }
        }
        // Slow path: write lock with double-check
        let mut guard = self
            .cached_tombstones
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(ts) = guard.as_ref() {
            return Ok(ts.clone());
        }
        let ts = self.load_tombstones_from_disk()?;
        *guard = Some(ts.clone());
        Ok(ts)
    }

    /// Update the cached tombstone set.
    ///
    /// Called after tombstones are mutated on disk (e.g., by `apply_changes`)
    /// to keep the in-memory cache consistent.
    pub(crate) fn set_cached_tombstones(&self, ts: TombstoneSet) {
        *self
            .cached_tombstones
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(ts);
    }

    /// Read tombstones from disk without caching.
    fn load_tombstones_from_disk(&self) -> Result<TombstoneSet, IndexError> {
        let path = self.dir_path.join("tombstones.bin");
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TombstoneSet::new());
            }
            Err(e) => return Err(e.into()),
        };
        if data.is_empty() {
            return Ok(TombstoneSet::new());
        }
        TombstoneSet::read_from(&path)
    }
}

/// Writer for building a new segment from a set of input files.
///
/// Creates a segment directory containing all index files (trigrams.bin,
/// meta.bin, paths.bin, content.zst, tombstones.bin) by orchestrating the
/// existing M1 writers. The build is atomic: files are written to a temp
/// directory first, then renamed to the final `seg_NNNN/` path.
pub struct SegmentWriter {
    base_dir: PathBuf,
    segment_id: SegmentId,
}

impl SegmentWriter {
    /// Create a new segment writer.
    ///
    /// # Arguments
    ///
    /// * `base_dir` - The segments directory (e.g. `.ferret_index/segments/`).
    ///   The segment will be created as a subdirectory named `seg_NNNN`.
    /// * `segment_id` - The ID for this segment.
    pub fn new(base_dir: &Path, segment_id: SegmentId) -> Self {
        SegmentWriter {
            base_dir: base_dir.to_path_buf(),
            segment_id,
        }
    }

    /// Build the segment from a set of input files.
    ///
    /// This method:
    /// 1. Creates a temp directory in base_dir
    /// 2. For each file: hashes content (blake3), detects language, counts lines
    /// 3. Builds trigram posting lists via PostingListBuilder
    /// 4. Writes trigrams.bin via TrigramIndexWriter
    /// 5. Writes meta.bin + paths.bin via MetadataBuilder
    /// 6. Writes content.zst via ContentStoreWriter
    /// 7. Creates empty tombstones.bin
    /// 8. Atomically renames temp dir to final seg_NNNN/ path
    ///
    /// Returns the opened Segment on success.
    pub fn build(self, files: Vec<InputFile>) -> Result<Segment, IndexError> {
        self.build_with_progress(files, || {})
    }

    /// Build the segment from a set of input files, calling `on_file_done`
    /// after each file has been processed (trigram extraction, content
    /// compression, and metadata recording).
    ///
    /// This is identical to [`build`](Self::build) but accepts a progress
    /// callback so callers can report indexing progress.
    pub fn build_with_progress<F: Fn() + Sync>(
        self,
        files: Vec<InputFile>,
        on_file_done: F,
    ) -> Result<Segment, IndexError> {
        let seg_name = format!("seg_{:04}", self.segment_id.0);
        let final_dir = self.base_dir.join(&seg_name);
        let temp_dir = self
            .base_dir
            .join(format!(".{seg_name}_tmp_{}", std::process::id()));

        // Clean up any leftover temp dir from a previous crash
        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir)?;
        }
        fs::create_dir_all(&temp_dir)?;

        // Build result, cleaning up temp dir on error
        match self.build_inner(&temp_dir, &final_dir, files, on_file_done) {
            Ok(segment) => Ok(segment),
            Err(e) => {
                // Best-effort cleanup of temp dir
                let _ = fs::remove_dir_all(&temp_dir);
                Err(e)
            }
        }
    }

    fn build_inner<F: Fn() + Sync>(
        &self,
        temp_dir: &Path,
        final_dir: &Path,
        files: Vec<InputFile>,
        on_file_done: F,
    ) -> Result<Segment, IndexError> {
        /// Zstd compression level matching `ContentStoreWriter::add_content`.
        const ZSTD_LEVEL: i32 = 3;

        /// Per-file results computed in the parallel phase.
        struct ProcessedFile {
            content_hash: [u8; 16],
            language: Language,
            line_count: u32,
            compressed: Vec<u8>,
            /// Pre-compressed highlight data and line count, or None if language unsupported.
            highlight_compressed: Option<(Vec<u8>, u32)>,
            #[cfg(feature = "symbols")]
            symbols: Vec<crate::symbol_extractor::SymbolEntry>,
        }

        let mut posting_builder = PostingListBuilder::file_only_with_capacity(65536);
        let mut metadata_builder = MetadataBuilder::new();
        let mut content_writer =
            ContentStoreWriter::new(&temp_dir.join("content.zst")).map_err(IndexError::Io)?;
        let mut highlight_writer =
            crate::highlight::HighlightStoreWriter::new(&temp_dir.join("highlights.zst"))
                .map_err(IndexError::Io)?;

        // Phase 1: Parallel per-file processing (blake3 hash + zstd compress)
        let processed: Vec<ProcessedFile> = files
            .par_iter()
            .map(|input| {
                let hash = blake3::hash(&input.content);
                let mut content_hash = [0u8; 16];
                content_hash.copy_from_slice(&hash.as_bytes()[..16]);

                let language = Language::from_path(Path::new(&input.path));
                let line_count = input.content.iter().filter(|&&b| b == b'\n').count() as u32;

                let compressed = zstd::bulk::compress(&input.content, ZSTD_LEVEL)
                    .expect("zstd compression should not fail on valid input");

                // Unified tree-sitter processing: parse once, extract highlights + symbols
                #[cfg(feature = "symbols")]
                let ts_results =
                    crate::tree_sitter_process::process_file(&input.content, language, FileId(0));
                #[cfg(feature = "symbols")]
                let highlight_compressed = ts_results.highlights.map(|line_tokens| {
                    let fh = crate::highlight::build_file_highlight(&line_tokens);
                    let serialized = crate::highlight::serialize_file_highlight(&fh);
                    let hl_compressed = zstd::bulk::compress(&serialized, ZSTD_LEVEL)
                        .expect("zstd compress highlight");
                    let lines = fh.line_offsets.len() as u32;
                    (hl_compressed, lines)
                });
                #[cfg(not(feature = "symbols"))]
                let highlight_compressed: Option<(Vec<u8>, u32)> = None;

                #[cfg(feature = "symbols")]
                let symbols = ts_results.symbols;

                on_file_done();

                ProcessedFile {
                    content_hash,
                    language,
                    line_count,
                    compressed,
                    highlight_compressed,
                    #[cfg(feature = "symbols")]
                    symbols,
                }
            })
            .collect();

        // Phase 2: Sequential writes (offset ordering + PostingListBuilder mutation)
        for (i, (input, proc_file)) in files.iter().zip(processed.iter()).enumerate() {
            let file_id = FileId(u32::try_from(i).map_err(|_| {
                IndexError::IndexCorruption("too many files for segment (>4B)".to_string())
            })?);

            // Add to trigram posting lists (mutates HashMap, must be sequential)
            posting_builder.add_file(file_id, &input.content);

            // Write pre-compressed content
            let (content_offset, content_len) = content_writer
                .add_raw(&proc_file.compressed)
                .map_err(IndexError::Io)?;

            // Write pre-compressed highlights (if available)
            let (highlight_offset, highlight_len, highlight_lines) =
                if let Some((ref hl_compressed, hl_lines)) = proc_file.highlight_compressed {
                    let (off, len) = highlight_writer
                        .add_raw(hl_compressed)
                        .map_err(IndexError::Io)?;
                    (off, len, hl_lines)
                } else {
                    (0, 0, 0)
                };

            metadata_builder.add_file(FileMetadata {
                file_id,
                path: input.path.clone(),
                content_hash: proc_file.content_hash,
                language: proc_file.language,
                size_bytes: u32::try_from(input.content.len()).unwrap_or(u32::MAX),
                mtime_epoch_secs: input.mtime,
                line_count: proc_file.line_count,
                content_offset,
                content_len,
                highlight_offset,
                highlight_len,
                highlight_lines,
            });
        }

        // Finalize posting lists (sort + dedup)
        posting_builder.finalize();

        // Write trigrams.bin
        TrigramIndexWriter::write(&posting_builder, &temp_dir.join("trigrams.bin"))?;

        // Write meta.bin + paths.bin
        let mut meta_file = fs::File::create(temp_dir.join("meta.bin"))?;
        let mut paths_file = fs::File::create(temp_dir.join("paths.bin"))?;
        metadata_builder
            .write_to(&mut meta_file, &mut paths_file)
            .map_err(IndexError::Io)?;

        // Finish content store and highlight store
        content_writer.finish().map_err(IndexError::Io)?;
        highlight_writer.finish().map_err(IndexError::Io)?;

        // Create empty tombstones.bin
        fs::write(temp_dir.join("tombstones.bin"), b"")?;

        // Collect pre-extracted symbols and write symbol index (when symbols feature is enabled)
        #[cfg(feature = "symbols")]
        {
            use crate::symbol_index::{SymbolIndexWriter, SymbolRecord};

            let mut symbol_records = Vec::new();
            for (i, proc_file) in processed.iter().enumerate() {
                let file_id = FileId(i as u32);
                for entry in &proc_file.symbols {
                    symbol_records.push(SymbolRecord {
                        file_id,
                        name: entry.name.clone(),
                        kind: entry.kind,
                        line: entry.line,
                        column: entry.column,
                    });
                }
            }
            SymbolIndexWriter::write(&symbol_records, temp_dir)?;
        }

        // Atomic rename temp dir to final path
        fs::rename(temp_dir, final_dir)?;

        // Open and return the segment
        Segment::open(final_dir, self.segment_id)
    }

    /// Build a segment from pre-processed compact files.
    ///
    /// Unlike [`build`](Self::build), this method:
    /// - Uses `ContentStoreWriter::add_raw()` to copy pre-compressed bytes directly
    /// - Reuses `content_hash`, `language`, `size_bytes`, `line_count` from source metadata
    /// - Only decompresses content for trigram extraction (unavoidable due to file_id remapping)
    ///
    /// This eliminates the blake3 hash, zstd compress, language detection, and line
    /// counting steps, roughly halving compaction CPU time and peak memory.
    pub fn build_from_compact(self, files: Vec<CompactInputFile>) -> Result<Segment, IndexError> {
        let seg_name = format!("seg_{:04}", self.segment_id.0);
        let final_dir = self.base_dir.join(&seg_name);
        let temp_dir = self
            .base_dir
            .join(format!(".{seg_name}_tmp_{}", std::process::id()));

        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir)?;
        }
        fs::create_dir_all(&temp_dir)?;

        match self.build_compact_inner(&temp_dir, &final_dir, files) {
            Ok(segment) => Ok(segment),
            Err(e) => {
                let _ = fs::remove_dir_all(&temp_dir);
                Err(e)
            }
        }
    }

    fn build_compact_inner(
        &self,
        temp_dir: &Path,
        final_dir: &Path,
        files: Vec<CompactInputFile>,
    ) -> Result<Segment, IndexError> {
        let mut posting_builder = PostingListBuilder::file_only_with_capacity(65536);
        let mut metadata_builder = MetadataBuilder::new();
        let mut content_writer =
            ContentStoreWriter::new(&temp_dir.join("content.zst")).map_err(IndexError::Io)?;
        let mut highlight_writer =
            crate::highlight::HighlightStoreWriter::new(&temp_dir.join("highlights.zst"))
                .map_err(IndexError::Io)?;

        #[cfg(feature = "symbols")]
        let mut symbol_records: Vec<crate::symbol_index::SymbolRecord> = Vec::new();

        for (i, file) in files.iter().enumerate() {
            let file_id = FileId(u32::try_from(i).map_err(|_| {
                IndexError::IndexCorruption("too many files for segment (>4B)".to_string())
            })?);

            // Trigram extraction from raw content (must be sequential)
            posting_builder.add_file(file_id, &file.raw_content);

            // Write pre-compressed content directly — no re-compress
            let (content_offset, content_len) = content_writer
                .add_raw(&file.compressed)
                .map_err(IndexError::Io)?;

            // Unified tree-sitter processing for highlights + symbols
            #[cfg(feature = "symbols")]
            let compact_ts =
                crate::tree_sitter_process::process_file(&file.raw_content, file.language, file_id);
            #[cfg(not(feature = "symbols"))]
            let compact_hl: Option<Vec<Vec<crate::highlight::Token>>> = None;
            #[cfg(feature = "symbols")]
            let compact_hl = compact_ts.highlights;

            let (highlight_offset, highlight_len, highlight_lines) = if let Some(line_tokens) =
                compact_hl
            {
                let fh = crate::highlight::build_file_highlight(&line_tokens);
                let (off, len, lines) = highlight_writer.add_file(&fh).map_err(IndexError::Io)?;
                (off, len, lines)
            } else {
                (0, 0, 0)
            };

            #[cfg(feature = "symbols")]
            for entry in compact_ts.symbols {
                symbol_records.push(crate::symbol_index::SymbolRecord {
                    file_id,
                    name: entry.name.clone(),
                    kind: entry.kind,
                    line: entry.line,
                    column: entry.column,
                });
            }

            metadata_builder.add_file(FileMetadata {
                file_id,
                path: file.path.clone(),
                content_hash: file.content_hash,
                language: file.language,
                size_bytes: file.size_bytes,
                mtime_epoch_secs: file.mtime,
                line_count: file.line_count,
                content_offset,
                content_len,
                highlight_offset,
                highlight_len,
                highlight_lines,
            });
        }

        posting_builder.finalize();

        TrigramIndexWriter::write(&posting_builder, &temp_dir.join("trigrams.bin"))?;

        let mut meta_file = fs::File::create(temp_dir.join("meta.bin"))?;
        let mut paths_file = fs::File::create(temp_dir.join("paths.bin"))?;
        metadata_builder
            .write_to(&mut meta_file, &mut paths_file)
            .map_err(IndexError::Io)?;

        content_writer.finish().map_err(IndexError::Io)?;
        highlight_writer.finish().map_err(IndexError::Io)?;
        fs::write(temp_dir.join("tombstones.bin"), b"")?;

        #[cfg(feature = "symbols")]
        crate::symbol_index::SymbolIndexWriter::write(&symbol_records, temp_dir)?;

        fs::rename(temp_dir, final_dir)?;
        Segment::open(final_dir, self.segment_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_file_construction() {
        let input = InputFile {
            path: "src/main.rs".to_string(),
            content: b"fn main() {}".to_vec(),
            mtime: 1700000000,
        };
        assert_eq!(input.path, "src/main.rs");
        assert_eq!(input.content, b"fn main() {}");
        assert_eq!(input.mtime, 1700000000);
    }

    #[test]
    fn test_segment_writer_creates_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let segment_id = SegmentId(0);
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

        let writer = SegmentWriter::new(&base_dir, segment_id);
        let segment = writer.build(files).unwrap();

        // Verify the segment directory exists with the correct name
        let seg_dir = base_dir.join("seg_0000");
        assert!(seg_dir.exists(), "segment directory should exist");
        assert!(seg_dir.join("trigrams.bin").exists());
        assert!(seg_dir.join("meta.bin").exists());
        assert!(seg_dir.join("paths.bin").exists());
        assert!(seg_dir.join("content.zst").exists());
        assert!(seg_dir.join("tombstones.bin").exists());

        // tombstones.bin should be empty
        let tombstones = std::fs::read(seg_dir.join("tombstones.bin")).unwrap();
        assert!(
            tombstones.is_empty(),
            "tombstones should be empty initially"
        );

        // Segment should report correct metadata
        assert_eq!(segment.segment_id(), segment_id);
        assert_eq!(segment.entry_count(), 2);
    }

    #[test]
    fn test_segment_open_reads_trigrams() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![
            InputFile {
                path: "src/main.rs".to_string(),
                content: b"fn main() {}".to_vec(),
                mtime: 1700000000,
            },
            InputFile {
                path: "src/lib.rs".to_string(),
                content: b"fn parse() {}".to_vec(),
                mtime: 1700000001,
            },
        ];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        // Trigram reader should work: "fn " appears in both files
        let fids = segment
            .trigram_reader()
            .lookup_file_ids(crate::types::Trigram::from_bytes(b'f', b'n', b' '))
            .unwrap();
        assert_eq!(fids, vec![FileId(0), FileId(1)]);
    }

    #[test]
    fn test_segment_get_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![InputFile {
            path: "src/main.rs".to_string(),
            content: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
            mtime: 1700000042,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(1));
        let segment = writer.build(files).unwrap();

        let meta = segment.get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(meta.path, "src/main.rs");
        assert_eq!(meta.language, crate::types::Language::Rust);
        assert_eq!(meta.mtime_epoch_secs, 1700000042);
        assert_eq!(meta.line_count, 3); // 3 newlines
        assert_eq!(meta.file_id, FileId(0));

        // Non-existent file_id
        assert!(segment.get_metadata(FileId(99)).unwrap().is_none());
    }

    #[test]
    fn test_segment_read_content() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let original_content = b"fn main() { println!(\"hello world\"); }";
        let files = vec![InputFile {
            path: "src/main.rs".to_string(),
            content: original_content.to_vec(),
            mtime: 1700000000,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        // Read content via metadata's offset/len
        let meta = segment.get_metadata(FileId(0)).unwrap().unwrap();
        let content = segment
            .content_reader()
            .read_content(meta.content_offset, meta.content_len)
            .unwrap();
        assert_eq!(content, original_content);
    }

    #[test]
    fn test_segment_dir_path() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() {}".to_vec(),
            mtime: 0,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(42));
        let segment = writer.build(files).unwrap();

        assert_eq!(segment.dir_path(), base_dir.join("seg_0042"));
        assert_eq!(segment.segment_id(), SegmentId(42));
    }

    #[test]
    fn test_segment_writer_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(vec![]).unwrap();

        assert_eq!(segment.entry_count(), 0);
        assert!(segment.get_metadata(FileId(0)).unwrap().is_none());
    }

    #[test]
    fn test_segment_writer_no_temp_dir_left() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() {}".to_vec(),
            mtime: 0,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let _segment = writer.build(files).unwrap();

        // No temp directory should remain — only seg_0000
        let entries: Vec<String> = fs::read_dir(&base_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "seg_0000");
    }

    #[test]
    fn test_segment_content_hash_is_blake3() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let content = b"fn main() {}";
        let files = vec![InputFile {
            path: "main.rs".to_string(),
            content: content.to_vec(),
            mtime: 0,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        let meta = segment.get_metadata(FileId(0)).unwrap().unwrap();

        // Verify hash matches blake3 truncated to 16 bytes
        let expected_hash = blake3::hash(content);
        let expected_16: [u8; 16] = expected_hash.as_bytes()[..16].try_into().unwrap();
        assert_eq!(meta.content_hash, expected_16);
    }

    // ---- Task 4: Segment reopening test ----

    #[test]
    fn test_segment_reopen_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let content_a = b"fn alpha() { let x = 1; }";
        let content_b = b"fn beta() { let y = 2; }";
        let files = vec![
            InputFile {
                path: "alpha.rs".to_string(),
                content: content_a.to_vec(),
                mtime: 100,
            },
            InputFile {
                path: "beta.rs".to_string(),
                content: content_b.to_vec(),
                mtime: 200,
            },
        ];

        // Build the segment
        let writer = SegmentWriter::new(&base_dir, SegmentId(5));
        let _segment = writer.build(files).unwrap();

        // Drop the segment, then reopen from disk
        drop(_segment);

        let seg_dir = base_dir.join("seg_0005");
        let reopened = Segment::open(&seg_dir, SegmentId(5)).unwrap();

        // Verify everything works after reopening
        assert_eq!(reopened.segment_id(), SegmentId(5));
        assert_eq!(reopened.entry_count(), 2);

        // Metadata
        let meta_a = reopened.get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(meta_a.path, "alpha.rs");
        assert_eq!(meta_a.mtime_epoch_secs, 100);

        let meta_b = reopened.get_metadata(FileId(1)).unwrap().unwrap();
        assert_eq!(meta_b.path, "beta.rs");

        // Content roundtrip
        let read_a = reopened
            .content_reader()
            .read_content(meta_a.content_offset, meta_a.content_len)
            .unwrap();
        assert_eq!(read_a, content_a);

        let read_b = reopened
            .content_reader()
            .read_content(meta_b.content_offset, meta_b.content_len)
            .unwrap();
        assert_eq!(read_b, content_b);

        // Trigram lookup
        let fids = reopened
            .trigram_reader()
            .lookup_file_ids(crate::types::Trigram::from_bytes(b'f', b'n', b' '))
            .unwrap();
        assert_eq!(fids, vec![FileId(0), FileId(1)]);
    }

    // ---- Task 5: Edge case and error handling tests ----

    #[test]
    fn test_segment_writer_single_file_short_content() {
        // Content shorter than 3 bytes produces no trigrams — should still work
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![InputFile {
            path: "tiny.txt".to_string(),
            content: b"ab".to_vec(), // only 2 bytes, no trigrams
            mtime: 0,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        assert_eq!(segment.entry_count(), 1);
        let meta = segment.get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(meta.path, "tiny.txt");
        assert_eq!(meta.size_bytes, 2);
        assert_eq!(meta.line_count, 0); // no newlines

        // Trigram reader should have 0 trigrams
        assert_eq!(segment.trigram_reader().trigram_count(), 0);

        // Content should still round-trip
        let content = segment
            .content_reader()
            .read_content(meta.content_offset, meta.content_len)
            .unwrap();
        assert_eq!(content, b"ab");
    }

    #[test]
    fn test_segment_writer_file_with_empty_content() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![InputFile {
            path: "empty.txt".to_string(),
            content: vec![],
            mtime: 0,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        assert_eq!(segment.entry_count(), 1);
        let meta = segment.get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(meta.size_bytes, 0);
        assert_eq!(meta.line_count, 0);
    }

    #[test]
    fn test_segment_open_nonexistent_dir() {
        let result = Segment::open(Path::new("/nonexistent/seg_0000"), SegmentId(0));
        assert!(result.is_err());
    }

    #[test]
    fn test_segment_writer_many_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        // Build a segment with 100 files
        let files: Vec<InputFile> = (0..100)
            .map(|i| InputFile {
                path: format!("file_{i:03}.rs"),
                content: format!("fn func_{i}() {{ let x = {i}; }}").into_bytes(),
                mtime: 1700000000 + i as u64,
            })
            .collect();

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        assert_eq!(segment.entry_count(), 100);

        // Spot-check first and last entries
        let first = segment.get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(first.path, "file_000.rs");

        let last = segment.get_metadata(FileId(99)).unwrap().unwrap();
        assert_eq!(last.path, "file_099.rs");
    }

    #[test]
    fn test_segment_writer_language_detection() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![
            InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() {}".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "app.py".to_string(),
                content: b"def main(): pass".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "index.ts".to_string(),
                content: b"function main() {}".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "Makefile".to_string(),
                content: b"all: build".to_vec(),
                mtime: 0,
            },
        ];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        let m0 = segment.get_metadata(FileId(0)).unwrap().unwrap();
        assert_eq!(m0.language, Language::Rust);

        let m1 = segment.get_metadata(FileId(1)).unwrap().unwrap();
        assert_eq!(m1.language, Language::Python);

        let m2 = segment.get_metadata(FileId(2)).unwrap().unwrap();
        assert_eq!(m2.language, Language::TypeScript);

        let m3 = segment.get_metadata(FileId(3)).unwrap().unwrap();
        assert_eq!(m3.language, Language::Unknown); // Makefile has no known extension
    }

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

        let reader = segment.metadata_reader();
        let all: Vec<_> = reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].path, "a.rs");
        assert_eq!(all[1].path, "b.rs");
    }

    // ---- Task: Tombstone loading tests ----

    use crate::tombstone::TombstoneSet;

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

        let tombstones = segment.load_tombstones().unwrap();
        assert!(tombstones.is_empty());
        assert_eq!(tombstones.len(), 0);
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

        // Manually write a tombstone file marking FileId(0) as deleted
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(0));
        ts.write_to(&segment.dir_path().join("tombstones.bin"))
            .unwrap();

        let loaded = segment.load_tombstones().unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(FileId(0)));
        assert!(!loaded.contains(FileId(1)));
    }

    #[test]
    fn test_segment_get_size_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![
            InputFile {
                path: "small.rs".to_string(),
                content: b"ab".to_vec(), // 2 bytes
                mtime: 0,
            },
            InputFile {
                path: "large.rs".to_string(),
                content: vec![b'x'; 5000], // 5000 bytes
                mtime: 0,
            },
        ];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        assert_eq!(segment.get_size_bytes(FileId(0)).unwrap(), Some(2));
        assert_eq!(segment.get_size_bytes(FileId(1)).unwrap(), Some(5000));
        assert_eq!(segment.get_size_bytes(FileId(99)).unwrap(), None);
    }

    #[test]
    fn test_segment_writer_uses_file_only_postings() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        let files = vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn main() {}".to_vec(),
            mtime: 0,
        }];

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        // Positional lookups should return empty
        let positions = segment
            .trigram_reader()
            .lookup_positions(crate::types::Trigram::from_bytes(b'f', b'n', b' '))
            .unwrap();
        assert!(
            positions.is_empty(),
            "segment should use file-only posting mode"
        );

        // File-level lookups still work
        let fids = segment
            .trigram_reader()
            .lookup_file_ids(crate::types::Trigram::from_bytes(b'f', b'n', b' '))
            .unwrap();
        assert_eq!(fids, vec![FileId(0)]);
    }

    #[test]
    fn test_load_tombstones_caching() {
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

        // First call loads from disk and caches
        let ts1 = segment.load_tombstones().unwrap();
        assert!(ts1.is_empty());

        // Write new tombstones to disk behind the segment's back
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(0));
        ts.write_to(&segment.dir_path().join("tombstones.bin"))
            .unwrap();

        // Second call should return CACHED (empty) value, not the on-disk update
        let ts2 = segment.load_tombstones().unwrap();
        assert!(
            ts2.is_empty(),
            "cached tombstones should be returned, not re-read from disk"
        );

        // Explicitly update the cache
        let mut updated = TombstoneSet::new();
        updated.insert(FileId(0));
        segment.set_cached_tombstones(updated);

        // Now load_tombstones should return the updated value
        let ts3 = segment.load_tombstones().unwrap();
        assert_eq!(ts3.len(), 1);
        assert!(ts3.contains(FileId(0)));
    }

    #[test]
    fn test_build_with_progress_callback_count() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

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

        let count = std::sync::atomic::AtomicUsize::new(0);
        let writer = SegmentWriter::new(&base_dir, SegmentId(1));
        writer
            .build_with_progress(files, || {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
            .unwrap();

        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "callback should fire once per file"
        );
    }

    #[test]
    fn test_load_tombstones_missing_file_returns_empty() {
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

        // Remove tombstones.bin to simulate it never being written
        let tombstone_path = segment.dir_path().join("tombstones.bin");
        fs::remove_file(&tombstone_path).unwrap();

        // Should return empty rather than an I/O error
        let ts = segment.load_tombstones().unwrap();
        assert!(ts.is_empty());
    }

    #[test]
    fn test_load_tombstones_after_directory_deleted() {
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

        // Simulate compaction deleting the entire segment directory while
        // a reader still holds an Arc<Segment> with an empty tombstone cache.
        let seg_dir = segment.dir_path().to_path_buf();
        fs::remove_dir_all(&seg_dir).unwrap();

        // load_tombstones should return empty, not an I/O error
        let ts = segment.load_tombstones().unwrap();
        assert!(ts.is_empty());
    }

    #[test]
    fn test_build_from_compact_preserves_content_and_metadata() {
        use crate::multi_search::search_segments;

        let dir = tempfile::tempdir().unwrap();
        let segments_dir = dir.path().join("segments");
        fs::create_dir_all(&segments_dir).unwrap();

        // Build a source segment with known files
        let writer = SegmentWriter::new(&segments_dir, SegmentId(0));
        let source = writer
            .build(vec![
                InputFile {
                    path: "hello.rs".to_string(),
                    content: b"fn hello_world() { println!(\"hello\"); }".to_vec(),
                    mtime: 1000,
                },
                InputFile {
                    path: "greet.rs".to_string(),
                    content: b"fn greet_user(name: &str) { println!(\"hi {}\", name); }".to_vec(),
                    mtime: 2000,
                },
            ])
            .unwrap();

        // Collect CompactInputFiles from the source segment
        let reader = source.metadata_reader();
        let mut compact_files = Vec::new();
        for entry in reader.iter_all() {
            let entry = entry.unwrap();
            let raw_compressed = source
                .content_reader()
                .read_raw_compressed(entry.content_offset, entry.content_len)
                .unwrap();
            let raw_content = source
                .content_reader()
                .read_content_with_size_hint(
                    entry.content_offset,
                    entry.content_len,
                    entry.size_bytes as usize,
                )
                .unwrap();
            compact_files.push(CompactInputFile {
                path: entry.path,
                raw_content,
                compressed: raw_compressed,
                content_hash: entry.content_hash,
                language: entry.language,
                size_bytes: entry.size_bytes,
                line_count: entry.line_count,
                mtime: entry.mtime_epoch_secs,
            });
        }

        // Build a new segment from compact files
        let writer2 = SegmentWriter::new(&segments_dir, SegmentId(1));
        let compacted = writer2.build_from_compact(compact_files).unwrap();

        // Verify metadata is preserved
        let new_reader = compacted.metadata_reader();
        let entries: Vec<_> = new_reader.iter_all().map(|e| e.unwrap()).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "hello.rs");
        assert_eq!(entries[0].mtime_epoch_secs, 1000);
        assert_eq!(entries[0].line_count, 0); // no newline in content
        assert_eq!(entries[1].path, "greet.rs");
        assert_eq!(entries[1].mtime_epoch_secs, 2000);

        // Verify content is searchable
        let snap = std::sync::Arc::new(vec![std::sync::Arc::new(compacted)]);
        let result = search_segments(&snap, "hello_world").unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, Path::new("hello.rs"));

        let result2 = search_segments(&snap, "greet_user").unwrap();
        assert_eq!(result2.files.len(), 1);
        assert_eq!(result2.files[0].path, Path::new("greet.rs"));
    }

    #[cfg(feature = "symbols")]
    #[test]
    fn test_segment_build_extracts_symbols_in_parallel() {
        use crate::symbol_index::SymbolIndexReader;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        fs::create_dir_all(&base_dir).unwrap();

        // Create enough files to exercise rayon parallelism
        let files: Vec<InputFile> = (0..20)
            .map(|i| InputFile {
                path: format!("src/mod_{i}.rs"),
                content: format!("fn func_{i}() {{}}\nstruct Struct_{i};\n").into_bytes(),
                mtime: 1700000000 + i as u64,
            })
            .collect();

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        // Verify the symbol index was created and contains expected symbols
        let seg_dir = base_dir.join("seg_0000");
        assert!(seg_dir.join("symbols.bin").exists());
        assert!(seg_dir.join("sym_trigrams.bin").exists());

        let reader = SymbolIndexReader::open(&seg_dir).unwrap();
        assert!(
            reader.symbol_count() >= 40,
            "expected at least 40 symbols (20 fns + 20 structs), got {}",
            reader.symbol_count()
        );

        // Verify we can search for symbols
        let hits = reader.search("func_0").unwrap();
        assert!(!hits.is_empty(), "should find func_0");
        assert_eq!(hits[0].name, "func_0");

        // Verify segment's symbol_reader() works
        assert!(segment.symbol_reader().is_some());
    }
}
