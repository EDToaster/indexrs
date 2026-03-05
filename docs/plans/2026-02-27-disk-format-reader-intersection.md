# Plan: Trigram Index Disk Format, Reader, and Posting List Intersection

**Date**: 2026-02-27
**Tickets**: HHC-30, HHC-31, HHC-32

## Goal

Implement the on-disk binary format for the trigram index, a memory-mapped reader,
and the core posting list intersection algorithm. Together these form the foundation
for persisting and querying the trigram index.

## Context

The in-memory `PostingListBuilder` and varint codec (`encode_delta_varint`,
`encode_positional_postings`, etc.) already exist. This work adds:

1. **Writer** (`index_writer.rs`) -- serializes `PostingListBuilder` to `trigrams.bin`
2. **Reader** (`index_reader.rs`) -- memory-maps `trigrams.bin` and decodes on demand
3. **Intersection** (`intersection.rs`) -- intersects posting lists for multi-trigram queries

## Binary Format (trigrams.bin)

```
[Header]  (10 bytes)
  magic: u32 = 0x54524947  ("TRIG")   -- little-endian
  version: u16 = 1                     -- little-endian
  trigram_count: u32                   -- little-endian

[Trigram Table]  (19 bytes per entry, sorted by trigram u32 value)
  trigram: [u8; 3]
  file_list_offset: u32   -- byte offset into File Posting Lists section
  file_list_len: u32      -- count of file_ids
  pos_list_offset: u32    -- byte offset into Positional Posting Lists section
  pos_list_len: u32       -- count of (file_id, offset) pairs

[File Posting Lists]
  Back-to-back delta-varint-encoded file_id sequences.
  Each trigram's file list starts at the offset recorded in its table entry.

[Positional Posting Lists]
  Back-to-back positional postings (grouped by file_id, delta-encoded offsets).
  Each trigram's positional list starts at the offset recorded in its table entry.
```

Section offsets are computed as:
- `table_offset = 10` (immediately after header)
- `file_postings_offset = 10 + trigram_count * 19`
- `pos_postings_offset = file_postings_offset + total_file_postings_bytes`

## Implementation Steps

### Step 1: TrigramIndexWriter (HHC-30)

File: `ferret-indexer-core/src/index_writer.rs`

- `TrigramIndexWriter::write(builder, path)` serializes to disk.
- Collect trigrams from builder, sort by `Trigram::to_u32()`.
- First pass: encode all file posting lists and positional posting lists into
  byte buffers, recording offsets and counts for the trigram table.
- Second pass: write header, trigram table, file postings section, positional
  postings section.
- Write to a temp file in the same directory, then atomic rename for crash safety.

### Step 2: TrigramIndexReader (HHC-31)

File: `ferret-indexer-core/src/index_reader.rs`

- `TrigramIndexReader::open(path)` memory-maps the file, validates header.
- Stores computed section offsets (table_offset, file_postings_offset, pos_postings_offset).
- `lookup_file_ids(trigram)`: binary search trigram table, decode file posting list.
- `lookup_positions(trigram)`: binary search, decode positional posting list.
- `trigram_count()`: returns the count from header.
- Binary search compares `Trigram::to_u32()` values against table entries.

### Step 3: Posting List Intersection (HHC-32)

File: `ferret-indexer-core/src/intersection.rs`

- `intersect_file_ids(lists)`: sorted merge intersection.
  - Sort lists by length (smallest first) for efficiency.
  - Use two-pointer merge for pairwise intersection.
- `find_candidates(reader, query)`: extract trigrams from query, look up each,
  intersect file_id lists.
  - Queries shorter than 3 chars return empty (cannot form trigrams).

### Step 4: Wire up in lib.rs

Add `pub mod index_writer; pub mod index_reader; pub mod intersection;` and
re-exports for the public types.

## Tests

### Writer + Reader Roundtrip
- Build PostingListBuilder from Appendix A data, write, read back, verify all posting lists
- Verify trigram_count matches
- Verify binary search finds known trigrams and returns empty for absent ones
- Verify magic/version validation rejects corrupt files

### Intersection
- Overlapping lists produce correct intersection
- No overlap produces empty result
- Single list returns that list
- Empty input returns empty
- find_candidates("parse") on Appendix A index returns [FileId(1)]
- find_candidates("fn") returns empty (< 3 chars, no trigrams)
- find_candidates("main") returns [FileId(0)]

## Verification

- `cargo test -p ferret-indexer-core` -- all tests pass (86 existing + new tests)
- `cargo check --workspace` -- no errors
