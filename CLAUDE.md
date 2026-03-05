# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo check --workspace          # Type-check all crates
cargo test --workspace           # Run all tests (unit + doc-tests)
cargo test -p ferret-indexer-core       # Test only the core library
cargo test -p ferret-indexer-core -- test_name  # Run a single test by name
cargo clippy --workspace -- -D warnings  # Lint (CI treats warnings as errors)
cargo fmt --all -- --check       # Check formatting
cargo fmt --all                  # Auto-format
```

Run the end-to-end demo (indexes a directory, searches it):
```bash
cargo run -p ferret-indexer-core --example demo -- <directory> <query>
```

Build a real on-disk index using the segment manager:
```bash
cargo run -p ferret-indexer-core --example build_index --release -- <directory>
```

Estimate index disk space and peak RAM for a directory:
```bash
cargo run -p ferret-indexer-core --example bench_space --release -- <directory> [segment-budget-mb]
```

Start the web interface (requires at least one registered repo):
```bash
cargo run -p ferret-indexer-cli -- web                # default port 4040
cargo run -p ferret-indexer-cli -- web --port 8080    # custom port
```

## Web Interface E2E Testing

The web interface (`ferret web`) can be tested end-to-end using Playwright MCP tools. This requires a running server with at least one indexed repo.

### Setup

```bash
# 1. Initialize and register a test repo
cargo run -p ferret-indexer-cli -- init
cargo run -p ferret-indexer-cli -- repos add . --name test-repo

# 2. Start the web server (runs in foreground, use background for automated tests)
cargo run -p ferret-indexer-cli -- web --port 4040
```

### Playwright MCP test workflow

Use the Playwright MCP tools (`browser_install`, `browser_navigate`, `browser_snapshot`, `browser_evaluate`, `browser_type`, `browser_click`, `browser_press_key`, `browser_close`) to interact with the running server.

**API tests** — navigate to JSON endpoints and verify response structure:
- `GET /api/v1/health` → `{"status":"ok","version":"...","uptime_seconds":...}`
- `GET /api/v1/repos` → `{"repos":[...]}`
- `GET /api/v1/repos/{name}/search?q=fn+main` → `{"stats":...,"results":[...],"pagination":...}`
- `GET /api/v1/repos/{name}/files/{path}` → `{"path":"...","language":"...","lines":[...]}`
- `GET /api/v1/repos/{name}/status` → `{"status":"ready","files_indexed":...,"segments":...}`

**UI tests** — navigate to `http://localhost:4040` and interact:
- Verify page loads: `browser_snapshot` should show search input, repo selector, header
- Search-as-you-type: `browser_type` into `.search-input` (slowly, to trigger htmx debounce), then `browser_snapshot` to check results
- File preview: `browser_click` a file link, verify line numbers and code render
- Keyboard shortcuts: `browser_press_key` with `Escape` (blur), `/` (focus search), `?` (help overlay), `j`/`k` (navigate results)

**SSE tests** — use `browser_evaluate` to test streaming endpoints:
```javascript
// Status stream
() => new Promise((resolve) => {
  const es = new EventSource('/api/v1/repos/test-repo/status/stream');
  es.addEventListener('status', (e) => { es.close(); resolve(JSON.parse(e.data)); });
  setTimeout(() => { es.close(); resolve(null); }, 5000);
})

// Search stream
() => new Promise((resolve) => {
  const es = new EventSource('/api/v1/repos/test-repo/search/stream?q=fn+main');
  const events = [];
  es.addEventListener('result', (e) => events.push('result'));
  es.addEventListener('stats', (e) => events.push('stats'));
  es.addEventListener('done', () => { es.close(); resolve(events); });
  setTimeout(() => { es.close(); resolve(events); }, 5000);
})
```

### Web server architecture

The web server is a stateless proxy — it does **not** own `SegmentManager` instances. All search/file/status operations are proxied to per-repo daemons over Unix sockets:

```
Browser → HTTP → Web Server (axum) → Unix Socket → Daemon → SegmentManager
```

Key modules in `ferret-web/src/`:
- `lib.rs` — `AppState`, `build_router()`, `start_server()`, route wiring
- `api.rs` — JSON API handlers (search, files, status, repos CRUD, refresh)
- `proxy.rs` — daemon proxy helpers (`search()`, `get_file()`, `status()`, `daemon_health()`)
- `error.rs` — `ApiError` type with JSON error serialization
- `ui.rs` — HTML handlers (`/`, `/search-results`, `/file/{repo}/{*path}`) using askama templates
- `static_files.rs` — embedded static file serving via `rust-embed`
- `sse.rs` — SSE streaming endpoints (`search/stream`, `status/stream`)

## Architecture

ferret is a local code indexing service for fast substring search, inspired by zoekt/codesearch. It uses **trigram indexing**: every 3-byte sequence in source files maps to posting lists of file IDs and byte offsets. Search works by extracting trigrams from the query, intersecting posting lists to find candidate files, then verifying matches against actual content.

### Workspace Crates

- **`ferret-indexer-core`** — Library with all indexing/search logic (35 modules). No binary targets.
- **`ferret-indexer-cli`** — CLI binary (`clap` + `tokio`). Subcommands: init, search, files, symbols, preview, status, reindex, estimate, repos, web, mcp. The `mcp` subcommand runs the MCP server over stdio (gated behind the `mcp` cargo feature, enabled by default). The `web` subcommand starts the web interface. The `repos` subcommand manages multi-repo registration (list/add/remove).
- **`ferret-indexer-daemon`** — Daemon protocol library. TLV wire format, Unix socket client helpers (`ensure_daemon`, `send_json_request`), structured JSON response types (`JsonSearchFrame`, `FileResponse`, `StatusResponse`, `HealthResponse`).
- **`ferret-indexer-web`** — Web interface library (`axum` + `htmx` + `askama`). Proxies all operations to per-repo daemons over Unix sockets. JSON API at `/api/v1/`, server-rendered HTML UI at `/`, SSE streaming for live search and status.

### Core Data Pipeline (ferret-indexer-core)

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
- `types.rs` — Core types: `FileId(u32)`, `Trigram([u8; 3])`, `SegmentId(u32)`, `Language` enum (59 variants + Unknown, with `from_extension()` detection), `SymbolKind` enum.
- `error.rs` — `IndexError` enum with `thiserror`. All fallible ops return `Result<T, IndexError>`.

#### M2 Modules (File Walking & Change Detection)

- `walker.rs` — `DirectoryWalkerBuilder` wraps `ignore::WalkBuilder` with `.gitignore` and `.ferretignore` support. Always skips `.git/` and `.ferret_index/`. Supports sequential and parallel walking with custom exclude patterns.
- `binary.rs` — Binary file detection: null-byte check in first 8 KB, comprehensive extension list (images, compiled, archives, media, fonts, bytecode), configurable max size (default 1 MB). `should_index_file()` combines all heuristics.
- `changes.rs` — Shared change-event types: `ChangeKind` enum (`Created`, `Modified`, `Deleted`, `Renamed`) and `ChangeEvent` struct (relative path + kind).
- `watcher.rs` — `FileWatcher` wraps `notify_debouncer_full` for filesystem event monitoring. 200 ms debounce, filters through `.gitignore` rules. Returns batched `ChangeEvent`s via `mpsc::Receiver`.
- `git_diff.rs` — `GitChangeDetector` shells out to `git` CLI for change detection. Combines committed changes (since last indexed commit), unstaged changes, and untracked files. De-duplicates by path.
- `hybrid_detector.rs` — `HybridDetector` merges file watcher (sub-second latency) + periodic git diff (default 30s) into a single de-duplicated `ChangeEvent` stream. On-demand `reindex()` support. Background thread with `Arc<AtomicBool>` flags.

#### M3 Modules (Segment Storage & Incremental Updates)

- `segment.rs` — `InputFile` (file to index), `SegmentWriter` (builds segment dirs atomically from files using M1 pipeline, uses file-only posting mode), `Segment` (loads segment from disk with trigram/metadata/content readers). Segments live under `.ferret_index/segments/seg_NNNN/`.
- `tombstone.rs` — `TombstoneSet` bitmap of deleted `FileId`s per segment. Binary persistence to `tombstones.bin` (TOMB magic). `needs_tombstone()`/`needs_new_entry()` helpers map `ChangeKind` to operations.
- `index_state.rs` — `IndexState` with `Mutex<Arc<Vec<Arc<Segment>>>>` for snapshot isolation. Lock-free reads via `Arc::clone()`, writer mutex for publishing.
- `multi_search.rs` — `search_segments()` queries all segments in a snapshot, filters tombstoned entries, verifies content matches with line/column tracking, deduplicates across segments (newest wins).
- `segment_manager.rs` — `SegmentManager` orchestrates segment lifecycle: `index_files()`, `index_files_with_budget()` (splits into size-capped segments, default 256 MB), `apply_changes()` (tombstone + rebuild), `should_compact()` (>10 segments or >30% tombstone ratio), `compact()` (merge segments removing tombstoned entries), `compact_background()` (tokio::spawn).
- `recovery.rs` — `recover_segments()` scans segment dirs on startup, cleans temp dirs, validates headers (magic + version), loads valid segments sorted by ID. `cleanup_lock_file()` for stale locks.
- `checkpoint.rs` — Checkpoint persistence for daemon indexing state. Records last indexed commit/mtime so the daemon can catch up incrementally on restart. Atomic writes via temp-file-then-rename.
- `catchup.rs` — Catch-up logic for daemon startup. Detects changes since last checkpoint via git (fast path) or hash-based diff (fallback), applies them to the segment manager, writes a new checkpoint.
- `hash_diff.rs` — Hash-based diff fallback for catch-up when git is unavailable. Walks the file tree, computes hashes, and compares against segment metadata to emit `ChangeEvent`s.
- `registry.rs` — Repo registry configuration for multi-repo support. TOML file (`~/.config/ferret/repos.toml`) listing known repositories by name and path.
- `reindex_progress.rs` — Structured progress events emitted during reindex operations. Sent as JSON over the daemon wire protocol.
- `disk.rs` — Utility for recursively computing directory sizes on disk.

#### M4 Modules (Query Engine & Ranking)

- `query.rs` — Query parser: `parse_query()` converts query strings into a `Query` AST via recursive descent. Supports `AND` (implicit), `OR`, `NOT`, exact phrases, regex patterns (`/pattern/`), path/language filters, case sensitivity modifiers. Types: `Query` enum, `LiteralQuery`, `PhraseQuery`, `RegexQuery`.
- `query_trigrams.rs` — Trigram extraction from `Query` AST. `extract_query_trigrams()` returns a `TrigramQuery` enum (`All`, `Any`, `None`) mapping query structure to trigram lookup strategy. All trigrams are ASCII-folded to match the case-folded index.
- `query_plan.rs` — Query planner: `plan_query()` builds segment-specific `QueryPlan`s from a `Query` AST. Plans include `PreFilter`s (language, path glob), sorted `ScoredTrigram` lists (smallest-first for efficient intersection), and a `VerifyStep` (literal or regex). `plan_query_multi()` plans across multiple segments.
- `ranking.rs` — Composite relevance scoring with 5 weighted signals: match type (0.30), path depth (0.15), filename match (0.15), match count (0.25), recency (0.15). `score_file_match(ScoringInput, RankingConfig) -> f64`. Types: `MatchType` enum, `RankingConfig`, `ScoringInput`.
- `verify.rs` — Content verification of trigram candidates. `ContentVerifier` supports literal, regex, and case-insensitive matching. `verify()` returns `Vec<LineMatch>` with highlight ranges. `verify_with_context()` adds before/after context lines with block merging.
- `query_match.rs` — Recursive query AST verifier. `QueryMatcher` evaluates a `Query` AST against file content by walking the AST tree — leaf nodes use `ContentVerifier`, boolean nodes (AND/OR/NOT) apply set logic.

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
.ferret_index/
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

Linear project name: ferret (team: HHC). Design docs live in `docs/design/`, implementation plans in `docs/plans/`.

### Milestone Status

- **M0** (complete) — Types, CLI skeleton, CI pipeline
- **M1** (complete) — Trigram extraction, posting lists, codec, metadata, content store, binary format reader, intersection
- **M2** (complete) — Directory walker, language detection, binary detection, file watcher, git-based change detection, hybrid change detector
- **M3** (complete) — Segment storage, tombstone bitmap, multi-segment query with snapshot isolation, segment manager with compaction, crash recovery
- **M4** (complete) — Query parser, query planner, trigram extraction from AST, content verifier, composite relevance ranking (5 weighted signals), `SearchOptions` with context lines
- **Web** (complete) — axum web server, JSON REST API, htmx frontend with search-as-you-type, file preview, SSE streaming, CLI `web` subcommand

## Conventions

- Rust edition 2024, resolver v3
- CI runs on both ubuntu-latest and macos-latest (check, clippy, test, fmt)
- Tests use `tempfile` crate for temp directories (always use `tempfile::tempdir()`, never hardcode paths)
- Index files use magic numbers and version fields for forward compatibility
- Writers use atomic temp-file-then-rename pattern for crash safety
- Git change detection shells out to `git` CLI (no libgit2 dependency)
- Directory walker honors `.gitignore` and `.ferretignore` files; always skips `.git/` and `.ferret_index/`

# ALWAYS BEFORE COMMITTING CHANGES

Before committing changes, make sure your changes pass the CI checks locally

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```
