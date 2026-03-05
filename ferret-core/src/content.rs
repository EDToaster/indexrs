//! Content store for compressed file storage with random access.
//!
//! The content store holds the raw content of every indexed file, compressed
//! with zstd at level 3 (good speed/ratio balance for source code). Each file
//! is independently compressed to allow random access by (offset, compressed_len).
//!
//! # Architecture
//!
//! - [`ContentStoreWriter`] appends compressed blocks sequentially to a file.
//!   Each call to [`add_content`](ContentStoreWriter::add_content) returns
//!   `(offset, compressed_len)` for storage in the metadata index.
//!
//! - [`ContentStoreReader`] memory-maps the content store file and decompresses
//!   blocks on demand given `(offset, compressed_len)`.
//!
//! # Example
//!
//! ```no_run
//! use ferret_indexer_core::content::{ContentStoreWriter, ContentStoreReader};
//! use std::path::Path;
//!
//! // Write content
//! let path = Path::new("content.zst");
//! let mut writer = ContentStoreWriter::new(path).unwrap();
//! let (offset, len) = writer.add_content(b"fn main() {}").unwrap();
//! writer.finish().unwrap();
//!
//! // Read content back
//! let reader = ContentStoreReader::open(path).unwrap();
//! let content = reader.read_content(offset, len).unwrap();
//! assert_eq!(content, b"fn main() {}");
//! ```

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use memmap2::Mmap;

use crate::IndexError;

/// Zstd compression level used for content storage.
///
/// Level 3 provides a good balance of compression speed and ratio for source
/// code, typically achieving 3-5x compression.
const ZSTD_COMPRESSION_LEVEL: i32 = 3;

/// Maximum decompressed content size (10 MB).
///
/// Prevents memory exhaustion from malicious or corrupted compressed data
/// (zip-bomb style attacks). Since the indexer's default max file size is 1 MB,
/// this provides a generous safety margin.
const MAX_DECOMPRESSED_SIZE: usize = 10 * 1024 * 1024;

/// Writer for building the content store (`content.zst`).
///
/// Appends independently compressed blocks sequentially. Each call to
/// [`add_content`](Self::add_content) compresses the input with zstd and
/// returns the `(offset, compressed_len)` pair needed to later retrieve it.
pub struct ContentStoreWriter {
    writer: BufWriter<File>,
    current_offset: u64,
}

impl std::fmt::Debug for ContentStoreWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentStoreWriter")
            .field("current_offset", &self.current_offset)
            .finish_non_exhaustive()
    }
}

impl ContentStoreWriter {
    /// Create a new content store file at the given path.
    ///
    /// The file is created (or truncated if it already exists) and wrapped
    /// in a buffered writer for efficient sequential writes.
    pub fn new(path: &Path) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            current_offset: 0,
        })
    }

    /// Add file content to the store.
    ///
    /// The content is independently compressed with zstd at level 3 and
    /// appended to the store file.
    ///
    /// Returns `(offset, compressed_len)` — the byte offset within the store
    /// and the length of the compressed block. These values are stored in the
    /// metadata index for later retrieval via [`ContentStoreReader::read_content`].
    pub fn add_content(&mut self, content: &[u8]) -> std::io::Result<(u64, u32)> {
        let compressed =
            zstd::bulk::compress(content, ZSTD_COMPRESSION_LEVEL).map_err(std::io::Error::other)?;

        let offset = self.current_offset;
        let compressed_len: u32 = compressed.len().try_into().map_err(|_| {
            std::io::Error::other(format!(
                "compressed block size {} exceeds u32::MAX",
                compressed.len()
            ))
        })?;

        self.writer.write_all(&compressed)?;
        self.current_offset += compressed_len as u64;

        Ok((offset, compressed_len))
    }

    /// Write already-compressed content to the store.
    ///
    /// Unlike [`add_content`], this method does not compress the input —
    /// the caller is responsible for providing zstd-compressed bytes.
    /// Returns `(offset, compressed_len)` like `add_content`.
    pub fn add_raw(&mut self, compressed: &[u8]) -> std::io::Result<(u64, u32)> {
        let offset = self.current_offset;
        let compressed_len: u32 = compressed.len().try_into().map_err(|_| {
            std::io::Error::other(format!(
                "compressed block size {} exceeds u32::MAX",
                compressed.len()
            ))
        })?;

        self.writer.write_all(compressed)?;
        self.current_offset += compressed_len as u64;

        Ok((offset, compressed_len))
    }

    /// Finalize and flush the content store.
    ///
    /// Ensures all buffered data is written to disk. The writer is consumed
    /// and the underlying file handle is closed.
    pub fn finish(mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// Reader for retrieving content from a memory-mapped content store.
///
/// Uses `memmap2::Mmap` for zero-copy access to the compressed data,
/// decompressing individual blocks on demand.
pub struct ContentStoreReader {
    mmap: Mmap,
}

impl std::fmt::Debug for ContentStoreReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentStoreReader")
            .field("size", &self.mmap.len())
            .finish_non_exhaustive()
    }
}

impl ContentStoreReader {
    /// Open a content store file via memory map.
    ///
    /// The file is opened read-only and memory-mapped. The OS handles paging
    /// data in from disk on demand.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: We treat the mmap as read-only. The file must not be modified
        // externally while mapped; this invariant is maintained by the segment
        // lifecycle (segments are immutable once written).
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self { mmap })
    }

    /// Read and decompress content at the given offset and compressed length.
    ///
    /// Slices the memory-mapped region at `[offset..offset+compressed_len]`
    /// and decompresses the zstd block, returning the original file content.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::IndexCorruption`] if:
    /// - The offset/length is out of bounds of the mapped file
    /// - The compressed data cannot be decompressed (corrupted block)
    pub fn read_content(&self, offset: u64, compressed_len: u32) -> crate::Result<Vec<u8>> {
        let start = usize::try_from(offset).map_err(|_| {
            IndexError::IndexCorruption(format!("content offset {offset} exceeds address space"))
        })?;
        let clen = compressed_len as usize;
        let end = start.checked_add(clen).ok_or_else(|| {
            IndexError::IndexCorruption(format!("content range overflow: {start} + {clen}"))
        })?;

        if end > self.mmap.len() {
            return Err(IndexError::IndexCorruption(format!(
                "content read out of bounds: offset={offset}, len={compressed_len}, \
                 store size={}",
                self.mmap.len()
            )));
        }

        let compressed = &self.mmap[start..end];
        let decoder = zstd::stream::Decoder::new(compressed)
            .map_err(|e| IndexError::IndexCorruption(format!("zstd decoder init failed: {e}")))?;
        let mut output = Vec::new();
        let bytes_read = decoder
            .take(MAX_DECOMPRESSED_SIZE as u64 + 1)
            .read_to_end(&mut output)
            .map_err(|e| IndexError::IndexCorruption(format!("zstd decompression failed: {e}")))?;
        if bytes_read > MAX_DECOMPRESSED_SIZE {
            return Err(IndexError::IndexCorruption(format!(
                "decompressed content exceeds size limit of {MAX_DECOMPRESSED_SIZE} bytes"
            )));
        }
        Ok(output)
    }

    /// Read and decompress content with a pre-allocation hint.
    ///
    /// Like [`read_content()`](Self::read_content) but pre-allocates the output
    /// buffer to `size_hint` bytes, avoiding reallocation during decompression.
    /// The `size_hint` should be the original uncompressed file size from metadata.
    pub fn read_content_with_size_hint(
        &self,
        offset: u64,
        compressed_len: u32,
        size_hint: usize,
    ) -> crate::Result<Vec<u8>> {
        let start = usize::try_from(offset).map_err(|_| {
            IndexError::IndexCorruption(format!("content offset {offset} exceeds address space"))
        })?;
        let clen = compressed_len as usize;
        let end = start.checked_add(clen).ok_or_else(|| {
            IndexError::IndexCorruption(format!("content range overflow: {start} + {clen}"))
        })?;

        if end > self.mmap.len() {
            return Err(IndexError::IndexCorruption(format!(
                "content read out of bounds: offset={offset}, len={compressed_len}, \
                 store size={}",
                self.mmap.len()
            )));
        }

        let compressed = &self.mmap[start..end];
        let decoder = zstd::stream::Decoder::new(compressed)
            .map_err(|e| IndexError::IndexCorruption(format!("zstd decoder init failed: {e}")))?;
        // Pre-allocate to avoid reallocation; cap at MAX_DECOMPRESSED_SIZE
        let capacity = size_hint.min(MAX_DECOMPRESSED_SIZE);
        let mut output = Vec::with_capacity(capacity);
        let bytes_read = decoder
            .take(MAX_DECOMPRESSED_SIZE as u64 + 1)
            .read_to_end(&mut output)
            .map_err(|e| IndexError::IndexCorruption(format!("zstd decompression failed: {e}")))?;
        if bytes_read > MAX_DECOMPRESSED_SIZE {
            return Err(IndexError::IndexCorruption(format!(
                "decompressed content exceeds size limit of {MAX_DECOMPRESSED_SIZE} bytes"
            )));
        }
        Ok(output)
    }
    /// Read raw compressed bytes from the content store without decompressing.
    ///
    /// Returns the zstd-compressed block as-is. Useful during compaction to
    /// copy content between segments without a decompress→re-compress cycle.
    pub fn read_raw_compressed(&self, offset: u64, compressed_len: u32) -> crate::Result<Vec<u8>> {
        let start = usize::try_from(offset).map_err(|_| {
            IndexError::IndexCorruption(format!("content offset {offset} exceeds address space"))
        })?;
        let clen = compressed_len as usize;
        let end = start.checked_add(clen).ok_or_else(|| {
            IndexError::IndexCorruption(format!("content range overflow: {start} + {clen}"))
        })?;

        if end > self.mmap.len() {
            return Err(IndexError::IndexCorruption(format!(
                "content read out of bounds: offset={offset}, len={compressed_len}, \
                 store size={}",
                self.mmap.len()
            )));
        }

        Ok(self.mmap[start..end].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// Helper: create a content store in a temp dir, write entries, finish, return path.
    fn write_store(
        dir: &std::path::Path,
        entries: &[&[u8]],
    ) -> (std::path::PathBuf, Vec<(u64, u32)>) {
        let path = dir.join("content.zst");
        let mut writer = ContentStoreWriter::new(&path).unwrap();
        let mut positions = Vec::new();
        for entry in entries {
            positions.push(writer.add_content(entry).unwrap());
        }
        writer.finish().unwrap();
        (path, positions)
    }

    #[test]
    fn test_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"fn main() { println!(\"Hello, world!\"); }";
        let (path, positions) = write_store(dir.path(), &[content.as_slice()]);

        let reader = ContentStoreReader::open(&path).unwrap();
        let (offset, len) = positions[0];
        let result = reader.read_content(offset, len).unwrap();
        assert_eq!(result, content);
    }

    #[test]
    fn test_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<&[u8]> = vec![
            b"fn main() {}",
            b"use std::io;\nfn read_file() -> io::Result<String> { todo!() }",
            b"#[derive(Debug)]\nstruct Config {\n    name: String,\n    value: u64,\n}",
            b"// A comment\nconst MAX: usize = 1024;",
        ];

        let (path, positions) = write_store(dir.path(), &files);
        let reader = ContentStoreReader::open(&path).unwrap();

        // Read each file independently and verify content matches
        for (i, original) in files.iter().enumerate() {
            let (offset, len) = positions[i];
            let result = reader.read_content(offset, len).unwrap();
            assert_eq!(
                &result,
                original,
                "content mismatch for file {i}: expected {} bytes, got {} bytes",
                original.len(),
                result.len()
            );
        }

        // Read in reverse order to confirm random access works
        for i in (0..files.len()).rev() {
            let (offset, len) = positions[i];
            let result = reader.read_content(offset, len).unwrap();
            assert_eq!(&result, files[i]);
        }
    }

    #[test]
    fn test_empty_content() {
        let dir = tempfile::tempdir().unwrap();
        let (path, positions) = write_store(dir.path(), &[b""]);

        let reader = ContentStoreReader::open(&path).unwrap();
        let (offset, len) = positions[0];
        let result = reader.read_content(offset, len).unwrap();
        assert!(
            result.is_empty(),
            "expected empty content, got {} bytes",
            result.len()
        );
    }

    #[test]
    fn test_large_content() {
        let dir = tempfile::tempdir().unwrap();

        // Generate ~1MB of realistic-looking source code content
        let mut large_content = Vec::with_capacity(1_048_576);
        let line =
            b"    let result = some_function(arg1, arg2, arg3).map_err(|e| Error::from(e))?;\n";
        while large_content.len() < 1_048_576 {
            large_content.extend_from_slice(line);
        }

        let (path, positions) = write_store(dir.path(), &[&large_content]);
        let reader = ContentStoreReader::open(&path).unwrap();
        let (offset, len) = positions[0];
        let result = reader.read_content(offset, len).unwrap();
        assert_eq!(result, large_content);
    }

    #[test]
    fn test_compression_effective() {
        let dir = tempfile::tempdir().unwrap();

        // Typical source code has significant redundancy — compression should help
        let source_code = r#"
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// A configuration manager that reads and writes key-value pairs.
pub struct ConfigManager {
    path: PathBuf,
    values: HashMap<String, String>,
}

impl ConfigManager {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            values: HashMap::new(),
        }
    }

    pub fn load(&mut self) -> io::Result<()> {
        let file = fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            if let Some((key, value)) = line.split_once('=') {
                self.values.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.values.insert(key.into(), value.into());
    }

    pub fn save(&self) -> io::Result<()> {
        let mut file = fs::File::create(&self.path)?;
        for (key, value) in &self.values {
            writeln!(file, "{key} = {value}")?;
        }
        Ok(())
    }
}
"#;

        let path = dir.path().join("content.zst");
        let mut writer = ContentStoreWriter::new(&path).unwrap();
        let (_, compressed_len) = writer.add_content(source_code.as_bytes()).unwrap();
        writer.finish().unwrap();

        let original_len = source_code.len() as u32;
        assert!(
            compressed_len < original_len,
            "compressed size ({compressed_len}) should be less than original size ({original_len})"
        );

        // For source code, we expect at least 2x compression
        let ratio = original_len as f64 / compressed_len as f64;
        assert!(
            ratio > 2.0,
            "compression ratio ({ratio:.1}x) should be at least 2x for source code"
        );
    }

    #[test]
    fn test_binary_content() {
        let dir = tempfile::tempdir().unwrap();

        // Non-UTF8 binary content: all byte values including nulls
        let mut binary: Vec<u8> = (0..=255u8).collect();
        // Repeat to make it more substantial
        binary.extend_from_slice(&binary.clone());
        binary.extend_from_slice(&[0x00, 0xFF, 0xFE, 0xFD, 0x80, 0x81]);

        let (path, positions) = write_store(dir.path(), &[&binary]);
        let reader = ContentStoreReader::open(&path).unwrap();
        let (offset, len) = positions[0];
        let result = reader.read_content(offset, len).unwrap();
        assert_eq!(result, binary);
    }

    #[test]
    fn test_reader_file_not_found() {
        let result = ContentStoreReader::open(Path::new("/nonexistent/path/content.zst"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_read_out_of_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_store(dir.path(), &[b"hello"]);

        let reader = ContentStoreReader::open(&path).unwrap();
        // Try to read way past the end of the file
        let result = reader.read_content(0, u32::MAX);
        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::IndexCorruption(msg) => {
                assert!(msg.contains("out of bounds"), "unexpected message: {msg}");
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    #[test]
    fn test_read_raw_compressed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("content.zst");

        let original = b"fn main() { println!(\"hello\"); }";
        let mut writer = ContentStoreWriter::new(&path).unwrap();
        let (offset, compressed_len) = writer.add_content(original).unwrap();
        writer.finish().unwrap();

        let reader = ContentStoreReader::open(&path).unwrap();
        let raw = reader.read_raw_compressed(offset, compressed_len).unwrap();

        // Raw bytes should decompress to original content
        let decompressed = zstd::bulk::decompress(&raw, 1024 * 1024).unwrap();
        assert_eq!(decompressed, original);
    }
}
