# Tree-sitter highlighting: replace syntect with unified tree-sitter processing

## Problem

Syntax highlight tokenization via syntect is the bottleneck in the indexing pipeline. Syntect uses TextMate grammars that run regex matching per line — O(lines × regex_complexity) — causing Phase 1 of segment building to take minutes on large repos. Meanwhile, tree-sitter is already used for symbol extraction (behind the `symbols` feature) and parses entire files in a single O(n) pass via compiled LR parsers (C code).

Both symbol extraction and highlighting require a full parse of every file. Currently these are two independent operations using different parsers (tree-sitter for symbols, syntect for highlights), meaning each file is parsed twice with the slower parser used for the more expensive task.

## Design

### Architecture: parse once, query twice

Replace syntect with raw tree-sitter queries. Parse each file once and run both highlight and symbol queries against the same AST:

```
content → tree-sitter parse → Tree ─┬→ highlights.scm query → Vec<Vec<Token>>
                                     └→ symbols query        → Vec<SymbolEntry>
```

No `tree-sitter-highlight` crate dependency. The `tree-sitter-highlight` crate owns its parser internally and doesn't accept a pre-parsed `Tree`, so it can't share the parse with symbol extraction. Instead, we use `tree_sitter::QueryCursor` directly against the parsed tree for both passes.

### Highlight query loading

Each tree-sitter grammar crate ships a `queries/highlights.scm` file. These are embedded at compile time via `include_str!` and compiled into `tree_sitter::Query` objects.

Highlight queries return captures like `@keyword`, `@string`, `@comment`, etc. We map capture names to our existing 16-category `TokenKind` enum:

| Capture name | TokenKind |
|---|---|
| `keyword`, `keyword.*` | Keyword |
| `string`, `string.*` | String |
| `comment`, `comment.*` | Comment |
| `number` | Number |
| `function`, `function.*` | Function |
| `type`, `type.*` | Type |
| `variable`, `variable.*`, `property` | Variable |
| `operator`, `keyword.operator` | Operator |
| `punctuation`, `punctuation.*` | Punctuation |
| `attribute` | Attribute |
| `constant`, `constant.*` | Constant |
| `module` | Module |
| `label` | Label |
| `constructor` | Type |
| (unmatched) | Plain |

### Converting query captures to `Vec<Vec<Token>>`

1. Collect all `(byte_start, byte_end, TokenKind)` spans from query matches
2. Sort by byte offset
3. Split into per-line token lists, filling gaps with `TokenKind::Plain`
4. Output is identical to what syntect produces today — downstream is unchanged

### LanguageConfig

A unified per-language config struct replaces the separate `tree_sitter_language()` and `compiled_queries()` caches:

```rust
pub struct LanguageConfig {
    pub ts_language: tree_sitter::Language,
    pub highlight_query: tree_sitter::Query,
    pub symbol_query: Option<tree_sitter::Query>,
}
```

Cached in `OnceLock<HashMap<Language, LanguageConfig>>`. Thread-local `Parser` cache (one per language per thread) reuses the existing pattern from `symbol_extractor.rs`.

### Unified entry point

```rust
pub struct TreeSitterResults {
    pub highlights: Option<Vec<Vec<Token>>>,
    pub symbols: Vec<SymbolEntry>,
}

pub fn process_file(content: &[u8], language: Language, file_id: FileId) -> TreeSitterResults
```

Called from `segment.rs` Phase 1 instead of separate `tokenize_file()` + `extract_symbols()` calls.

### Language coverage

9 languages via 7 existing grammar crates: Rust, Python, TypeScript, JavaScript, Go, C, C++, Ruby, Java. Unsupported languages return `None` highlights and empty symbols (same as today). More grammars can be added incrementally.

### Feature gating

Everything behind the existing `symbols` feature flag. When disabled, `process_file()` returns `None` highlights and empty symbols.

## Changes

### New code

- `tree_sitter_process.rs` — `process_file()`, highlight query runner, `TreeSitterResults`, capture-to-TokenKind mapping

### Modified code

- `grammar.rs` — load `highlights.scm` per language, build `LanguageConfig` with both highlight and symbol queries
- `segment.rs` — Phase 1 calls `process_file()` instead of separate `tokenize_file()` + `extract_symbols()`
- `highlight.rs` — remove all syntect code; keep `TokenKind`, `Token`, `FileHighlight`, RLE encode/decode, `HighlightStoreWriter`/`HighlightStoreReader`
- `symbol_extractor.rs` — refactor to accept a `&Tree` instead of parsing internally; move symbol query definitions into `grammar.rs`
- `Cargo.toml` — remove `syntect` dependency

### Deleted code

- `syntect` dependency
- `SYNTAX_SET`, `SCOPE_PREFIXES`, `ScopePrefixes` struct
- `classify_scope()`, `language_to_syntect_ext()`
- `tokenize_file()` (replaced by `process_file()`)
- `MAX_TOKENIZE_LINES` / `MAX_TOKENIZE_BYTES` (tree-sitter is fast enough)

### Unchanged

- Binary format (`highlights.zst`, `TokenKind` values, RLE encoding)
- `HighlightStoreWriter` / `HighlightStoreReader`
- `multi_search.rs`, web/daemon consumers
- Existing indexes remain readable

## Test strategy

- Port `test_tokenize_rust_file` and highlight store roundtrip tests to use the new path
- Existing symbol extraction tests pass unchanged (same results, different plumbing)
- New test: `process_file` returns both highlights and symbols from a single call
- Verify TokenKind output is reasonable for each supported language (keywords highlighted as Keyword, strings as String, etc.)
