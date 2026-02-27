# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo check --workspace          # Type-check all crates
cargo test --workspace           # Run all tests (unit + doc-tests)
cargo test -p indexrs-core       # Test only the core library
cargo test -p indexrs-core -- test_name  # Run a single test by name
cargo clippy --workspace -- -D warnings  # Lint (CI treats warnings as errors)
cargo fmt --all -- --check       # Check formatting
cargo fmt --all                  # Auto-format
```

Run the end-to-end demo (indexes a directory, searches it):
```bash
cargo run -p indexrs-core --example demo -- <directory> <query>
```

## Architecture

indexrs is a local code indexing service for fast substring search, inspired by zoekt/codesearch. It uses **trigram indexing**: every 3-byte sequence in source files maps to posting lists of file IDs and byte offsets. Search works by extracting trigrams from the query, intersecting posting lists to find candidate files, then verifying matches against actual content.

### Workspace Crates

- **`indexrs-core`** — Library with all indexing/search logic. No binary targets.
- **`indexrs-cli`** — CLI binary (`clap` + `tokio`). Subcommands: search, files, symbols, preview, status, reindex. Currently stubs that delegate to core.
- **`indexrs-mcp`** — MCP server binary (`rmcp` + `tokio`). Currently a stub.

### Core Data Pipeline (indexrs-core)

The indexing pipeline flows: **files → trigrams → posting lists → binary format → disk**. Search reverses it: **query → trigram extraction → posting list intersection → candidate verification**.

Key modules and their roles:

- `trigram.rs` — `extract_trigrams()` slides a 3-byte window over content. `extract_unique_trigrams()` deduplicates.
- `posting.rs` — `PostingListBuilder` accumulates file-level and positional posting lists during index build. Call `add_file()` per file, then `finalize()` to sort/dedup.
- `codec.rs` — Delta-varint encoding/decoding for compact posting list serialization. Uses `integer-encoding` crate.
- `index_writer.rs` — `TrigramIndexWriter::write()` serializes `PostingListBuilder` to `trigrams.bin`. Atomic rename for crash safety.
- `index_reader.rs` — `TrigramIndexReader::open()` memory-maps `trigrams.bin`. O(log n) binary search on sorted trigram table, on-demand posting list decoding.
- `intersection.rs` — `find_candidates(reader, query)` extracts trigrams from query, looks up each, intersects file ID lists (smallest-first merge). Queries < 3 chars return empty.
- `metadata.rs` — `MetadataBuilder`/`MetadataReader` for file metadata (path, hash, language, content offset). Fixed 58-byte entries + string pool.
- `content.rs` — `ContentStoreWriter`/`ContentStoreReader` for zstd-compressed file content with random access via (offset, len) pairs.
- `types.rs` — Core types: `FileId(u32)`, `Trigram([u8; 3])`, `SegmentId(u32)`, `Language` enum, `SymbolKind` enum.
- `error.rs` — `IndexError` enum with `thiserror`. All fallible ops return `Result<T, IndexError>`.

### Binary Format (trigrams.bin)

```
[Header 10B]  magic:u32 "TRIG" | version:u16 | trigram_count:u32
[Trigram Table]  19B/entry, sorted by Trigram::to_u32()
  trigram:[u8;3] | file_list_offset:u32 | file_list_len:u32 | pos_list_offset:u32 | pos_list_len:u32
[File Posting Lists]  delta-varint encoded file_id sequences
[Positional Posting Lists]  grouped-by-file_id, delta-encoded offsets
```

All integers are little-endian. The reader uses `memmap2` for zero-copy access.

## Project Tracking

Linear project: [indexrs](https://linear.app/instacart/project/indexrs-9ebe8e9be56f) (team: HHC). Milestones M0-M1 are complete. Design docs live in `docs/design/`, implementation plans in `docs/plans/`.

## Conventions

- Rust edition 2024, resolver v3
- CI runs on both ubuntu-latest and macos-latest
- Tests use `tempfile` crate for temp directories (always use `tempfile::tempdir()`, never hardcode paths)
- Index files use magic numbers and version fields for forward compatibility
- Writers use atomic temp-file-then-rename pattern for crash safety
