# Content Store Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the content store for ferret — stores raw file contents compressed with zstd, retrievable by (offset, compressed_len).

**Architecture:** A single `content.rs` module in `ferret-indexer-core` containing `ContentStoreWriter` (appends zstd-compressed blocks to a file) and `ContentStoreReader` (mmap-based random access with on-demand decompression). Each file is independently compressed for random access. The (offset, compressed_len) tuple returned by the writer is stored externally in the metadata index.

**Tech Stack:** Rust 2024, zstd (level 3), memmap2

---

## Task 1: Add content module with failing tests

**File:** `ferret-indexer-core/src/content.rs` (NEW)

Create the module with struct definitions and empty/unimplemented method stubs. Write all required tests:

- `test_roundtrip` — write content, read back, verify exact match
- `test_multiple_files` — write 3+ files, read each independently by offset
- `test_empty_content` — write empty bytes, read back empty Vec
- `test_large_content` — write ~1MB, read back, verify
- `test_compression_effective` — verify compressed size < original for typical source
- `test_binary_content` — write non-UTF8 bytes, read back, verify
- `test_reader_file_not_found` — open nonexistent path returns proper error

**File:** `ferret-indexer-core/src/lib.rs` (UPDATE) — add `pub mod content;` and re-exports.

**Test:** `cargo test -p ferret-indexer-core -- content` — all tests fail (unimplemented).

---

## Task 2: Implement ContentStoreWriter

**File:** `ferret-indexer-core/src/content.rs` (UPDATE)

Implement:
- `ContentStoreWriter::new(path)` — create file with BufWriter
- `ContentStoreWriter::add_content(content)` — compress with zstd level 3, write to file, return (offset, compressed_len as u32)
- `ContentStoreWriter::finish()` — flush BufWriter

Key details:
- Use `zstd::bulk::compress(content, 3)` for independent per-file compression
- Track `current_offset` as u64, advance after each write
- compressed_len is u32 (max ~4GB per compressed block, more than enough)

**Test:** Writer-only tests should pass (those that don't need reader).

---

## Task 3: Implement ContentStoreReader

**File:** `ferret-indexer-core/src/content.rs` (UPDATE)

Implement:
- `ContentStoreReader::open(path)` — open file, create mmap
- `ContentStoreReader::read_content(offset, compressed_len)` — slice mmap, decompress with zstd

Key details:
- Use `memmap2::Mmap` for zero-copy access
- Validate offset + compressed_len <= mmap.len(), return IndexError on out-of-bounds
- Use `zstd::bulk::decompress()` for decompression (need upper bound or use streaming)
- Use `zstd::stream::decode_all()` for decompression (handles unknown output size)

**Test:** `cargo test -p ferret-indexer-core -- content` — all tests pass.

---

## Task 4: Final verification and commit

- `cargo test -p ferret-indexer-core` — all tests pass
- `cargo check --workspace` — no errors
- Commit with descriptive message
