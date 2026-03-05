//! Unified tree-sitter processing: parse once, extract highlights + symbols.

use std::cell::RefCell;
use std::collections::HashMap;

use streaming_iterator::StreamingIterator;

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

fn parse(
    content: &[u8],
    language: Language,
    ts_lang: &tree_sitter::Language,
) -> Option<tree_sitter::Tree> {
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
    match name {
        "keyword"
        | "keyword.function"
        | "keyword.return"
        | "keyword.control"
        | "keyword.import"
        | "keyword.storage"
        | "keyword.directive"
        | "keyword.modifier"
        | "keyword.type"
        | "keyword.coroutine"
        | "keyword.repeat"
        | "keyword.conditional"
        | "keyword.exception" => TokenKind::Keyword,

        "keyword.operator" | "operator" => TokenKind::Operator,

        "string"
        | "string.escape"
        | "string.regexp"
        | "string.special"
        | "string.special.symbol" => TokenKind::String,

        "comment" | "comment.documentation" => TokenKind::Comment,

        "number" | "constant.builtin" | "boolean" => TokenKind::Number,

        "function" | "function.builtin" | "function.method" | "function.macro"
        | "function.call" | "method" => TokenKind::Function,

        "type" | "type.builtin" | "type.definition" | "constructor" => TokenKind::Type,

        "variable" | "variable.builtin" | "variable.parameter" | "variable.member" | "property"
        | "property.builtin" => TokenKind::Variable,

        "punctuation" | "punctuation.bracket" | "punctuation.delimiter" | "punctuation.special" => {
            TokenKind::Punctuation
        }

        "attribute" => TokenKind::Attribute,
        "constant" => TokenKind::Constant,
        "module" | "namespace" => TokenKind::Module,
        "label" | "lifetime" => TokenKind::Label,
        "escape" => TokenKind::String,

        _ => {
            // Prefix fallback for unrecognized dotted names
            if name.starts_with("keyword") {
                TokenKind::Keyword
            } else if name.starts_with("string") {
                TokenKind::String
            } else if name.starts_with("comment") {
                TokenKind::Comment
            } else if name.starts_with("function") {
                TokenKind::Function
            } else if name.starts_with("type") {
                TokenKind::Type
            } else if name.starts_with("variable") {
                TokenKind::Variable
            } else if name.starts_with("constant") {
                TokenKind::Constant
            } else if name.starts_with("punctuation") {
                TokenKind::Punctuation
            } else if name.starts_with("property") {
                TokenKind::Variable
            } else {
                TokenKind::Plain
            }
        }
    }
}

/// Run highlight query against a parsed tree, producing per-line token lists.
fn run_highlight_query(
    query: &tree_sitter::Query,
    tree: &tree_sitter::Tree,
    content: &[u8],
) -> Vec<Vec<Token>> {
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

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
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
        for bk in &mut byte_kinds[start..end.min(content.len())] {
            *bk = kind;
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
                tokens.push(Token {
                    len: pos - run_start,
                    kind,
                });
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
        None => {
            return TreeSitterResults {
                highlights: None,
                #[cfg(feature = "symbols")]
                symbols: Vec::new(),
            };
        }
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
        None => {
            return TreeSitterResults {
                highlights: None,
                #[cfg(feature = "symbols")]
                symbols: Vec::new(),
            };
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_file_rust_returns_highlights_and_symbols() {
        let src = b"fn main() {\n    let x = 42;\n}\n";
        let results = process_file(src, Language::Rust, FileId(0));
        assert!(results.highlights.is_some(), "should produce highlights");
        let highlights = results.highlights.unwrap();
        // 3 content lines + 1 empty trailing line (after final \n)
        assert_eq!(highlights.len(), 4);
        // "fn" should be classified as Keyword
        assert!(
            highlights[0]
                .iter()
                .any(|t| t.kind == crate::highlight::TokenKind::Keyword)
        );
        // Symbols: should find "main"
        #[cfg(feature = "symbols")]
        assert!(results.symbols.iter().any(|s| s.name == "main"));
    }

    #[test]
    fn test_process_file_unsupported_language() {
        let src = b"echo hello";
        let results = process_file(src, Language::Shell, FileId(0));
        assert!(results.highlights.is_none());
        #[cfg(feature = "symbols")]
        assert!(results.symbols.is_empty());
    }

    #[test]
    fn test_process_file_empty_content() {
        let results = process_file(b"", Language::Rust, FileId(0));
        // Empty file: highlights should be Some(empty), symbols empty
        assert!(results.highlights.is_some());
        assert!(results.highlights.unwrap().is_empty());
        #[cfg(feature = "symbols")]
        assert!(results.symbols.is_empty());
    }

    #[test]
    fn test_highlight_captures_include_strings() {
        let src = b"let msg = \"hello world\";\n";
        let results = process_file(src, Language::Rust, FileId(0));
        let highlights = results.highlights.unwrap();
        assert!(
            highlights[0]
                .iter()
                .any(|t| t.kind == crate::highlight::TokenKind::String)
        );
    }

    #[test]
    fn test_highlight_captures_include_comments() {
        let src = b"// this is a comment\nfn foo() {}\n";
        let results = process_file(src, Language::Rust, FileId(0));
        let highlights = results.highlights.unwrap();
        assert!(
            highlights[0]
                .iter()
                .any(|t| t.kind == crate::highlight::TokenKind::Comment)
        );
    }
}
