# Trigram Extraction and Posting Lists Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement trigram extraction (HHC-27) and in-memory posting lists builder (HHC-28) for the ferret-indexer-core crate.

**Architecture:** Two new modules in ferret-indexer-core: `trigram.rs` (extraction functions) and `posting.rs` (PostingListBuilder). Both are re-exported from lib.rs. Uses existing `Trigram` and `FileId` types from types.rs. Follows the design in `docs/design/indexing-system.md` sections 1.2.1 and Appendix A.

**Tech Stack:** Rust 2024, std collections (HashMap, HashSet)

---

## Task 1: Create trigram.rs with extract_trigrams

**File:** `ferret-indexer-core/src/trigram.rs`

Implement:
- `pub fn extract_trigrams(content: &[u8]) -> impl Iterator<Item = Trigram> + '_` — slides a 3-byte window across content, yielding a `Trigram` for each position. Content shorter than 3 bytes produces no trigrams.
- `pub fn extract_unique_trigrams(content: &[u8]) -> HashSet<Trigram>` — collects extract_trigrams into a HashSet for deduplication.

Both functions operate at the byte level, not character level.

**Tests in trigram.rs:**
- `test_extract_trigrams_empty` — empty content yields no trigrams
- `test_extract_trigrams_one_byte` — 1-byte content yields no trigrams
- `test_extract_trigrams_two_bytes` — 2-byte content yields no trigrams
- `test_extract_trigrams_three_bytes` — 3-byte content yields exactly 1 trigram
- `test_extract_trigrams_fn_main` — "fn main() {}" produces exactly the trigrams from Appendix A File 0
- `test_extract_unique_trigrams_deduplicates` — content with repeated trigrams returns each once
- `test_extract_unique_trigrams_fn_main` — "fn main() {}" produces the correct unique set

**Test:** `cargo test -p ferret-indexer-core -- trigram` — all pass.

---

## Task 2: Create posting.rs with PostingListBuilder

**File:** `ferret-indexer-core/src/posting.rs`

Implement `PostingListBuilder`:
```rust
pub struct PostingListBuilder {
    file_postings: HashMap<Trigram, Vec<FileId>>,
    positional_postings: HashMap<Trigram, Vec<(FileId, u32)>>,
}
```

Methods:
- `new()` — empty builder
- `add_file(&mut self, file_id: FileId, content: &[u8])` — extract trigrams from content; for each (offset, trigram): push file_id to file_postings, push (file_id, offset as u32) to positional_postings
- `finalize(&mut self)` — sort all file posting lists ascending; deduplicate file_ids; sort positional posting lists by (file_id, offset)
- `file_postings(&self) -> &HashMap<Trigram, Vec<FileId>>`
- `positional_postings(&self) -> &HashMap<Trigram, Vec<(FileId, u32)>>`
- `trigram_count(&self) -> usize` — number of distinct trigrams

**Tests in posting.rs:**
- `test_posting_builder_empty` — new builder has 0 trigram count
- `test_posting_builder_single_file` — add "fn main() {}", verify trigram_count matches Appendix A (10 unique for file 0)
- `test_posting_builder_appendix_a` — add both files from Appendix A, finalize, verify file and positional posting lists match the table exactly
- `test_posting_builder_finalize_sorts` — add files in reverse order, verify finalize produces sorted lists
- `test_posting_builder_file_dedup` — add same content for same file_id twice (before finalize), verify file postings are deduplicated after finalize

**Test:** `cargo test -p ferret-indexer-core -- posting` — all pass.

---

## Task 3: Wire up lib.rs

**File:** `ferret-indexer-core/src/lib.rs` (update)

Add module declarations and re-exports:
```rust
pub mod posting;
pub mod trigram;

pub use posting::PostingListBuilder;
pub use trigram::{extract_trigrams, extract_unique_trigrams};
```

**Test:** `cargo check --workspace` — no errors.

---

## Task 4: Final verification and commit

- `cargo check --workspace` passes
- `cargo test -p ferret-indexer-core` passes — all tests green
- Commit with descriptive message covering both HHC-27 and HHC-28
