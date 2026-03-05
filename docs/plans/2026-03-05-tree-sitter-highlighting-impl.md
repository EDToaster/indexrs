# Tree-sitter Highlighting Migration — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan. Dispatch one subagent per task, review between tasks.

**Goal:** Replace syntect with tree-sitter for syntax highlighting, sharing a single parse with symbol extraction.

**Architecture:** Parse each file once with tree-sitter, run both `highlights.scm` and symbol queries against the same AST via `QueryCursor`. A new `tree_sitter_process.rs` module provides `process_file()` as the unified entry point. The storage format (`TokenKind`, RLE, `highlights.zst`) is unchanged.

**Tech Stack:** `tree-sitter` 0.24, existing grammar crates (`tree-sitter-rust`, etc.), `tree_sitter::QueryCursor` for highlight extraction.

---

### Task 1: Extend grammar.rs with LanguageConfig and highlight queries

**Files:**
- Modify: `ferret-core/src/grammar.rs`

**Step 1: Write the test**

Add to the existing test block in `grammar.rs`:

```rust
#[test]
fn test_language_config_loads_for_all_supported() {
    for lang in [
        Language::Rust, Language::Python, Language::TypeScript,
        Language::JavaScript, Language::Go, Language::C,
        Language::Cpp, Language::Ruby, Language::Java,
    ] {
        let config = language_config(lang);
        assert!(config.is_some(), "should have config for {lang:?}");
        let c = config.unwrap();
        // highlight query should have at least one capture name
        assert!(c.highlight_query.capture_names().len() > 0, "no captures for {lang:?}");
    }
}

#[test]
fn test_language_config_none_for_unsupported() {
    assert!(language_config(Language::Shell).is_none());
    assert!(language_config(Language::Unknown).is_none());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core --features symbols -- test_language_config`
Expected: FAIL — `language_config` not defined.

**Step 3: Implement LanguageConfig**

Add to `grammar.rs`:

```rust
use std::sync::OnceLock;
use std::collections::HashMap;

/// Unified per-language tree-sitter config: grammar + highlight query + symbol query.
pub struct LanguageConfig {
    pub ts_language: tree_sitter::Language,
    pub highlight_query: tree_sitter::Query,
    pub symbol_query: Option<tree_sitter::Query>,
}

/// Get the cached `LanguageConfig` for a given language, or `None` if unsupported.
pub fn language_config(lang: Language) -> Option<&'static LanguageConfig> {
    static CACHE: OnceLock<HashMap<Language, LanguageConfig>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let entries: Vec<(Language, tree_sitter::Language, &str, Option<&str>)> = vec![
            (Language::Rust, tree_sitter_rust::LANGUAGE.into(), tree_sitter_rust::HIGHLIGHTS_QUERY, Some(super::symbol_extractor::RUST_QUERY)),
            (Language::Python, tree_sitter_python::LANGUAGE.into(), tree_sitter_python::HIGHLIGHTS_QUERY, Some(super::symbol_extractor::PYTHON_QUERY)),
            (Language::TypeScript, tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), tree_sitter_typescript::HIGHLIGHTS_QUERY, Some(super::symbol_extractor::TYPESCRIPT_QUERY)),
            (Language::JavaScript, tree_sitter_typescript::LANGUAGE_TSX.into(), tree_sitter_typescript::HIGHLIGHTS_QUERY, Some(super::symbol_extractor::TYPESCRIPT_QUERY)),
            (Language::Go, tree_sitter_go::LANGUAGE.into(), tree_sitter_go::HIGHLIGHTS_QUERY, Some(super::symbol_extractor::GO_QUERY)),
            (Language::C, tree_sitter_c::LANGUAGE.into(), tree_sitter_c::HIGHLIGHT_QUERY, Some(super::symbol_extractor::C_QUERY)),
            (Language::Cpp, tree_sitter_c::LANGUAGE.into(), tree_sitter_c::HIGHLIGHT_QUERY, Some(super::symbol_extractor::C_QUERY)),
            (Language::Ruby, tree_sitter_ruby::LANGUAGE.into(), tree_sitter_ruby::HIGHLIGHTS_QUERY, Some(super::symbol_extractor::RUBY_QUERY)),
            (Language::Java, tree_sitter_java::LANGUAGE.into(), tree_sitter_java::HIGHLIGHTS_QUERY, Some(super::symbol_extractor::JAVA_QUERY)),
        ];
        let mut map = HashMap::with_capacity(entries.len());
        for (lang, ts_lang, hl_query_src, sym_query_src) in entries {
            let highlight_query = match tree_sitter::Query::new(&ts_lang, hl_query_src) {
                Ok(q) => q,
                Err(e) => {
                    tracing::warn!(?lang, %e, "failed to compile highlight query");
                    continue;
                }
            };
            let symbol_query = sym_query_src.and_then(|src| {
                tree_sitter::Query::new(&ts_lang, src).ok()
            });
            map.insert(lang, LanguageConfig { ts_language: ts_lang, highlight_query, symbol_query });
        }
        map
    }).get(&lang)
}
```

Note: This references symbol query constants from `symbol_extractor`. Those need to be made `pub(crate)` (they are currently private `const`s). That is handled in Task 3.

Note: `tree-sitter-c` exports `HIGHLIGHT_QUERY` (singular) while all other crates use `HIGHLIGHTS_QUERY` (plural).

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-indexer-core --features symbols -- test_language_config`
Expected: PASS

**Step 5: Commit**

```
git add ferret-core/src/grammar.rs
git commit -m "feat(core): add LanguageConfig with highlight queries to grammar.rs"
```

---

### Task 2: Create tree_sitter_process.rs with process_file()

**Files:**
- Create: `ferret-core/src/tree_sitter_process.rs`
- Modify: `ferret-core/src/lib.rs` (add module declaration)

**Step 1: Write tests**

Create `ferret-core/src/tree_sitter_process.rs` with tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileId, Language};

    #[test]
    fn test_process_file_rust_returns_highlights_and_symbols() {
        let src = b"fn main() {\n    let x = 42;\n}\n";
        let results = process_file(src, Language::Rust, FileId(0));
        assert!(results.highlights.is_some(), "should produce highlights");
        let highlights = results.highlights.unwrap();
        assert_eq!(highlights.len(), 3, "3 lines");
        // "fn" should be classified as Keyword
        assert!(highlights[0].iter().any(|t| t.kind == crate::highlight::TokenKind::Keyword));
        // Symbols: should find "main"
        assert!(results.symbols.iter().any(|s| s.name == "main"));
    }

    #[test]
    fn test_process_file_unsupported_language() {
        let src = b"echo hello";
        let results = process_file(src, Language::Shell, FileId(0));
        assert!(results.highlights.is_none());
        assert!(results.symbols.is_empty());
    }

    #[test]
    fn test_process_file_empty_content() {
        let results = process_file(b"", Language::Rust, FileId(0));
        // Empty file: highlights may be Some(empty) or None, symbols empty
        assert!(results.symbols.is_empty());
    }

    #[test]
    fn test_highlight_captures_include_strings() {
        let src = b"let msg = \"hello world\";\n";
        let results = process_file(src, Language::Rust, FileId(0));
        let highlights = results.highlights.unwrap();
        assert!(highlights[0].iter().any(|t| t.kind == crate::highlight::TokenKind::String));
    }

    #[test]
    fn test_highlight_captures_include_comments() {
        let src = b"// this is a comment\nfn foo() {}\n";
        let results = process_file(src, Language::Rust, FileId(0));
        let highlights = results.highlights.unwrap();
        assert!(highlights[0].iter().any(|t| t.kind == crate::highlight::TokenKind::Comment));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p ferret-indexer-core --features symbols -- tree_sitter_process`
Expected: FAIL — module doesn't compile yet.

**Step 3: Implement process_file and highlight query runner**

The core implementation in `tree_sitter_process.rs`:

```rust
//! Unified tree-sitter processing: parse once, extract highlights + symbols.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::grammar::language_config;
use crate::highlight::{Token, TokenKind};
use crate::types::{FileId, Language};

#[cfg(feature = "symbols")]
use crate::symbol_extractor::SymbolEntry;

/// Results from a unified tree-sitter processing pass.
pub struct TreeSitterResults {
    /// Per-line highlight tokens, or None if language is unsupported.
    pub highlights: Option<Vec<Vec<Token>>>,
    /// Extracted symbol definitions.
    #[cfg(feature = "symbols")]
    pub symbols: Vec<SymbolEntry>,
}

thread_local! {
    static PARSER_CACHE: RefCell<HashMap<Language, tree_sitter::Parser>> =
        RefCell::new(HashMap::new());
}

fn parse(content: &[u8], language: Language, ts_lang: &tree_sitter::Language) -> Option<tree_sitter::Tree> {
    PARSER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let parser = cache.entry(language).or_insert_with(|| {
            let mut p = tree_sitter::Parser::new();
            let _ = p.set_language(ts_lang);
            p
        });
        // Ensure parser is set to correct language (handles reuse across calls)
        let _ = parser.set_language(ts_lang);
        parser.parse(content, None)
    })
}

/// Map a tree-sitter highlight capture name to our TokenKind.
fn capture_to_token_kind(name: &str) -> TokenKind {
    // Check exact match first, then prefix match for dotted names
    match name {
        "keyword" | "keyword.function" | "keyword.return" | "keyword.control"
        | "keyword.import" | "keyword.storage" | "keyword.directive"
        | "keyword.modifier" | "keyword.type" | "keyword.coroutine"
        | "keyword.repeat" | "keyword.conditional" | "keyword.exception" => TokenKind::Keyword,

        "keyword.operator" | "operator" => TokenKind::Operator,

        "string" | "string.escape" | "string.regexp" | "string.special"
        | "string.special.symbol" => TokenKind::String,

        "comment" | "comment.documentation" => TokenKind::Comment,

        "number" | "constant.builtin" | "boolean" => TokenKind::Number,

        "function" | "function.builtin" | "function.method" | "function.macro"
        | "function.call" | "method" => TokenKind::Function,

        "type" | "type.builtin" | "type.definition" | "constructor" => TokenKind::Type,

        "variable" | "variable.builtin" | "variable.parameter"
        | "variable.member" | "property" | "property.builtin" => TokenKind::Variable,

        "punctuation" | "punctuation.bracket" | "punctuation.delimiter"
        | "punctuation.special" => TokenKind::Punctuation,

        "attribute" => TokenKind::Attribute,
        "constant" => TokenKind::Constant,
        "module" | "namespace" => TokenKind::Module,
        "label" | "lifetime" => TokenKind::Label,
        "escape" => TokenKind::String,

        _ => {
            // Prefix fallback for unrecognized dotted names
            if name.starts_with("keyword") { TokenKind::Keyword }
            else if name.starts_with("string") { TokenKind::String }
            else if name.starts_with("comment") { TokenKind::Comment }
            else if name.starts_with("function") { TokenKind::Function }
            else if name.starts_with("type") { TokenKind::Type }
            else if name.starts_with("variable") { TokenKind::Variable }
            else if name.starts_with("constant") { TokenKind::Constant }
            else if name.starts_with("punctuation") { TokenKind::Punctuation }
            else if name.starts_with("property") { TokenKind::Variable }
            else { TokenKind::Plain }
        }
    }
}

/// Run highlight query against a parsed tree, producing per-line token lists.
fn run_highlight_query(
    query: &tree_sitter::Query,
    tree: &tree_sitter::Tree,
    content: &[u8],
) -> Vec<Vec<Token>> {
    use streaming_iterator::StreamingIterator;

    // Build capture-index → TokenKind lookup
    let capture_kinds: Vec<TokenKind> = query
        .capture_names()
        .iter()
        .map(|name| capture_to_token_kind(name))
        .collect();

    // Collect all highlight spans: (start_byte, end_byte, TokenKind)
    let mut spans: Vec<(usize, usize, TokenKind)> = Vec::new();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), content);

    while let Some(m) = { matches.advance(); matches.get() } {
        for capture in m.captures {
            let kind = capture_kinds[capture.index as usize];
            if kind != TokenKind::Plain {
                let node = capture.node;
                spans.push((node.start_byte(), node.end_byte(), kind));
            }
        }
    }

    // Sort by start byte, then by span length descending (larger spans first,
    // so inner/more-specific captures override outer ones when we process)
    spans.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));

    // Build line break index
    let mut line_starts: Vec<usize> = vec![0];
    for (i, &b) in content.iter().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let num_lines = line_starts.len();

    // Build per-byte kind map (last writer wins for overlapping spans)
    let mut byte_kinds = vec![TokenKind::Plain; content.len()];
    for &(start, end, kind) in &spans {
        for b in start..end.min(content.len()) {
            byte_kinds[b] = kind;
        }
    }

    // Convert byte_kinds into per-line Token lists
    let mut all_lines = Vec::with_capacity(num_lines);
    for line_idx in 0..num_lines {
        let line_start = line_starts[line_idx];
        let line_end = if line_idx + 1 < num_lines {
            line_starts[line_idx + 1]
        } else {
            content.len()
        };

        let mut tokens: Vec<Token> = Vec::new();
        if line_start < line_end {
            let mut pos = line_start;
            while pos < line_end {
                let kind = byte_kinds[pos];
                let run_start = pos;
                while pos < line_end && byte_kinds[pos] == kind {
                    pos += 1;
                }
                tokens.push(Token { len: pos - run_start, kind });
            }
        }
        all_lines.push(tokens);
    }

    all_lines
}

/// Unified file processing: parse once, extract highlights + symbols.
pub fn process_file(content: &[u8], language: Language, file_id: FileId) -> TreeSitterResults {
    let config = match language_config(language) {
        Some(c) => c,
        None => return TreeSitterResults {
            highlights: None,
            #[cfg(feature = "symbols")]
            symbols: Vec::new(),
        },
    };

    if content.is_empty() {
        return TreeSitterResults {
            highlights: Some(Vec::new()),
            #[cfg(feature = "symbols")]
            symbols: Vec::new(),
        };
    }

    let tree = match parse(content, language, &config.ts_language) {
        Some(t) => t,
        None => return TreeSitterResults {
            highlights: None,
            #[cfg(feature = "symbols")]
            symbols: Vec::new(),
        },
    };

    let highlights = Some(run_highlight_query(&config.highlight_query, &tree, content));

    #[cfg(feature = "symbols")]
    let symbols = if let Some(ref sq) = config.symbol_query {
        crate::symbol_extractor::extract_symbols_from_tree(file_id, content, &tree, sq)
    } else {
        Vec::new()
    };

    TreeSitterResults {
        highlights,
        #[cfg(feature = "symbols")]
        symbols,
    }
}
```

Add to `lib.rs`:

```rust
#[cfg(feature = "symbols")]
pub mod tree_sitter_process;
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferret-indexer-core --features symbols -- tree_sitter_process`
Expected: PASS

**Step 5: Commit**

```
git add ferret-core/src/tree_sitter_process.rs ferret-core/src/lib.rs
git commit -m "feat(core): add tree_sitter_process module with unified process_file"
```

---

### Task 3: Refactor symbol_extractor.rs to accept a pre-parsed Tree

**Files:**
- Modify: `ferret-core/src/symbol_extractor.rs`

**Step 1: Make symbol query constants pub(crate)**

Change all query constants from `const` to `pub(crate) const`:
- `RUST_QUERY` (line 36)
- `PYTHON_QUERY` (line 68)
- `TYPESCRIPT_QUERY` (line 77)
- `GO_QUERY` (line 98)
- `RUBY_QUERY` (line 127)
- `JAVA_QUERY` (line 145)
- `C_QUERY` (line 163)

**Step 2: Add extract_symbols_from_tree function**

Add a new public function that accepts a `&Tree` and `&Query` instead of parsing internally:

```rust
/// Extract symbols from a pre-parsed tree-sitter AST.
///
/// This is the shared-parse variant — called by `tree_sitter_process::process_file`
/// when the tree is already available from highlight processing.
pub fn extract_symbols_from_tree(
    file_id: FileId,
    content: &[u8],
    tree: &tree_sitter::Tree,
    query: &tree_sitter::Query,
) -> Vec<SymbolEntry> {
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut symbols = Vec::new();
    let mut matches = cursor.matches(query, tree.root_node(), content);

    while let Some(match_) = {
        matches.advance();
        matches.get()
    } {
        let mut name_text: Option<String> = None;
        let mut kind: Option<SymbolKind> = None;
        let mut def_line: u32 = 0;
        let mut def_column: u16 = 0;

        for capture in match_.captures {
            let capture_name = query.capture_names()[capture.index as usize];
            if capture_name == "name" {
                let node = capture.node;
                if let Ok(text) = std::str::from_utf8(&content[node.byte_range()]) {
                    name_text = Some(text.to_string());
                    def_line = node.start_position().row as u32;
                    def_column = node.start_position().column as u16;
                }
            } else if let Some(k) = kind_from_capture(capture_name) {
                kind = Some(k);
            }
        }

        if let (Some(sym_name), Some(sym_kind)) = (name_text, kind) {
            symbols.push(SymbolEntry {
                file_id,
                name: sym_name,
                kind: sym_kind,
                line: def_line,
                column: def_column,
            });
        }
    }

    symbols
}
```

This is essentially the body of `extract_symbols()` lines 279-318, but accepting `tree` and `query` as parameters. The existing `extract_symbols()` function remains for backward compatibility and for the compaction path.

**Step 3: Run existing symbol tests to verify nothing broke**

Run: `cargo test -p ferret-indexer-core --features symbols -- symbol_extractor`
Expected: all existing tests PASS

**Step 4: Commit**

```
git add ferret-core/src/symbol_extractor.rs
git commit -m "refactor(core): add extract_symbols_from_tree for shared-parse path"
```

---

### Task 4: Wire process_file into segment.rs Phase 1

**Files:**
- Modify: `ferret-core/src/segment.rs`

**Step 1: Update build_inner Phase 1**

Replace the separate `tokenize_file` + `extract_symbols` calls in the `par_iter` closure (lines 462-478) with a unified `process_file` call.

Current code (lines 462-478):

```rust
// Tokenize for syntax highlighting
let highlight_compressed =
    crate::highlight::tokenize_file(&input.content, language).map(|line_tokens| {
        let fh = crate::highlight::build_file_highlight(&line_tokens);
        let serialized = crate::highlight::serialize_file_highlight(&fh);
        let hl_compressed = zstd::bulk::compress(&serialized, ZSTD_LEVEL)
            .expect("zstd compress highlight");
        let lines = fh.line_offsets.len() as u32;
        (hl_compressed, lines)
    });

#[cfg(feature = "symbols")]
let symbols = crate::symbol_extractor::extract_symbols(
    FileId(0), // placeholder — remapped in Phase 3
    &input.content,
    language,
);
```

Replace with:

```rust
// Unified tree-sitter processing: parse once, extract highlights + symbols
#[cfg(feature = "symbols")]
let ts_results = crate::tree_sitter_process::process_file(
    &input.content, language, FileId(0),
);
#[cfg(feature = "symbols")]
let highlight_compressed =
    ts_results.highlights.map(|line_tokens| {
        let fh = crate::highlight::build_file_highlight(&line_tokens);
        let serialized = crate::highlight::serialize_file_highlight(&fh);
        let hl_compressed = zstd::bulk::compress(&serialized, ZSTD_LEVEL)
            .expect("zstd compress highlight");
        let lines = fh.line_offsets.len() as u32;
        (hl_compressed, lines)
    });
#[cfg(not(feature = "symbols"))]
let highlight_compressed: Option<(Vec<u8>, u32)> = None;

#[cfg(feature = "symbols")]
let symbols = ts_results.symbols;
```

**Step 2: Update compact path (build_from_compact, ~line 640-648)**

The compaction path at line 640-648 also calls `tokenize_file`. Since compaction re-reads raw content, it should also use the unified path. Replace:

```rust
let (highlight_offset, highlight_len, highlight_lines) = if let Some(line_tokens) =
    crate::highlight::tokenize_file(&file.raw_content, file.language)
{
```

With:

```rust
#[cfg(feature = "symbols")]
let compact_hl = crate::tree_sitter_process::process_file(
    &file.raw_content, file.language, file_id,
);
#[cfg(not(feature = "symbols"))]
let compact_hl = crate::tree_sitter_process::TreeSitterResults { highlights: None };

let (highlight_offset, highlight_len, highlight_lines) = if let Some(line_tokens) =
    compact_hl.highlights
{
```

Note: The compaction path also re-extracts symbols (lines 682-699). Update that to use `compact_hl.symbols` instead of calling `extract_symbols` again.

**Step 3: Run full test suite**

Run: `cargo test -p ferret-indexer-core --features symbols`
Expected: all tests PASS

**Step 4: Commit**

```
git add ferret-core/src/segment.rs
git commit -m "feat(core): wire process_file into segment build for unified parse"
```

---

### Task 5: Remove syntect dependency

**Files:**
- Modify: `ferret-core/Cargo.toml`
- Modify: `ferret-core/src/highlight.rs`
- Modify: `ferret-core/src/lib.rs`

**Step 1: Remove syntect from Cargo.toml**

Delete line 37:

```toml
syntect = { version = "5", default-features = false, features = ["default-syntaxes", "default-themes", "parsing", "html", "regex-fancy"] }
```

**Step 2: Remove syntect code from highlight.rs**

Delete all of the following from `highlight.rs`:
- `use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};` (line 147)
- `static SYNTAX_SET: LazyLock<SyntaxSet>` (line 152)
- `MAX_TOKENIZE_LINES`, `MAX_TOKENIZE_BYTES` constants (lines 155-160)
- `ScopePrefixes` struct and `SCOPE_PREFIXES` static (lines 165-203)
- `language_to_syntect_ext` function (lines 206-247)
- `classify_scope` function (lines 249-309)
- `tokenize_file` function (lines 311-373)
- `use std::sync::LazyLock;` (if no longer needed — check if other code in the file uses it)
- Test `test_tokenize_rust_file` and `test_tokenize_unknown_language_returns_none` (they test the removed `tokenize_file`)

Keep everything else: `TokenKind`, `Token`, `FileHighlight`, `encode_rle`, `decode_rle`, `build_file_highlight`, `serialize_file_highlight`, `deserialize_file_highlight`, `HighlightStoreWriter`, `HighlightStoreReader`, and their tests.

**Step 3: Update lib.rs re-exports**

In `lib.rs` line 62, remove `tokenize_file` from the re-export:

```rust
// Before:
pub use highlight::{
    FileHighlight, HighlightStoreReader, HighlightStoreWriter, Token, TokenKind,
    build_file_highlight, decode_rle, encode_rle, tokenize_file,
};

// After:
pub use highlight::{
    FileHighlight, HighlightStoreReader, HighlightStoreWriter, Token, TokenKind,
    build_file_highlight, decode_rle, encode_rle,
};
```

**Step 4: Check for any remaining syntect references**

Run: `grep -r syntect ferret-core/` — should return nothing.

**Step 5: Run clippy and full test suite**

Run: `cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: clean clippy, all tests pass.

**Step 6: Commit**

```
git add ferret-core/Cargo.toml ferret-core/src/highlight.rs ferret-core/src/lib.rs
git commit -m "refactor(core): remove syntect dependency, highlighting now via tree-sitter"
```

---

### Task 6: Clean up old grammar.rs and symbol_extractor.rs

**Files:**
- Modify: `ferret-core/src/grammar.rs` — remove old `tree_sitter_language()` and `supports_symbols()` if no longer called (check callers first)
- Modify: `ferret-core/src/symbol_extractor.rs` — remove `compiled_queries()` cache and `PARSER_CACHE` if all callers now use the shared-parse path. Keep `extract_symbols()` since the compaction path may still need it as a standalone entry point.

**Step 1: Check all callers**

Run: `grep -rn 'tree_sitter_language\|supports_symbols\|compiled_queries\|extract_symbols(' ferret-core/src/`

If `extract_symbols()` is still called from compaction, keep it but have it delegate to the new `language_config` + `extract_symbols_from_tree` internally.

**Step 2: Run full test suite**

Run: `cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: all pass.

**Step 3: Commit**

```
git add ferret-core/src/grammar.rs ferret-core/src/symbol_extractor.rs
git commit -m "refactor(core): consolidate grammar and symbol extraction into LanguageConfig"
```

---

### Task 7: Final verification

**Step 1: Run CI checks**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

**Step 2: Run with symbols feature disabled**

```bash
cargo test -p ferret-indexer-core
cargo check --workspace
```

Verify the build works without the `symbols` feature — highlighting should be disabled (returns `None`), no compile errors.

**Step 3: Smoke test with the demo**

```bash
cargo run -p ferret-indexer-core --example demo --features symbols -- ferret-core/src "fn main"
```

Verify search results appear and segments build successfully.
