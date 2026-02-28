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

Build a real on-disk index using the segment manager:
```bash
cargo run -p indexrs-core --example build_index --release -- <directory>
```

Estimate index disk space and peak RAM for a directory:
```bash
cargo run -p indexrs-core --example bench_space --release -- <directory> [segment-budget-mb]
```

## Architecture

indexrs is a local code indexing service for fast substring search, inspired by zoekt/codesearch. It uses **trigram indexing**: every 3-byte sequence in source files maps to posting lists of file IDs and byte offsets. Search works by extracting trigrams from the query, intersecting posting lists to find candidate files, then verifying matches against actual content.

### Workspace Crates

- **`indexrs-core`** — Library with all indexing/search logic (24 modules). No binary targets.
- **`indexrs-cli`** — CLI binary (`clap` + `tokio`). Subcommands: search, files, symbols, preview, status, reindex. Currently stubs that delegate to core.
- **`indexrs-mcp`** — MCP server binary (`rmcp` + `tokio`). Currently a stub.

### Core Data Pipeline (indexrs-core)

The indexing pipeline flows: **files → trigrams → posting lists → binary format → disk**. Search reverses it: **query → trigram extraction → posting list intersection → candidate verification**.

#### M0–M1 Modules (Indexing & Search)

- `trigram.rs` — `extract_trigrams()` slides a 3-byte window over content. `extract_unique_trigrams()` deduplicates. `extract_trigrams_folded()` / `extract_unique_trigrams_folded()` fold ASCII A-Z to a-z inline for case-insensitive indexing. `ascii_fold_byte()` folds a single byte.
- `posting.rs` — `PostingListBuilder` accumulates file-level posting lists during index build. Uses ASCII-folded trigram extraction (A-Z → a-z) so the index supports case-insensitive lookup by default. Two constructors: `new()` stores positions (for tests), `file_only()` skips positional postings (used by `SegmentWriter`, ~78% smaller index).
- `codec.rs` — Delta-varint encoding/decoding for compact posting list serialization. Uses `integer-encoding` crate.
- `index_writer.rs` — `TrigramIndexWriter::write()` serializes `PostingListBuilder` to `trigrams.bin`. Atomic rename for crash safety.
- `index_reader.rs` — `TrigramIndexReader::open()` memory-maps `trigrams.bin`. O(log n) binary search on sorted trigram table, on-demand posting list decoding.
- `intersection.rs` — `find_candidates(reader, query)` extracts trigrams from query, looks up each, intersects file ID lists (smallest-first merge). Queries < 3 chars return empty.
- `metadata.rs` — `MetadataBuilder`/`MetadataReader` for file metadata (path, hash, language, content offset). Fixed 58-byte entries + string pool.
- `content.rs` — `ContentStoreWriter`/`ContentStoreReader` for zstd-compressed file content with random access via (offset, len) pairs.
- `search.rs` — Search result types: `LineMatch`, `FileMatch` (with relevance score), `SearchResult` (with duration). Implements `Display` for plain-text output.
- `types.rs` — Core types: `FileId(u32)`, `Trigram([u8; 3])`, `SegmentId(u32)`, `Language` enum (36 variants with `from_extension()` detection), `SymbolKind` enum.
- `error.rs` — `IndexError` enum with `thiserror`. All fallible ops return `Result<T, IndexError>`.

#### M2 Modules (File Walking & Change Detection)

- `walker.rs` — `DirectoryWalkerBuilder` wraps `ignore::WalkBuilder` with `.gitignore` and `.indexrsignore` support. Always skips `.git/` and `.indexrs/`. Supports sequential and parallel walking with custom exclude patterns.
- `binary.rs` — Binary file detection: null-byte check in first 8 KB, comprehensive extension list (images, compiled, archives, media, fonts, bytecode), configurable max size (default 1 MB). `should_index_file()` combines all heuristics.
- `changes.rs` — Shared change-event types: `ChangeKind` enum (`Created`, `Modified`, `Deleted`, `Renamed`) and `ChangeEvent` struct (relative path + kind).
- `watcher.rs` — `FileWatcher` wraps `notify_debouncer_full` for filesystem event monitoring. 200 ms debounce, filters through `.gitignore` rules. Returns batched `ChangeEvent`s via `mpsc::Receiver`.
- `git_diff.rs` — `GitChangeDetector` shells out to `git` CLI for change detection. Combines committed changes (since last indexed commit), unstaged changes, and untracked files. De-duplicates by path.
- `hybrid_detector.rs` — `HybridDetector` merges file watcher (sub-second latency) + periodic git diff (default 30s) into a single de-duplicated `ChangeEvent` stream. On-demand `reindex()` support. Background thread with `Arc<AtomicBool>` flags.

#### M3 Modules (Segment Storage & Incremental Updates)

- `segment.rs` — `InputFile` (file to index), `SegmentWriter` (builds segment dirs atomically from files using M1 pipeline, uses file-only posting mode), `Segment` (loads segment from disk with trigram/metadata/content readers). Segments live under `.indexrs/segments/seg_NNNN/`.
- `tombstone.rs` — `TombstoneSet` bitmap of deleted `FileId`s per segment. Binary persistence to `tombstones.bin` (TOMB magic). `needs_tombstone()`/`needs_new_entry()` helpers map `ChangeKind` to operations.
- `index_state.rs` — `IndexState` with `Mutex<Arc<Vec<Arc<Segment>>>>` for snapshot isolation. Lock-free reads via `Arc::clone()`, writer mutex for publishing.
- `multi_search.rs` — `search_segments()` queries all segments in a snapshot, filters tombstoned entries, verifies content matches with line/column tracking, deduplicates across segments (newest wins).
- `segment_manager.rs` — `SegmentManager` orchestrates segment lifecycle: `index_files()`, `index_files_with_budget()` (splits into size-capped segments, default 256 MB), `apply_changes()` (tombstone + rebuild), `should_compact()` (>10 segments or >30% tombstone ratio), `compact()` (merge segments removing tombstoned entries), `compact_background()` (tokio::spawn).
- `recovery.rs` — `recover_segments()` scans segment dirs on startup, cleans temp dirs, validates headers (magic + version), loads valid segments sorted by ID. `cleanup_lock_file()` for stale locks.

### Binary Formats

All integers are little-endian. The reader uses `memmap2` for zero-copy access.

**trigrams.bin:**
```
[Header 10B]  magic:u32 "TRIG" | version:u16 | trigram_count:u32
[Trigram Table]  19B/entry, sorted by Trigram::to_u32()
  trigram:[u8;3] | file_list_offset:u32 | file_list_len:u32 | pos_list_offset:u32 | pos_list_len:u32
[File Posting Lists]  delta-varint encoded file_id sequences
[Positional Posting Lists]  grouped-by-file_id, delta-encoded offsets (optional; pos_offset/pos_len are 0/0 when absent)
```

Positional postings are optional. `SegmentWriter` uses file-only mode (`PostingListBuilder::file_only()`) by default, which sets pos_offset/pos_len to 0/0 for all trigrams. This reduces index size by ~78% and peak build RAM by ~83%. The binary format is unchanged — readers handle 0/0 gracefully by returning empty position lists.

**meta.bin:**
```
[Header 10B]  magic:u32 "META" | version:u16 | entry_count:u32
[Entries]  58B each, indexed by file_id
  file_id:u32 | path_offset:u32 | path_len:u32 | content_hash:[u8;16] |
  language:u16 | size_bytes:u32 | mtime_epoch_secs:u64 | line_count:u32 |
  content_offset:u64 | content_len:u32
```
Plus **paths.bin** — contiguous UTF-8 path strings (no separators; offsets from meta entries).

**content.zst:** Zstd-compressed blocks (level 3), each independently compressed. Random access via (offset, compressed_len) stored in metadata.

**tombstones.bin:**
```
[Header 14B]  magic:u32 "TOMB" | version:u16 | max_file_id:u32 | tombstone_count:u32
[Bitmap]  ceil(max_file_id/64) * 8 bytes of little-endian u64 words
```

### On-Disk Segment Layout

```
.indexrs/
  segments/
    seg_0001/
      meta.bin        # File metadata entries
      trigrams.bin    # Trigram posting lists
      content.zst     # Zstd-compressed file contents
      paths.bin       # Path string pool
      tombstones.bin  # Bitmap of deleted file_ids
    seg_0002/
      ...
  lock                # Advisory lock file for single-writer
```

## Project Tracking

Linear project name: indexrs (team: HHC). Design docs live in `docs/design/`, implementation plans in `docs/plans/`.

### Milestone Status

- **M0** (complete) — Types, CLI skeleton, CI pipeline
- **M1** (complete) — Trigram extraction, posting lists, codec, metadata, content store, binary format reader, intersection
- **M2** (complete) — Directory walker, language detection, binary detection, file watcher, git-based change detection, hybrid change detector
- **M3** (complete) — Segment storage, tombstone bitmap, multi-segment query with snapshot isolation, segment manager with compaction, crash recovery

## Conventions

- Rust edition 2024, resolver v3
- CI runs on both ubuntu-latest and macos-latest (check, clippy, test, fmt)
- Tests use `tempfile` crate for temp directories (always use `tempfile::tempdir()`, never hardcode paths)
- Index files use magic numbers and version fields for forward compatibility
- Writers use atomic temp-file-then-rename pattern for crash safety
- Git change detection shells out to `git` CLI (no libgit2 dependency)
- Directory walker honors `.gitignore` and `.indexrsignore` files; always skips `.git/` and `.indexrs/`
