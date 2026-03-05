//! Tree-sitter grammar registry for symbol extraction.
//!
//! Maps [`Language`] variants to their tree-sitter grammars, enabling AST-based
//! symbol extraction for supported languages. This module is gated behind the
//! `symbols` cargo feature.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::types::Language;

/// Unified per-language tree-sitter config: grammar + highlight query + symbol query.
pub struct LanguageConfig {
    pub ts_language: tree_sitter::Language,
    pub highlight_query: tree_sitter::Query,
    pub symbol_query: Option<tree_sitter::Query>,
}

/// Get the cached `LanguageConfig` for a given language, or `None` if unsupported.
pub fn language_config(lang: Language) -> Option<&'static LanguageConfig> {
    static CACHE: OnceLock<HashMap<Language, LanguageConfig>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let entries: Vec<(Language, tree_sitter::Language, &str, Option<&str>)> = vec![
                (
                    Language::Rust,
                    tree_sitter_rust::LANGUAGE.into(),
                    tree_sitter_rust::HIGHLIGHTS_QUERY,
                    Some(super::symbol_extractor::RUST_QUERY),
                ),
                (
                    Language::Python,
                    tree_sitter_python::LANGUAGE.into(),
                    tree_sitter_python::HIGHLIGHTS_QUERY,
                    Some(super::symbol_extractor::PYTHON_QUERY),
                ),
                (
                    Language::TypeScript,
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                    Some(super::symbol_extractor::TYPESCRIPT_QUERY),
                ),
                (
                    Language::JavaScript,
                    tree_sitter_typescript::LANGUAGE_TSX.into(),
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                    Some(super::symbol_extractor::TYPESCRIPT_QUERY),
                ),
                (
                    Language::Go,
                    tree_sitter_go::LANGUAGE.into(),
                    tree_sitter_go::HIGHLIGHTS_QUERY,
                    Some(super::symbol_extractor::GO_QUERY),
                ),
                (
                    Language::C,
                    tree_sitter_c::LANGUAGE.into(),
                    tree_sitter_c::HIGHLIGHT_QUERY,
                    Some(super::symbol_extractor::C_QUERY),
                ),
                (
                    Language::Cpp,
                    tree_sitter_c::LANGUAGE.into(),
                    tree_sitter_c::HIGHLIGHT_QUERY,
                    Some(super::symbol_extractor::C_QUERY),
                ),
                (
                    Language::Ruby,
                    tree_sitter_ruby::LANGUAGE.into(),
                    tree_sitter_ruby::HIGHLIGHTS_QUERY,
                    Some(super::symbol_extractor::RUBY_QUERY),
                ),
                (
                    Language::Java,
                    tree_sitter_java::LANGUAGE.into(),
                    tree_sitter_java::HIGHLIGHTS_QUERY,
                    Some(super::symbol_extractor::JAVA_QUERY),
                ),
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
                let symbol_query =
                    sym_query_src.and_then(|src| tree_sitter::Query::new(&ts_lang, src).ok());
                map.insert(
                    lang,
                    LanguageConfig {
                        ts_language: ts_lang,
                        highlight_query,
                        symbol_query,
                    },
                );
            }
            map
        })
        .get(&lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_config_loads_for_all_supported() {
        for lang in [
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::JavaScript,
            Language::Go,
            Language::C,
            Language::Cpp,
            Language::Ruby,
            Language::Java,
        ] {
            let config = language_config(lang);
            assert!(config.is_some(), "should have config for {lang:?}");
            let c = config.unwrap();
            // highlight query should have at least one capture name
            assert!(
                !c.highlight_query.capture_names().is_empty(),
                "no captures for {lang:?}"
            );
        }
    }

    #[test]
    fn test_language_config_none_for_unsupported() {
        assert!(language_config(Language::Shell).is_none());
        assert!(language_config(Language::Unknown).is_none());
    }

    #[test]
    fn test_parser_can_be_configured() {
        // Verify that the language can actually be used with a parser
        let config = language_config(Language::Rust).unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&config.ts_language)
            .expect("should be able to set Rust language on parser");

        let source = b"fn main() {}";
        let tree = parser.parse(source, None).expect("should parse Rust code");
        let root = tree.root_node();
        assert_eq!(root.kind(), "source_file");
    }
}
