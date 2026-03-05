# ferret Indexing System Design

## Overview

ferret is a local code indexing service that maintains a persistent index of files in
large repositories, enabling fast code search with capabilities comparable to GitHub's
code search. This document specifies the index architecture, storage format, incremental
update strategy, query engine, and crate dependencies.

The design draws on published work from GitHub's Blackbird engine, Google/Sourcegraph's
zoekt, Russ Cox's trigram search (codesearch), and livegrep's suffix-array approach, adapted
for a single-machine, single-repo (or small set of repos) use case.

---

## 1. Index Architecture

### 1.1 Approach Comparison

| Approach | Space | Build Time | Query Types | Regex Performance | Complexity |
|---|---|---|---|---|---|
| **Trigram index** | ~0.2-0.3x source | Fast | Substring, regex | Good (candidate + verify) | Low |
| **Suffix array** | ~3-5x source | Moderate | Arbitrary regex | Excellent | High |
| **Full-text (tantivy/Lucene)** | ~0.3-0.5x source | Fast | Token search, some regex | Poor for arbitrary substrings | Low |
| **Sparse ngram (Blackbird)** | ~0.5x source (at scale) | Fast | Substring, regex | Excellent | High |

**Recommendation: Trigram index with content storage.**

Rationale:
- Trigrams are the proven approach for code search at our scale (single machine, repos up to
  ~1M files). Both zoekt and codesearch use them successfully.
- Suffix arrays (livegrep) offer better regex performance but require 3-5x source size in RAM,
  which is prohibitive for large monorepos. A 10GB repo would need 30-50GB of RAM.
- Full-text indexes (tantivy) are designed for natural language with tokenization and stemming.
  Code search requires substring matching (`HttpReq` must match `HttpRequest`), which
  full-text indexes handle poorly without trigram augmentation.
- Blackbird's sparse ngram approach is optimized for GitHub's scale (billions of documents
  across shards). At local scale, standard trigrams are sufficient and simpler.

### 1.2 Data Structures

The index consists of four primary structures:

```
+--------------------------------------------------+
|                   INDEX                           |
|                                                   |
|  +------------------+  +----------------------+   |
|  | File Metadata    |  | Trigram Index        |   |
|  | Index            |  | (trigram -> postings)|   |
|  +------------------+  +----------------------+   |
|                                                   |
|  +------------------+  +----------------------+   |
|  | Content Store    |  | Symbol Index         |   |
|  | (compressed)     |  | (name -> locations)  |   |
|  +------------------+  +----------------------+   |
+--------------------------------------------------+
```

#### 1.2.1 Trigram Index

The trigram index maps every 3-byte sequence found in source files to the set of
(file_id, byte_offset) pairs where that trigram appears.

**How trigram search works:**

1. **Index build**: For each file, slide a 3-byte window across the content. For each
   trigram, record `(file_id, offset)` in the posting list for that trigram.

2. **Query**: Given a search string like `HttpRequest`, extract trigrams: `Htt`, `ttp`,
   `tpR`, `pRe`, `Req`, `equ`, `que`, `ues`, `est`. Look up posting lists for each
   trigram. Intersect them (AND), filtering to entries where offsets are at correct
   relative distances. The result is a set of candidate (file, offset) pairs.

3. **Verify**: For each candidate, read the actual file content and confirm the match.
   This eliminates false positives from the trigram intersection.

**For regex queries**, the regex is analyzed to extract required literal substrings. For
example, `/Err(.*Error)/` yields trigrams from `Err` and `rror`. The regex `(foo|bar)`
yields `(foo_trigrams OR bar_trigrams)`. The regex engine (regex crate) handles the
candidate → verify step.

**Posting list format:**

Each trigram's posting list is a sorted array of `(file_id: u32, offset: u32)` pairs,
delta-encoded and varint-compressed. For file-level-only queries (where we just need to
know which files contain a trigram, not the position), we maintain a separate
file-level posting list: a sorted array of `file_id: u32` values, also delta-encoded.

Using file-level posting lists for initial filtering is much faster (smaller lists to
intersect), and we only consult positional posting lists when needed for multi-trigram
proximity queries.

**Trigram count math:**

There are at most 256^3 = 16,777,216 possible byte trigrams. In practice, source code
uses a much smaller alphabet. English-language code typically exercises ~100-200 distinct
byte values, yielding roughly 500k-2M distinct trigrams per large repo.

#### 1.2.2 File Metadata Index

A structured table mapping `file_id` to:

| Field | Type | Size |
|---|---|---|
| `file_id` | u32 | 4B |
| `path` | string (offset+len into path pool) | 8B |
| `content_hash` | blake3 (truncated) | 16B |
| `language` | u16 (enum) | 2B |
| `size_bytes` | u32 | 4B |
| `mtime_epoch_secs` | u64 | 8B |
| `line_count` | u32 | 4B |
| `content_offset` | u64 | 8B |
| `content_len` | u32 | 4B |

Total: ~58 bytes per file. For 100k files: ~5.8MB. For 1M files: ~58MB.

The path pool is a contiguous buffer of all file paths, deduplicated by common prefixes
via a trie-compressed path table. Paths are also indexed by trigram for path-based
filtering (`path:src/lib` queries).

Language detection uses the `hyperpolyglot` crate (a Rust port of GitHub's Linguist),
mapping each file to an enum value used for `language:rust` filters.

#### 1.2.3 Content Store

The raw content of every indexed file, compressed with zstd at level 3 (good balance of
speed and ratio for source code, typically achieving 3-5x compression). Content is stored
in a single file with the offset/length recorded in the metadata index.

This is necessary for:
- Verification of trigram candidates (the "verify" step)
- Returning matching lines with context in search results
- Rebuilding the trigram index after corruption or format changes

The content store is optional for queries if the original files are available, but keeping
a copy in the index means queries work even if the working tree has changed since indexing.

**Size estimate:**

| Repo Size | Raw Content | Compressed (~3.5x) | Trigram Index (~0.2x) | Metadata | Total Index |
|---|---|---|---|---|---|
| 1k files, 10MB | 10MB | 3MB | 2MB | 0.06MB | ~5MB |
| 10k files, 100MB | 100MB | 29MB | 20MB | 0.6MB | ~50MB |
| 100k files, 1GB | 1GB | 286MB | 200MB | 5.8MB | ~492MB |
| 1M files, 10GB | 10GB | 2.86GB | 2GB | 58MB | ~4.9GB |

#### 1.2.4 Symbol Index

A secondary index mapping symbol names to `(file_id, line, column, symbol_kind)` tuples.
Symbols are extracted using tree-sitter parsers for each supported language.

Symbol kinds include: function, method, class, struct, enum, interface, type_alias,
constant, variable, module, trait, impl.

The symbol index is itself trigram-indexed (symbol names map to trigrams), enabling
fuzzy and substring symbol search. This is separate from the content trigram index
because symbol searches have different ranking (exact match > prefix match > substring).

Symbol entries:

| Field | Type | Size |
|---|---|---|
| `symbol_name` | string (offset+len) | 8B |
| `file_id` | u32 | 4B |
| `line` | u32 | 4B |
| `column` | u16 | 2B |
| `kind` | u8 | 1B |
| `parent_symbol` | u32 (index, 0 = none) | 4B |

Total: ~23 bytes per symbol. A typical 100k-file repo might have ~2M symbols: ~46MB.

---

## 2. Incremental Reindexing

### 2.1 Change Detection Strategy

**Hybrid approach: git-based primary, file watcher secondary.**

1. **Git-based detection (primary)**: On startup and periodically (every 30s), run
   `git diff --name-status <indexed-commit> HEAD` and `git diff --name-status` (for
   unstaged changes). This gives us the exact set of changed, added, and deleted files
   relative to the last indexed state. This is fast (~10ms for typical diffs),
   reliable, and handles renames via git's rename detection.

   We store the last indexed commit SHA and working-tree content hashes in the index
   header. On the next indexing pass, we diff against these to find what changed.

2. **File watcher (secondary, for live updates)**: Use the `notify` crate with
   `notify-debouncer-full` to watch the repo for filesystem changes. When files change,
   we queue them for reindexing. This gives sub-second latency for updates between
   git-based polls.

   The file watcher is supplementary. If it misses events (which `notify` acknowledges
   can happen under heavy load), the periodic git-based check catches up.

**Why not file watcher only?** The `notify` crate documentation explicitly states that for
large directories, "notify may fail to receive all events." Linux inotify has per-user
watch limits (default ~8192). Git-based detection is 100% reliable and covers the initial
"what changed since last run" case naturally.

**Why not git-based only?** Latency. `git diff` every 100ms is wasteful and still slower
than a file watcher for interactive use. The watcher provides instant feedback.

### 2.2 Debouncing Strategy

File changes are debounced with a 200ms window:

1. File watcher detects a change to `src/main.rs`.
2. A 200ms timer starts for that path.
3. If more changes arrive for `src/main.rs` within 200ms, the timer resets.
4. After 200ms of quiet, `src/main.rs` is queued for reindexing.

For bulk operations (e.g., `git checkout` switching branches), the git-based detector
handles the full diff. The debouncer prevents reindexing the same file 10 times during
an IDE auto-save sequence.

### 2.3 Partial Index Update

The index uses an **append-and-compact** strategy inspired by tantivy's segment model:

1. **Segments**: The index is composed of multiple immutable segments. Each segment
   contains a subset of the indexed files with its own trigram posting lists, metadata,
   and content store.

2. **Update**: When files change, a new segment is created containing only the updated
   files. A "tombstone" bitmap marks the old versions of those files in existing segments
   as deleted.

3. **Query**: At query time, results from all segments are merged, skipping tombstoned
   entries. This is a standard merge-on-read pattern.

4. **Compaction**: Periodically (or when the number of segments exceeds a threshold, e.g.,
   10 segments or tombstone ratio > 30%), segments are merged into a single new segment,
   removing tombstoned entries. This runs in the background.

**Benefits:**
- Updates are fast: only changed files need reindexing.
- No lock contention: readers use the existing segments, the writer creates a new one.
- Crash safety: new segments are written atomically (write to temp file, then rename).

**Handling specific change types:**

- **File modified**: Create new segment with updated content, tombstone old entry.
- **File deleted**: Add to tombstone bitmap, no new segment entry needed.
- **File renamed**: Tombstone old path, create new entry with new path. If content
  unchanged (detected via blake3 hash), we can skip re-extracting trigrams and just
  update the metadata.
- **File created**: Add to new segment.

### 2.4 Consistency Model

The index uses **snapshot isolation** for reads:

- Each query captures a snapshot of the current segment list and tombstone bitmaps at
  query start.
- Writers can create new segments concurrently without affecting in-flight queries.
- The segment list is updated atomically via an `Arc<SegmentList>` swap.
- This is lock-free for readers. Writers acquire a mutex only briefly to publish the
  new segment list.

```rust
struct IndexState {
    /// Current set of segments, atomically swapped on updates.
    segments: Arc<Vec<Arc<Segment>>>,
    /// Commit SHA at time of last full index.
    indexed_commit: Option<String>,
    /// Content hashes for working tree state.
    content_hashes: HashMap<PathBuf, blake3::Hash>,
}
```

---

## 3. Storage Format

### 3.1 On-Disk Layout

The index lives in `.ferret_index/` within the repo root (or a configurable location). The
directory contains:

```
.ferret_index/
  index.meta          # Index header: version, config, last commit SHA
  segments/
    seg_0001/
      meta.bin        # File metadata entries for this segment
      trigrams.bin    # Trigram posting lists
      content.zst     # Zstd-compressed file contents
      symbols.bin     # Symbol index entries
      paths.bin       # Path string pool
      tombstones.bin  # Bitmap of deleted file_ids
    seg_0002/
      ...
  lock                # Advisory lock file for single-writer
```

### 3.2 Trigram Posting List Format

```
[Header]
  magic: u32 = 0x54524947  ("TRIG")
  version: u16
  trigram_count: u32

[Trigram Table]  (sorted by trigram value for binary search)
  For each trigram:
    trigram: [u8; 3]        # The 3-byte trigram
    file_list_offset: u32   # Offset into file posting section
    file_list_len: u32      # Number of file_ids
    pos_list_offset: u32    # Offset into positional posting section
    pos_list_len: u32       # Number of (file_id, offset) pairs

[File Posting Lists]
  For each trigram's file list:
    Delta-encoded, varint-compressed sequence of file_ids.

[Positional Posting Lists]
  For each trigram's positional list:
    Grouped by file_id. For each file:
      file_id: varint
      offset_count: varint
      offsets: delta-encoded varint sequence
```

The trigram table is sorted by trigram value, enabling O(log n) lookup via binary search
on the memory-mapped file. With ~1M distinct trigrams, the table is ~19MB
(1M * (3 + 4 + 4 + 4 + 4) = 19MB).

### 3.3 Memory-Mapped Access

All index files are accessed via `memmap2::Mmap`. This provides:

- **Zero-copy reads**: The OS pages in data on demand. Frequently accessed posting lists
  stay in the page cache.
- **No explicit caching layer**: The OS handles caching, reducing complexity.
- **Startup time**: Opening the index is near-instant (just the mmap call). The OS
  loads pages lazily on first access.
- **Memory efficiency**: Multiple reader processes/threads share the same physical pages.

For the content store, we use `mmap` with `MADV_SEQUENTIAL` during indexing (we read each
file once, linearly) and `MADV_RANDOM` during querying (we jump to specific file offsets).

### 3.4 Format Versioning

The index header contains a format version number. If the on-disk format is incompatible
with the running binary, the index is rebuilt from scratch. We do not attempt format
migration for a v0.x project.

### 3.5 Performance Estimates

**Cold start (first query after reboot, nothing in page cache):**

| Repo Size | Index Size | Cold Start (SSD) | Cold Start (HDD) |
|---|---|---|---|
| 10k files | ~50MB | ~50ms | ~500ms |
| 100k files | ~500MB | ~100ms | ~2s |
| 1M files | ~5GB | ~200ms | ~10s |

Cold start is dominated by reading the trigram table into memory. On SSD, sequential
reads at ~3GB/s make this fast. Subsequent queries benefit from OS page cache.

**Warm query (index in page cache):**

| Query Type | Target | Mechanism |
|---|---|---|
| Literal search | <10ms | Trigram lookup + verify |
| Simple regex | <50ms | Literal extraction + trigram + verify |
| Complex regex (few literals) | <200ms | Broader trigram scan + verify |
| Symbol search | <20ms | Symbol trigram index + direct lookup |
| Path filter | <5ms | Path trigram index lookup |

**Index build time:**

| Repo Size | Full Build (8 threads) | Incremental (100 files changed) |
|---|---|---|
| 10k files | ~2s | ~50ms |
| 100k files | ~20s | ~200ms |
| 1M files | ~3min | ~500ms |

---

## 4. Query Engine

### 4.1 Query Language

The query language mirrors GitHub code search syntax:

```
# Literal search
HttpRequest

# Regex search
/Http(Request|Response)/

# Exact phrase
"fn main()"

# Symbol search
symbol:parse_query

# Path filter
path:src/lib

# Language filter
language:rust

# Combinations
language:rust path:src/ symbol:new /impl.*Display/

# Boolean operators
foo OR bar
foo AND bar     (AND is implicit between terms)
NOT deprecated
```

### 4.2 Query Parsing

The query string is parsed into an AST:

```rust
enum Query {
    Literal(String),
    Regex(String),
    Phrase(String),
    Symbol(String),
    Path(String),
    Language(String),
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
}
```

Parsing uses a simple recursive descent parser. Unquoted space-separated terms are
implicitly ANDed. The `/regex/` syntax distinguishes regex from literal searches.

### 4.3 Query Planning

The query planner converts the AST into an execution plan:

1. **Filter selection**: Each query term maps to an index:
   - `Literal` / `Regex` / `Phrase` -> trigram index
   - `Symbol` -> symbol index
   - `Path` -> path trigram index or metadata scan
   - `Language` -> metadata filter

2. **Cost estimation**: For trigram queries, estimate the posting list sizes for the
   extracted trigrams (stored in the trigram table header). Choose the execution order
   that intersects smaller lists first.

3. **Plan structure**:
   ```
   Filter(language=rust)
     AND Intersect(trigram postings for "HttpReq")
       AND Verify(regex against content)
   ```

   Pre-filters (language, path) are applied first since they reduce the candidate set
   cheaply via bitmap intersection on file_ids.

### 4.4 Execution

```
Query String
    |
    v
[Parser] --> Query AST
    |
    v
[Planner] --> Execution Plan
    |
    v
[Executor]
    |
    +---> [Metadata Filter] (language, path) --> candidate file_ids bitmap
    |
    +---> [Trigram Lookup] --> intersect posting lists --> candidate (file, offset) pairs
    |
    +---> [Candidate Filter] --> apply bitmap intersection to trigram candidates
    |
    +---> [Verify] --> read content, run regex/literal match --> confirmed matches
    |
    +---> [Rank] --> score and sort results
    |
    v
Results (file, line, column, matched text, context)
```

### 4.5 Result Ranking

Results are scored using a weighted combination:

| Signal | Weight | Description |
|---|---|---|
| Match type | 0.3 | Exact > prefix > substring > regex |
| Path depth | 0.15 | Shallower files ranked higher (src/lib.rs > src/a/b/c/d.rs) |
| File name match | 0.15 | Bonus if query appears in filename |
| Symbol match | 0.2 | Bonus if match is in a symbol definition |
| File recency | 0.1 | More recently modified files ranked higher |
| Line position | 0.1 | Matches near top of file ranked higher |

Scores are normalized to [0, 1]. The top N results (default 100) are returned. For
streaming results (fzf interface), results are emitted as found, with reranking in
the client.

---

## 5. Rust Crate Recommendations

### 5.1 Core Dependencies

| Component | Crate | Version | Rationale |
|---|---|---|---|
| **File watching** | `notify` | 8.x | Cross-platform filesystem events (FSEvents/inotify/ReadDirectoryChanges) |
| **Debouncing** | `notify-debouncer-full` | 0.4.x | Full-featured debouncer with rename tracking |
| **Directory walking** | `ignore` | 0.4.x | Respects .gitignore, parallel walking, from ripgrep ecosystem |
| **Parsing (symbols)** | `tree-sitter` | 0.26.x | Incremental parsing, query system for symbol extraction |
| **Language grammars** | `tree-sitter-{rust,javascript,python,go,...}` | latest | Per-language grammars loaded by tree-sitter |
| **Language detection** | `hyperpolyglot` | 0.1.x | Rust port of GitHub's Linguist, classifies files by language |
| **Memory mapping** | `memmap2` | 0.9.x | Cross-platform mmap, safe Rust API |
| **Hashing** | `blake3` | 1.x | Fast content hashing for change detection (SIMD-accelerated) |
| **Compression** | `zstd` | 0.13.x | Fast compression for content store (3-5x ratio on source code) |
| **Regex** | `regex` | 1.x | For query parsing and candidate verification (same engine as ripgrep) |
| **Varint encoding** | `integer-encoding` | 4.x | For delta-encoded posting lists |

### 5.2 Interface Dependencies

| Component | Crate | Version | Rationale |
|---|---|---|---|
| **MCP server** | `rmcp` | 0.1.x | Rust MCP SDK for tool/resource serving |
| **Web server** | `axum` | 0.8.x | Async web framework, tower ecosystem |
| **CLI output** | (stdout) | - | fzf-compatible output is just newline-delimited text to stdout |
| **Async runtime** | `tokio` | 1.x | Required by axum, used for file watcher integration |
| **Serialization** | `serde` + `serde_json` | 1.x | JSON output for MCP and web API responses |

### 5.3 Custom Implementation Required

The following components do not have suitable off-the-shelf crates and need custom code:

- **Trigram index builder and reader**: The core indexing data structure. While tantivy
  provides a full-text index, it does not natively support byte-level trigram indexing
  with positional posting lists. Building a custom trigram index is straightforward
  (~500-1000 lines) and gives us full control over the on-disk format.

- **Query parser**: The code search query language is domain-specific. A simple recursive
  descent parser (~200-300 lines) is preferable to pulling in a parser combinator library.

- **Segment manager**: The append-and-compact segment lifecycle (creation, tombstoning,
  merging) is specific to our index format.

- **Result ranking**: The scoring model is application-specific.

### 5.4 Why Not tantivy?

tantivy is a full-text search engine optimized for natural language (tokenization,
stemming, BM25 scoring). Code search has fundamentally different requirements:

1. **Substring matching**: `HttpReq` must find `HttpRequest`. tantivy tokenizes on word
   boundaries, so `HttpReq` would not match `HttpRequest` without custom tokenization.
2. **Punctuation sensitivity**: `fn()` and `fn` are different in code. tantivy's default
   tokenizers strip punctuation.
3. **No stemming**: `running` should NOT match `run` in code search.
4. **Byte-level regex**: Code search needs regex over raw bytes, not over tokenized terms.

tantivy could be adapted with a trigram tokenizer, but at that point we've lost most of
its value and taken on a large dependency. A purpose-built trigram index is simpler and
more efficient for our use case.

---

## 6. Architecture Diagram

```
                              ferret Architecture

  +------------------------------------------------------------------+
  |                        FILE CHANGE DETECTION                      |
  |                                                                    |
  |   +------------------+       +---------------------------+        |
  |   | notify           |       | git diff                  |        |
  |   | (file watcher)   |       | (periodic, 30s interval)  |        |
  |   +--------+---------+       +-------------+-------------+        |
  |            |                               |                      |
  |            v                               v                      |
  |   +------------------+       +---------------------------+        |
  |   | Debouncer        |       | Change Detector           |        |
  |   | (200ms window)   +------>| (union of both sources)   |        |
  |   +------------------+       +-------------+-------------+        |
  +-------------------------------------------|----------------------+
                                              |
                                    changed file list
                                              |
                                              v
  +------------------------------------------------------------------+
  |                           INDEXER                                  |
  |                                                                    |
  |   +------------------+  +------------------+  +----------------+  |
  |   | Content Reader   |  | Trigram Extractor|  | tree-sitter    |  |
  |   | (ignore crate,   |  | (3-byte sliding  |  | Symbol         |  |
  |   |  .gitignore)     |  |  window)         |  | Extractor      |  |
  |   +--------+---------+  +--------+---------+  +--------+-------+  |
  |            |                      |                     |          |
  |            v                      v                     v          |
  |   +------------------+  +------------------+  +----------------+  |
  |   | Content Store    |  | Trigram Index     |  | Symbol Index   |  |
  |   | (zstd compress)  |  | (posting lists)  |  | (name -> loc)  |  |
  |   +------------------+  +------------------+  +----------------+  |
  |            |                      |                     |          |
  |            +----------------------+---------------------+          |
  |                                   |                                |
  |                          +--------v---------+                      |
  |                          | Segment Writer   |                      |
  |                          | (atomic create)  |                      |
  |                          +--------+---------+                      |
  |                                   |                                |
  |                          +--------v---------+                      |
  |                          | Segment Manager  |                      |
  |                          | (compact/merge)  |                      |
  |                          +------------------+                      |
  +------------------------------------------------------------------+
                                              |
                                     writes to disk
                                              |
                                              v
  +------------------------------------------------------------------+
  |                     STORAGE (.ferret_index/)                            |
  |                                                                    |
  |   segments/seg_NNNN/                                              |
  |     meta.bin | trigrams.bin | content.zst | symbols.bin           |
  |                                                                    |
  |   All files memory-mapped via memmap2                             |
  +------------------------------------------------------------------+
                                              ^
                                      reads from disk
                                              |
  +------------------------------------------------------------------+
  |                        QUERY ENGINE                               |
  |                                                                    |
  |   +------------------+                                            |
  |   | Query Parser     |  "language:rust /impl.*Display/"           |
  |   | (recursive       |                                            |
  |   |  descent)        |                                            |
  |   +--------+---------+                                            |
  |            |                                                      |
  |            v                                                      |
  |   +------------------+                                            |
  |   | Query Planner    |  Choose indexes, order intersections       |
  |   +--------+---------+                                            |
  |            |                                                      |
  |            v                                                      |
  |   +------------------+  +------------------+  +----------------+  |
  |   | Metadata Filter  |  | Trigram Lookup   |  | Symbol Lookup  |  |
  |   | (lang, path)     |  | (intersect       |  | (trigram on    |  |
  |   |                  |  |  posting lists)  |  |  names)        |  |
  |   +--------+---------+  +--------+---------+  +--------+-------+  |
  |            |                      |                     |          |
  |            +----------------------+---------------------+          |
  |                                   |                                |
  |                          +--------v---------+                      |
  |                          | Candidate Verify |                      |
  |                          | (regex on content)|                     |
  |                          +--------+---------+                      |
  |                                   |                                |
  |                          +--------v---------+                      |
  |                          | Ranker / Scorer  |                      |
  |                          +--------+---------+                      |
  |                                   |                                |
  +-----------------------------------|------------------------------+
                                      |
                               ranked results
                                      |
               +----------------------+----------------------+
               |                      |                      |
               v                      v                      v
  +------------------+  +------------------+  +------------------+
  |   MCP Server     |  |   Web Server     |  |   CLI (fzf)      |
  |   (rmcp)         |  |   (axum)         |  |   (stdout)       |
  |                  |  |                  |  |                  |
  |   Tools:         |  |   GET /search    |  |   ferret search |
  |   - search       |  |   GET /symbols   |  |   "query"        |
  |   - symbols      |  |   GET /file      |  |   | fzf          |
  |   - file_content |  |   WebSocket for  |  |                  |
  |                  |  |   live results   |  |                  |
  +------------------+  +------------------+  +------------------+
```

### Data Flow Summary

1. **Change detection** identifies modified files via notify + git diff.
2. **Indexer** reads file content, extracts trigrams and symbols, compresses content.
3. **Segment writer** creates a new immutable segment atomically.
4. **Segment manager** compacts segments in the background.
5. **Query engine** parses the query, plans execution across indexes, fetches candidates
   from trigram posting lists, verifies against actual content, ranks results.
6. **Interfaces** (MCP, web, CLI) present results to users.

---

## Appendix A: Trigram Index Worked Example

Given two files:

**File 0** (`main.rs`): `fn main() {}`
**File 1** (`lib.rs`): `fn parse() {}`

Trigrams extracted:

| Trigram | File Posting List | Positional Posting List |
|---|---|---|
| `fn ` | [0, 1] | [(0, 0), (1, 0)] |
| `n m` | [0] | [(0, 1)] |
| ` ma` | [0] | [(0, 2)] |
| `mai` | [0] | [(0, 3)] |
| `ain` | [0] | [(0, 4)] |
| `in(` | [0] | [(0, 5)] |
| `n()` | [0, 1] | [(0, 6), (1, 8)] |
| `() ` | [0, 1] | [(0, 7), (1, 9)] |
| `) {` | [0, 1] | [(0, 8), (1, 10)] |
| ` {}` | [0, 1] | [(0, 9), (1, 11)] |
| `n p` | [1] | [(1, 1)] |
| ` pa` | [1] | [(1, 2)] |
| `par` | [1] | [(1, 3)] |
| `ars` | [1] | [(1, 4)] |
| `rse` | [1] | [(1, 5)] |
| `se(` | [1] | [(1, 6)] |
| `e()` | [1] | [(1, 7)] |

**Query**: `parse`
**Trigrams**: `par AND ars AND rse`
**File posting intersection**: [1] AND [1] AND [1] = [1]
**Verify**: Read file 1, confirm `parse` appears. Match at offset 3.
**Result**: `lib.rs:1:4: fn parse() {}`

## Appendix B: Key Design Decisions Log

| Decision | Choice | Alternatives Considered | Rationale |
|---|---|---|---|
| Primary index type | Trigram | Suffix array, full-text | Best space/speed/complexity balance for local use |
| Storage backend | Custom binary + mmap | SQLite, sled, tantivy | Lower overhead, full control over format, mmap gives OS-level caching |
| Change detection | Hybrid git + notify | notify only, polling | Git is reliable for full diff, notify adds low-latency updates |
| Content hashing | blake3 | sha256, xxhash | Fastest cryptographic hash (SIMD), used for content dedup |
| Compression | zstd level 3 | lz4, snappy, gzip | Best ratio/speed tradeoff for source code |
| Concurrency model | Segments + Arc swap | RwLock, single-threaded | Lock-free reads, simple writer exclusion |
| Symbol extraction | tree-sitter | ctags, regex-based | Incremental parsing, accurate ASTs, good Rust bindings |
| Language detection | hyperpolyglot | manual extension map | Comprehensive, matches GitHub's Linguist output |
