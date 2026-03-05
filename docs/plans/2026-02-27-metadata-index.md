# File Metadata Index Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the file metadata index mapping file_id to metadata, with a path string pool and binary on-disk format.

**Architecture:** Single module `metadata.rs` in ferret-indexer-core containing `FileMetadata` (entry struct), `MetadataBuilder` (in-memory builder with add/lookup/serialize), and `MetadataReader` (zero-copy reader over byte slices). Binary format uses fixed-size 58-byte entries in meta.bin with a separate paths.bin string pool.

**Tech Stack:** Rust 2024, serde, little-endian binary format

---

## Task 1: Add Language u16 conversion methods

**File:** `ferret-indexer-core/src/types.rs`

Add `to_u16()` and `from_u16()` methods to the `Language` enum for binary serialization:

```rust
impl Language {
    pub fn to_u16(self) -> u16 { ... }
    pub fn from_u16(v: u16) -> Language { ... }
}
```

Mapping: Rust=0, Python=1, TypeScript=2, JavaScript=3, Go=4, C=5, Cpp=6, Java=7, Ruby=8, Shell=9, Markdown=10, Unknown=0xFFFF.

**Test:** `cargo test -p ferret-indexer-core -- types::tests::test_language_u16` -- passes.

---

## Task 2: Define FileMetadata struct and MetadataBuilder (skeleton + tests)

**File:** `ferret-indexer-core/src/metadata.rs` (NEW)

Define `FileMetadata` struct with all fields from the spec. Define `MetadataBuilder` with `new()`, `add_file()`, `next_file_id()`, `get()`, `get_by_path()`, `file_count()`, `iter()`.

Write failing tests first:
- `test_builder_add_and_get` -- add a few files, verify get by file_id returns correct data
- `test_builder_get_by_path` -- lookup by path works
- `test_builder_not_found` -- get for nonexistent file_id returns None
- `test_builder_file_count` -- count matches number of added files
- `test_builder_empty` -- empty builder has count 0, get returns None

**Test:** `cargo test -p ferret-indexer-core -- metadata` -- tests fail (not yet implemented), then implement and pass.

---

## Task 3: Implement MetadataBuilder methods

**File:** `ferret-indexer-core/src/metadata.rs`

Implement all MetadataBuilder methods. The builder stores entries in a Vec, uses a HashMap<String, usize> for path-to-index lookup.

**Test:** `cargo test -p ferret-indexer-core -- metadata::tests::test_builder` -- all pass.

---

## Task 4: Implement binary serialization (write_to)

**File:** `ferret-indexer-core/src/metadata.rs`

Implement `MetadataBuilder::write_to()` that writes:

meta.bin format:
- Header: magic (0x4D455441 u32 LE), version (1 u16 LE), entry_count (u32 LE)
- Entries: 58 bytes each (file_id, path_offset, path_len, content_hash, language, size_bytes, mtime_epoch_secs, line_count, content_offset, content_len) all LE

paths.bin format:
- Contiguous UTF-8 path strings, no separators

Write failing tests first:
- `test_binary_size` -- serialized meta.bin size = 10 (header) + entry_count * 58
- `test_write_empty` -- 0 entries produces valid header-only output

**Test:** `cargo test -p ferret-indexer-core -- metadata::tests::test_binary` -- fail then pass.

---

## Task 5: Implement MetadataReader

**File:** `ferret-indexer-core/src/metadata.rs`

Implement `MetadataReader::new()`, `get()`, `entry_count()`. Reader validates magic and version, reads fixed-size entries, resolves paths from the paths.bin buffer.

Write failing tests:
- `test_roundtrip` -- build metadata, write to binary, read back with MetadataReader, verify all fields match
- `test_roundtrip_empty` -- 0 files roundtrip works
- `test_roundtrip_paths` -- verify paths are correctly stored and retrieved
- `test_reader_invalid_magic` -- returns IndexError on bad magic
- `test_reader_invalid_version` -- returns IndexError on bad version

**Test:** `cargo test -p ferret-indexer-core -- metadata::tests::test_reader` -- fail then pass.

---

## Task 6: Wire up lib.rs and final verification

**File:** `ferret-indexer-core/src/lib.rs` (update)

Add `pub mod metadata;` and re-export key types.

- `cargo test -p ferret-indexer-core` -- all tests pass
- `cargo check --workspace` -- no errors
- Commit with descriptive message

---
