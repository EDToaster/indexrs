# MCP Server Interface Design

## Overview

ferret exposes its code index to AI assistants via the [Model Context Protocol](https://modelcontextprotocol.io). The MCP interface is the primary way LLMs interact with the index -- it must be optimized for token efficiency, discoverability, and practical usefulness in coding workflows.

Transport: stdio (for local integration with Claude Code, Cursor, etc.)

---

## 1. MCP Tools

### 1.1 `search_code`

Full-text and regex search across indexed file contents. This is the primary workhorse tool.

```json
{
  "name": "search_code",
  "title": "Search Code",
  "description": "Search file contents across indexed repositories. Supports literal strings, regex patterns (surrounded in /slashes/), and boolean operators (AND, OR, NOT). Results include matching lines with surrounding context.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "Search query. Literal text by default. Wrap in /slashes/ for regex. Supports AND, OR, NOT operators and \"exact phrase\" matching."
      },
      "path": {
        "type": "string",
        "description": "Filter by file path glob pattern. Examples: '*.rs', 'src/**/*.ts', 'tests/'"
      },
      "language": {
        "type": "string",
        "description": "Filter by programming language. Examples: 'rust', 'python', 'typescript'"
      },
      "repo": {
        "type": "string",
        "description": "Filter to a specific indexed repository by name or path."
      },
      "case_sensitive": {
        "type": "boolean",
        "description": "Whether the search is case-sensitive. Default: false."
      },
      "context_lines": {
        "type": "integer",
        "description": "Number of lines of context to show before and after each match. Default: 2. Max: 10.",
        "minimum": 0,
        "maximum": 10
      },
      "max_results": {
        "type": "integer",
        "description": "Maximum number of matching files to return. Default: 20. Max: 100.",
        "minimum": 1,
        "maximum": 100
      },
      "offset": {
        "type": "integer",
        "description": "Skip this many matching files (for pagination). Default: 0.",
        "minimum": 0
      }
    },
    "required": ["query"]
  },
  "annotations": {
    "readOnlyHint": true,
    "openWorldHint": false
  }
}
```

**Response format** (returned as `text` content):

```
Found 47 matches across 12 files (showing 1-12)

## src/index/builder.rs
L42:   fn build_trigram_index(&mut self, content: &str) -> TrigramIndex {
L43:       let mut index = TrigramIndex::new();
L44:*      for trigram in content.trigrams() {
L45:           index.insert(trigram, self.current_doc_id);
L46:       }

L128:* fn trigrams(&self) -> impl Iterator<Item = Trigram> + '_ {
L129:      self.as_bytes().windows(3).map(|w| Trigram::from_bytes(w))
L130:  }

## src/search/engine.rs
L15:   use crate::index::TrigramIndex;
L16:*  pub fn search_trigrams(index: &TrigramIndex, query: &str) -> Vec<DocId> {
L17:       let query_trigrams: Vec<_> = query.trigrams().collect();
```

Lines marked with `*` are matches. File paths are relative to the repository root. The header line reports total matches for pagination awareness.

### 1.2 `search_symbols`

Search for symbol definitions (functions, types, structs, classes, interfaces, constants).

```json
{
  "name": "search_symbols",
  "title": "Search Symbols",
  "description": "Search for symbol definitions across indexed repositories. Finds functions, types, structs, classes, interfaces, traits, constants, and other named definitions. Faster and more precise than full-text search when you know you're looking for a definition.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "Symbol name or pattern to search for. Supports prefix matching (e.g., 'build_' finds 'build_index', 'build_trigram')."
      },
      "kind": {
        "type": "string",
        "enum": ["function", "type", "struct", "class", "interface", "trait", "constant", "method", "module", "enum", "variable"],
        "description": "Filter by symbol kind."
      },
      "path": {
        "type": "string",
        "description": "Filter by file path glob pattern."
      },
      "language": {
        "type": "string",
        "description": "Filter by programming language."
      },
      "repo": {
        "type": "string",
        "description": "Filter to a specific indexed repository."
      },
      "max_results": {
        "type": "integer",
        "description": "Maximum number of symbols to return. Default: 20. Max: 100.",
        "minimum": 1,
        "maximum": 100
      },
      "offset": {
        "type": "integer",
        "description": "Skip this many results (for pagination). Default: 0.",
        "minimum": 0
      }
    },
    "required": ["query"]
  },
  "annotations": {
    "readOnlyHint": true,
    "openWorldHint": false
  }
}
```

**Response format:**

```
Found 8 symbols matching "TrigramIndex"

## struct TrigramIndex
   src/index/trigram.rs:15
   pub struct TrigramIndex {
       map: HashMap<Trigram, RoaringBitmap>,
       doc_count: usize,
   }

## impl TrigramIndex (6 methods)
   src/index/trigram.rs:22
   - fn new() -> Self                           :22
   - fn insert(&mut self, trigram: Trigram, ...)  :30
   - fn search(&self, trigrams: &[Trigram]) -> .. :45
   - fn merge(&mut self, other: &TrigramIndex)   :62
   - fn doc_count(&self) -> usize                :78
   - fn memory_usage(&self) -> usize             :82

## fn build_trigram_index
   src/index/builder.rs:42
   fn build_trigram_index(&mut self, content: &str) -> TrigramIndex {
```

### 1.3 `search_files`

Search for files by name/path pattern. Useful for navigating project structure.

```json
{
  "name": "search_files",
  "title": "Search Files",
  "description": "Search for files by name or path pattern across indexed repositories. Returns file paths with basic metadata (language, size). Useful for finding files when you know part of the name but not the location.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "File name or path pattern. Supports glob patterns (e.g., '*.rs', 'test_*.py') and substring matching."
      },
      "language": {
        "type": "string",
        "description": "Filter by programming language."
      },
      "repo": {
        "type": "string",
        "description": "Filter to a specific indexed repository."
      },
      "max_results": {
        "type": "integer",
        "description": "Maximum number of files to return. Default: 30. Max: 200.",
        "minimum": 1,
        "maximum": 200
      },
      "offset": {
        "type": "integer",
        "description": "Skip this many results (for pagination). Default: 0.",
        "minimum": 0
      }
    },
    "required": ["query"]
  },
  "annotations": {
    "readOnlyHint": true,
    "openWorldHint": false
  }
}
```

**Response format:**

```
Found 23 files matching "config"

src/config.rs                    (Rust, 2.1 KB)
src/config/mod.rs                (Rust, 450 B)
src/config/indexing.rs           (Rust, 1.8 KB)
src/config/search.rs             (Rust, 920 B)
tests/config_test.rs             (Rust, 3.4 KB)
.cargo/config.toml               (TOML, 120 B)
```

### 1.4 `get_file`

Read a specific file's contents from the index. The index stores file content, so this avoids filesystem access and works even if the file has been modified since last index.

```json
{
  "name": "get_file",
  "title": "Get File Contents",
  "description": "Read the contents of an indexed file. Returns the file as it was at the time of last indexing. Supports reading a range of lines to avoid returning excessively large files.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "path": {
        "type": "string",
        "description": "File path relative to the repository root."
      },
      "repo": {
        "type": "string",
        "description": "Repository name or path. Required if multiple repositories are indexed."
      },
      "start_line": {
        "type": "integer",
        "description": "First line to return (1-indexed). Default: 1.",
        "minimum": 1
      },
      "end_line": {
        "type": "integer",
        "description": "Last line to return (inclusive). Default: end of file, capped at 500 lines from start_line.",
        "minimum": 1
      }
    },
    "required": ["path"]
  },
  "annotations": {
    "readOnlyHint": true,
    "openWorldHint": false
  }
}
```

**Response format:**

```
src/index/trigram.rs (lines 1-85 of 142, Rust, indexed 2m ago)

  1 | use std::collections::HashMap;
  2 | use roaring::RoaringBitmap;
  3 |
  4 | #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
  5 | pub struct Trigram([u8; 3]);
...
 85 | }
```

If the file exceeds 500 lines from `start_line`, the response is truncated with a note:

```
(truncated at line 500 -- use start_line/end_line to read more)
```

### 1.5 `index_status`

Report on the current state of the index.

```json
{
  "name": "index_status",
  "title": "Index Status",
  "description": "Get the current status of the ferret service, including which repositories are indexed, how many files and symbols are tracked, last index time, and whether reindexing is in progress.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "repo": {
        "type": "string",
        "description": "Get detailed status for a specific repository. Omit for an overview of all repositories."
      }
    }
  },
  "annotations": {
    "readOnlyHint": true,
    "openWorldHint": false
  }
}
```

**Response format (overview):**

```
ferret status: healthy
Uptime: 4h 32m

Repositories:
  ferret       /Users/howard/src/ferret      12 files    248 symbols   indexed 2m ago
  myproject     /Users/howard/src/myproject   1,847 files  42,310 symbols indexed 5m ago

Total: 2 repos, 1,859 files, 42,558 symbols
Index size: 24.5 MB (memory), 18.2 MB (disk)
```

**Response format (detailed, single repo):**

```
Repository: myproject
Path: /Users/howard/src/myproject
Last indexed: 2026-02-27T14:32:15Z (5 minutes ago)
Index duration: 3.2s

Files:     1,847 indexed / 42 excluded
Symbols:   42,310
Languages: TypeScript (1,204), JavaScript (312), JSON (180), CSS (94), Markdown (57)

Excluded patterns: node_modules/**, dist/**, *.min.js
Watched: yes (inotify)
Pending changes: 3 files
```

### 1.6 `reindex`

Trigger reindexing of a repository.

```json
{
  "name": "reindex",
  "title": "Trigger Reindex",
  "description": "Trigger reindexing of a repository. By default performs an incremental reindex (only changed files). Use full=true to rebuild the entire index from scratch.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "repo": {
        "type": "string",
        "description": "Repository name or path to reindex. Required if multiple repositories are indexed."
      },
      "full": {
        "type": "boolean",
        "description": "If true, rebuild the entire index from scratch instead of incrementally. Default: false."
      }
    }
  },
  "annotations": {
    "readOnlyHint": false,
    "destructiveHint": false,
    "idempotentHint": true,
    "openWorldHint": false
  }
}
```

**Response format:**

```
Reindex started for myproject (incremental)
Changed files detected: 3
Estimated time: <1s
```

Or, if already in progress:

```
Reindex already in progress for myproject (started 2s ago, 45% complete)
```

---

## 2. MCP Resources

Resources provide direct access to indexed data via URI. These are useful for LLMs that want to pull in context proactively.

### 2.1 Static Resources

| URI | Description |
|-----|-------------|
| `ferret://status` | Overall index status (same as `index_status` tool with no args) |

### 2.2 Resource Templates

| URI Template | Description | MIME Type |
|---|---|---|
| `ferret://repo/{repo}/file/{path}` | Contents of a specific indexed file | Detected per-file (e.g., `text/x-rust`) |
| `ferret://repo/{repo}/tree` | Directory tree listing of a repository | `text/plain` |
| `ferret://repo/{repo}/symbols` | All symbols in a repository (outline) | `text/plain` |
| `ferret://repo/{repo}/status` | Detailed status of a specific repository | `text/plain` |

**Resource template definitions:**

```json
[
  {
    "uriTemplate": "ferret://repo/{repo}/file/{+path}",
    "name": "Indexed File",
    "description": "Contents of a file as stored in the index. The path is relative to the repository root.",
    "mimeType": "text/plain"
  },
  {
    "uriTemplate": "ferret://repo/{repo}/tree",
    "name": "Repository Tree",
    "description": "Directory tree listing of all indexed files in a repository. Useful for understanding project structure.",
    "mimeType": "text/plain"
  },
  {
    "uriTemplate": "ferret://repo/{repo}/symbols",
    "name": "Repository Symbols",
    "description": "Outline of all symbols (functions, types, constants, etc.) in a repository, grouped by file.",
    "mimeType": "text/plain"
  },
  {
    "uriTemplate": "ferret://repo/{repo}/status",
    "name": "Repository Index Status",
    "description": "Detailed indexing status for a specific repository.",
    "mimeType": "text/plain"
  }
]
```

**Example: `ferret://repo/myproject/tree` response:**

```
myproject/ (1,847 files)
  src/
    config/
      mod.rs
      indexing.rs
      search.rs
    index/
      mod.rs
      builder.rs
      trigram.rs
    search/
      mod.rs
      engine.rs
      query.rs
    main.rs
    lib.rs
  tests/
    integration/
      search_test.rs
    config_test.rs
  Cargo.toml
  Cargo.lock
```

**Example: `ferret://repo/myproject/symbols` response:**

```
myproject symbols (42,310 total)

## src/index/trigram.rs
  struct Trigram                     :5
  struct TrigramIndex                :15
  fn TrigramIndex::new               :22
  fn TrigramIndex::insert            :30
  fn TrigramIndex::search            :45
  fn TrigramIndex::merge             :62

## src/index/builder.rs
  struct IndexBuilder                :8
  fn IndexBuilder::new               :18
  fn IndexBuilder::build_trigram_index :42
  fn IndexBuilder::index_file        :55
...
```

---

## 3. Result Format Design

### Principles

1. **Text-first.** All responses use `text` content type. No JSON blobs that waste tokens on structural syntax. LLMs parse plain text more naturally.

2. **File-grouped.** Search results are grouped by file with the file path as a section header. This matches how developers think about code.

3. **Match highlighting.** Matching lines are marked with `*` in the gutter. This is cheaper than wrapping matches in `**bold**` markers that consume more tokens.

4. **Line numbers always.** Every code line includes its line number. This enables the LLM to reference specific locations and use `get_file` with `start_line`/`end_line` for more context.

5. **Metadata in headers.** File metadata (language, size, staleness) appears in headers, not inline with code.

6. **Summary first.** Every response starts with a one-line summary (e.g., "Found 47 matches across 12 files") so the LLM can assess whether to paginate or refine the query.

### Token Efficiency

A typical search result for 20 files with 2 context lines per match uses roughly 1,500-3,000 tokens. This fits comfortably within a single tool response without dominating the context window.

The `context_lines` parameter lets the LLM control the tradeoff: use 0 for a compact overview, or increase to 5-10 when investigating specific matches.

The `max_results` default of 20 is deliberately conservative. An LLM can always paginate with `offset` or increase `max_results` if needed, but we avoid flooding the context window by default.

---

## 4. Pagination & Limits

### Approach: Offset-based pagination

Every search tool accepts `max_results` (page size) and `offset` (skip count). The response header always reports total matches so the LLM knows whether more results exist.

```
Found 47 matches across 12 files (showing 1-12)
```

vs.

```
Found 47 matches across 12 files (showing 13-20, offset=12)
```

### Default Limits

| Parameter | Default | Maximum |
|-----------|---------|---------|
| `search_code.max_results` | 20 files | 100 |
| `search_code.context_lines` | 2 | 10 |
| `search_symbols.max_results` | 20 | 100 |
| `search_files.max_results` | 30 | 200 |
| `get_file` line range | 500 lines | 500 |

### Why Not Cursor-based Pagination?

Offset-based is simpler and sufficient for a local index. The index is not changing between paginated requests (or if it is, slight inconsistency is acceptable). Cursor-based pagination adds protocol complexity without meaningful benefit here.

### Large Result Guidance

When a query returns a very large number of matches (>100 files), the response includes a hint:

```
Found 2,341 matches across 487 files (showing 1-20)
Tip: Consider narrowing with path:, language:, or a more specific query.
```

---

## 5. Error Handling

Errors are returned using MCP's `isError: true` response field with a human-readable message.

### Error Categories

| Error | When | Response |
|---|---|---|
| `repository_not_found` | `repo` parameter doesn't match any indexed repository | `Error: Repository "foo" not found. Indexed repositories: ferret, myproject` |
| `file_not_found` | `get_file` path doesn't exist in the index | `Error: File "src/missing.rs" not found in repository "myproject". Did you mean "src/main.rs"?` |
| `invalid_query` | Malformed regex, unbalanced parens, empty query | `Error: Invalid regex in query: unmatched '(' at position 5` |
| `invalid_parameter` | Out-of-range parameter value | `Error: context_lines must be between 0 and 10, got 25` |
| `index_stale` | Index is significantly out of date | Result returned normally, but with a warning header: `Warning: Index for "myproject" is 2 hours old. Run reindex to update.` |
| `index_building` | Index is currently being built, no results available yet | `Error: Index for "myproject" is currently being built (35% complete). Try again shortly.` |
| `no_results` | Query returned zero matches | `No matches found for "foobar" across 1,859 indexed files. Suggestions: check spelling, try a broader query, or remove filters.` |

### Staleness Warnings

When results are returned but the index is more than 10 minutes old (and file watching is enabled), a warning is prepended:

```
Warning: Index is 15 minutes stale. 7 file changes pending. Consider running reindex.

Found 12 matches across 4 files (showing 1-4)
...
```

This is a warning, not an error -- results are still returned.

---

## 6. Example Interactions

### Example 1: Finding and Understanding a Function

An LLM is asked "how does the trigram search work in ferret?"

**Step 1:** Search for the relevant code.

```
Tool: search_symbols
Arguments: { "query": "trigram", "repo": "ferret" }
```

Response:
```
Found 5 symbols matching "trigram"

## struct Trigram
   src/index/trigram.rs:5
   pub struct Trigram([u8; 3]);

## struct TrigramIndex
   src/index/trigram.rs:15
   pub struct TrigramIndex { ... }

## fn TrigramIndex::search
   src/index/trigram.rs:45
   pub fn search(&self, trigrams: &[Trigram]) -> RoaringBitmap { ... }

## fn build_trigram_index
   src/index/builder.rs:42
   fn build_trigram_index(&mut self, content: &str) -> TrigramIndex { ... }

## fn search_trigrams
   src/search/engine.rs:16
   pub fn search_trigrams(index: &TrigramIndex, query: &str) -> Vec<DocId> { ... }
```

**Step 2:** Read the search implementation.

```
Tool: get_file
Arguments: { "path": "src/search/engine.rs", "repo": "ferret", "start_line": 10, "end_line": 60 }
```

Response:
```
src/search/engine.rs (lines 10-60 of 142, Rust, indexed 2m ago)

 10 | use crate::index::{TrigramIndex, Trigram};
 11 |
 12 | /// Search the trigram index for documents matching the query.
 13 | /// Returns document IDs sorted by relevance.
 14 | pub fn search_trigrams(index: &TrigramIndex, query: &str) -> Vec<DocId> {
 15 |     let query_trigrams: Vec<Trigram> = query.trigrams().collect();
 16 |     if query_trigrams.is_empty() {
 17 |         return vec![];
 18 |     }
 19 |     // Intersect posting lists for all trigrams
 20 |     let mut result = index.search(&query_trigrams[0..1]);
...
```

The LLM now has enough context to explain the trigram search algorithm.

### Example 2: Finding All Error Handling Patterns

An LLM is doing a code review and wants to check error handling consistency.

```
Tool: search_code
Arguments: {
  "query": "/\\.unwrap\\(\\)/",
  "language": "rust",
  "repo": "ferret",
  "context_lines": 1,
  "max_results": 50
}
```

Response:
```
Found 23 matches across 8 files (showing 1-8)

## src/index/builder.rs
L34:   let metadata = fs::metadata(&path);
L35:*  let content = fs::read_to_string(&path).unwrap();
L36:   let trigrams = self.build_trigram_index(&content);

L89:   let file = File::open(&path);
L90:*  let reader = BufReader::new(file.unwrap());
L91:   for line in reader.lines() {

## src/config.rs
L12:*  let config: Config = toml::from_str(&contents).unwrap();
...
```

### Example 3: Navigating an Unfamiliar Codebase

An LLM is asked to add a feature to a project it hasn't seen before.

**Step 1:** Understand the project structure.

```
Tool: index_status
Arguments: { "repo": "myproject" }
```

Response:
```
Repository: myproject
Path: /Users/howard/src/myproject
Last indexed: 2026-02-27T14:32:15Z (5 minutes ago)

Files:     1,847 indexed / 42 excluded
Symbols:   42,310
Languages: TypeScript (1,204), JavaScript (312), JSON (180), CSS (94), Markdown (57)
```

**Step 2:** Browse the directory tree.

```
Tool: search_files
Arguments: { "query": "src/", "repo": "myproject", "max_results": 50 }
```

**Step 3:** Search for the relevant module.

```
Tool: search_code
Arguments: { "query": "authentication", "repo": "myproject", "path": "src/**" }
```

**Step 4:** Drill into the specific file.

```
Tool: get_file
Arguments: { "path": "src/auth/middleware.ts", "repo": "myproject" }
```

### Example 4: Refining a Broad Search

An LLM searches for something generic and gets too many results.

**Step 1:** Initial broad search.

```
Tool: search_code
Arguments: { "query": "config" }
```

Response:
```
Found 2,341 matches across 487 files (showing 1-20)
Tip: Consider narrowing with path:, language:, or a more specific query.

## src/config.rs
L1:*  //! Configuration loading and validation
L2:   use serde::Deserialize;
...
```

**Step 2:** Narrow it down.

```
Tool: search_code
Arguments: {
  "query": "config",
  "path": "src/config*",
  "language": "rust",
  "max_results": 50
}
```

Response:
```
Found 34 matches across 4 files (showing 1-4)
...
```

The LLM progressively narrows the search to find exactly what it needs.

---

## 7. Design Decisions & Rationale

### Why plain text over structured JSON responses?

LLMs process natural text more efficiently than JSON. A search result in JSON requires structural tokens (`{`, `}`, `"key":`, etc.) that consume context without adding semantic value. Plain text with lightweight formatting (line numbers, `##` headers, `*` markers) conveys the same information in roughly 40% fewer tokens.

However, the tools do define `inputSchema` precisely so that LLMs can construct valid requests reliably.

### Why separate `search_code`, `search_symbols`, and `search_files`?

A single "search everything" tool would require the LLM to parse mixed result types and would make parameters confusing (symbol `kind` doesn't apply to content search). Separate tools with focused parameters give the LLM clear signals about which tool to use and reduce parameter confusion.

GitHub code search uses qualifiers (`symbol:`, `path:`, `content:`) within a single search box, but that's a UI convention for humans. For an MCP interface, distinct tools are more discoverable and self-documenting.

### Why `get_file` instead of just using resources?

Both are provided. Resources (`ferret://repo/{repo}/file/{+path}`) work well for proactive context loading. The `get_file` tool adds line-range support, which is important for large files -- an LLM can request just lines 40-80 after seeing a search result pointing to line 44.

### Why offset pagination over cursor-based?

For a local, mostly-static index, offset pagination is simpler and sufficient. The LLM can reason about offsets ("I've seen 20 results, let me get the next 20") more naturally than opaque cursors. The index doesn't change between paginated requests in practice.

---

## 8. Future Considerations

These are explicitly out of scope for v1 but worth noting:

- **Semantic search.** Embedding-based search for "find code that does X" queries. Would be a separate tool (`search_semantic`) to keep the interface clean.
- **Cross-reference.** "Find all callers of function X" or "find all implementations of trait Y." Requires deeper language analysis than trigram indexing.
- **Diff search.** Search across git diffs ("what changed related to authentication in the last week").
- **MCP Prompts.** Predefined prompt templates (e.g., "code review this file", "explain this module") that compose tool calls. Deferred until real usage patterns emerge.
