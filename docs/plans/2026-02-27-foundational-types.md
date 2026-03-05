# Foundational Types Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Define all foundational types and error types for the ferret-indexer-core crate.

**Architecture:** Types split across 3 modules: types.rs (identifiers, enums), search.rs (result structs), error.rs (thiserror error type). All re-exported from lib.rs.

**Tech Stack:** Rust 2024, thiserror, serde

---

## Task 1: Create ferret-indexer-core crate structure

**File:** `ferret-indexer-core/Cargo.toml`

```toml
[package]
name = "ferret-indexer-core"
version = "0.1.0"
edition = "2024"

[dependencies]
thiserror = "2"
serde = { version = "1", features = ["derive"] }
```

**File:** `Cargo.toml` (workspace root — replace existing)

```toml
[workspace]
members = ["ferret-indexer-core"]
resolver = "3"
```

**File:** `ferret-indexer-core/src/lib.rs`

```rust
pub mod error;
pub mod search;
pub mod types;
```

**Test:** `cargo check -p ferret-indexer-core` — should fail (missing modules), confirming the crate is recognized.

---

## Task 2: Implement types.rs — Core identifier types and enums

**File:** `ferret-indexer-core/src/types.rs`

Define:
- `FileId(u32)` — newtype, derives: Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize. Display impl shows inner value.
- `Trigram([u8; 3])` — newtype, same derives. Methods: `from_bytes(a, b, c) -> Self`, `to_u32() -> u32`. Display shows bytes as chars if printable.
- `SegmentId(u32)` — newtype, same derives. Display impl shows inner value.
- `Language` enum — Rust, Python, TypeScript, JavaScript, Go, C, Cpp, Java, Ruby, Shell, Markdown, Unknown. Derives include Serialize/Deserialize. Method: `from_extension(ext: &str) -> Language`.
- `SymbolKind` enum — Function, Struct, Trait, Enum, Interface, Class, Method, Constant, Variable, Type, Module. Derives include Serialize/Deserialize.

**Tests in types.rs:**
- `test_file_id_display` — FileId(42) displays as "42"
- `test_trigram_from_bytes` — Trigram::from_bytes(b'a', b'b', b'c') == Trigram([b'a', b'b', b'c'])
- `test_trigram_to_u32` — verified numeric value
- `test_trigram_display` — printable bytes render as string
- `test_language_from_extension` — .rs -> Rust, .py -> Python, .unknown -> Unknown
- `test_symbol_kind_display` — each variant renders its name

**Test:** `cargo test -p ferret-indexer-core -- types` — all pass.

---

## Task 3: Implement error.rs — Central error type

**File:** `ferret-indexer-core/src/error.rs`

Define `IndexError` enum with `#[derive(Debug, thiserror::Error)]`:
- `#[error("I/O error: {0}")] Io(#[from] std::io::Error)` — auto-converts from io::Error
- `#[error("index corruption: {0}")] IndexCorruption(String)`
- `#[error("query parse error: {0}")] QueryParse(String)`
- `#[error("unsupported format version: {version}")] UnsupportedVersion { version: u32 }`
- `#[error("segment not found: {0}")] SegmentNotFound(SegmentId)` — uses SegmentId from types

Also define: `pub type Result<T> = std::result::Result<T, IndexError>;`

**Tests in error.rs:**
- `test_io_error_from` — std::io::Error converts via From
- `test_error_display` — each variant has expected message

**Test:** `cargo test -p ferret-indexer-core -- error` — all pass.

---

## Task 4: Implement search.rs — Search result types

**File:** `ferret-indexer-core/src/search.rs`

Define:
- `LineMatch` — line_number: u32, content: String, ranges: Vec<(usize, usize)>. Derives: Debug, Clone, PartialEq, Serialize, Deserialize.
- `FileMatch` — file_id: FileId, path: PathBuf, language: Language, lines: Vec<LineMatch>, score: f64. Same derives (except PartialEq needs manual handling for f64 or skip score).
- `SearchResult` — total_count: usize, files: Vec<FileMatch>, duration: Duration. Display impl shows summary: "{total_count} results in {files.len()} files ({duration:?})".

**Tests in search.rs:**
- `test_search_result_display` — formatted output matches expected string
- `test_line_match_ranges` — highlight ranges are stored correctly

**Test:** `cargo test -p ferret-indexer-core -- search` — all pass.

---

## Task 5: Wire up lib.rs re-exports

**File:** `ferret-indexer-core/src/lib.rs` (update)

Add `pub use` for all public types so users can `use ferret_indexer_core::FileId` etc.

```rust
pub mod error;
pub mod search;
pub mod types;

pub use error::{IndexError, Result};
pub use search::{FileMatch, LineMatch, SearchResult};
pub use types::{FileId, Language, SegmentId, SymbolKind, Trigram};
```

**Test:** `cargo check -p ferret-indexer-core` and `cargo test -p ferret-indexer-core` — all pass, no warnings.

---

## Task 6: Final verification and commit

- `cargo check -p ferret-indexer-core` passes
- `cargo test -p ferret-indexer-core` passes — all tests green
- `cargo doc -p ferret-indexer-core --no-deps` builds without warnings
- Commit all changes with message describing the foundational types
