# indexrs

Fast local code search using trigram indexing. Inspired by [Google Code Search](https://swtch.com/~rsc/regexp/regexp4.html) and [zoekt](https://github.com/sourcegraph/zoekt).

indexrs builds a trigram index over source files, enabling substring search without scanning every file. Queries that would take seconds with `grep -r` return in milliseconds.

## How it works

Every 3-byte sequence (trigram) in each source file is recorded in a posting list. To search for `"parse"`, indexrs extracts the trigrams `"par"`, `"ars"`, `"rse"`, looks up which files contain all three, then verifies the actual match in the narrowed candidate set.

The index is stored as immutable **segments** on disk. File changes are handled incrementally — modified files are tombstoned in old segments and re-indexed into new ones. Background compaction merges fragmented segments.

```
.indexrs/segments/
  seg_0001/
    trigrams.bin     # Trigram posting lists (delta-varint encoded)
    meta.bin         # File metadata (58-byte fixed entries)
    paths.bin        # Path string pool
    content.zst      # Zstd-compressed file contents
    tombstones.bin   # Bitmap of deleted file IDs
```

## Quick start

Run the demo to index a directory and search it:

```bash
cargo run -p indexrs-core --example demo -- <directory> <query>
```

Examples:

```bash
# Search this repo for "Trigram"
cargo run -p indexrs-core --example demo -- . "Trigram"

# Search a specific directory for "fn main"
cargo run -p indexrs-core --example demo -- ./indexrs-core/src "fn main"

# Build a real on-disk index using the segment manager
cargo run -p indexrs-core --example build_index --release -- <directory>

# Estimate index disk space and peak RAM
cargo run -p indexrs-core --example bench_space --release -- <directory>
```

## Query language

The `--query` flag enables an advanced query language with boolean operators, filters, and pattern types. Without `--query`, the search argument is treated as a plain substring (or regex with `--regex`).

```bash
# Enable the query language
indexrs search --query '<query expression>'
```

`--query` is mutually exclusive with `--regex`, `--case-sensitive`, `--ignore-case`, `--smart-case`, `--language`, and `--path` — those features are expressed inside the query string instead.

### Syntax

| Feature | Syntax | Example |
|---|---|---|
| Literal substring | bare word | `parse_query` |
| Exact phrase | `"quoted text"` | `"fn main"` |
| Regex | `/pattern/` | `/fn\s+\w+/` |
| Implicit AND | space-separated terms | `foo bar` (files containing both) |
| OR | `term1 OR term2` | `println OR eprintln` |
| NOT | `NOT term` | `NOT test` |
| Path filter | `path:prefix` | `path:src/core/` |
| Language filter | `language:name` or `lang:ext` | `language:rust`, `lang:py` |
| Case-sensitive | `case:yes term` | `case:yes FooBar` |

Operator precedence: NOT binds tightest, then AND (implicit), then OR. So `a b OR c d` parses as `(a AND b) OR (c AND d)`.

### Examples

```bash
# Find files containing both "Result" and "Error"
indexrs search --query 'Result Error'

# Find println or eprintln in Rust files
indexrs search --query 'language:rust println OR eprintln'

# Regex for function definitions, excluding tests
indexrs search --query '/fn\s+\w+/ NOT test'

# Exact phrase in files under src/
indexrs search --query 'path:src/ "fn main"'

# Case-sensitive match
indexrs search --query 'case:yes ParseError'
```

### Supported languages

`rust`, `python`, `typescript`, `javascript`, `go`, `c`, `cpp`, `java`, `ruby`, `shell`, `markdown`, `yaml`, `toml`, `json`, `xml`, `html`, `css`, `scss`, `sass`, `sql`, `protobuf`, `dockerfile`, `hcl`, `kotlin`, `swift`, `scala`, `elixir`, `erlang`, `haskell`, `ocaml`, `lua`, `perl`, `r`, `dart`, `zig`, `nix`

Both full names and common extensions work: `language:rust` and `lang:rs` are equivalent.

## Building

```bash
cargo build --workspace
```

## Testing

```bash
cargo test --workspace                     # All tests
cargo test -p indexrs-core                 # Core library only
cargo test -p indexrs-core -- test_name    # Single test
cargo clippy --workspace -- -D warnings   # Lint
cargo fmt --all -- --check                # Format check
```

## Workspace crates

| Crate | Description |
|---|---|
| `indexrs-core` | Library with all indexing, search, and change detection logic |
| `indexrs-cli` | CLI binary with subcommands: `search`, `files`, `symbols`, `preview`, `status`, `reindex` |
| `indexrs-mcp` | MCP server for IDE integration |

## Architecture

### Indexing pipeline

```
files → trigram extraction → posting lists → delta-varint codec → binary format → disk segment
```

1. **Trigram extraction** — Slide a 3-byte window over file bytes
2. **Posting lists** — Map each trigram to the file IDs that contain it (positional byte offsets are optional, disabled by default for ~78% smaller indexes)
3. **Codec** — Delta-encode sorted IDs, then varint-compress (~4x smaller than raw u32 arrays)
4. **Segment write** — Serialize to `trigrams.bin` with a sorted trigram table for O(log n) lookup

### Search pipeline

```
query → parse → plan → trigram intersection → candidate verification → ranking
```

1. **Parse** query string into an AST (supports AND, OR, NOT, phrases, regex, path/language filters)
2. **Plan** the query: extract trigrams, estimate posting list sizes, choose smallest-first intersection order
3. **Intersect** posting lists via memory-mapped binary search on the trigram table
4. **Verify** candidates by decompressing content and matching (literal, regex, or case-insensitive)
5. **Rank** results by composite score: match type, path depth, filename match, match density, recency

### Incremental updates

- New/modified files go into a new segment
- Old entries are tombstoned (bitmap in `tombstones.bin`)
- Compaction merges segments, removing tombstoned entries
- Snapshot isolation via `Arc<Vec<Arc<Segment>>>` — readers never block writers

### Change detection

Three mechanisms feed changes into the segment manager:

- **File watcher** — `notify`-based filesystem events with 200ms debounce
- **Git diff** — Periodic `git` CLI calls to detect committed + unstaged + untracked changes
- **Hybrid detector** — Merges both sources into a deduplicated change stream

## Key design decisions

- **Byte-level trigrams** — Works on raw bytes, not characters. UTF-8 multi-byte sequences are handled naturally.
- **File-only posting lists** — By default, only file-level posting lists are stored (which file IDs contain each trigram). Positional byte-offset postings are optional and disabled in production, reducing index size by ~78% and peak build RAM by ~83%.
- **Size-budgeted segments** — `index_files_with_budget()` automatically splits large file sets into segments capped at 256 MB of raw content, keeping peak memory bounded.
- **Memory-mapped reads** — `trigrams.bin`, `meta.bin`, `paths.bin` are mmap'd via `memmap2`. The OS pages data in on demand.
- **Independent zstd compression** — Each file in `content.zst` is compressed independently (level 3), enabling random access without decompressing the whole store.
- **Atomic writes** — All writers use temp-file-then-rename for crash safety.
- **Magic numbers + versions** — Every binary file has a header for forward compatibility.

## Status

| Milestone | Status |
|---|---|
| M0: Types, CLI skeleton, CI | Complete |
| M1: Trigram indexing, posting lists, codec, search | Complete |
| M2: Directory walker, binary detection, file watcher, git change detection | Complete |
| M3: Segments, tombstones, multi-segment query, compaction, crash recovery | Complete |
| M4: Query parser, query planner, content verifier, relevance ranking | Complete |

## License

This project is not yet published under a specific license.
