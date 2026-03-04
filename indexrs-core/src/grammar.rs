//! Tree-sitter grammar registry for symbol extraction.
//!
//! Maps [`Language`] variants to their tree-sitter grammars, enabling AST-based
//! symbol extraction for supported languages. This module is gated behind the
//! `symbols` cargo feature.

use crate::types::Language;

/// Returns the tree-sitter [`Language`](tree_sitter::Language) for a given
/// [`Language`] variant, or `None` if the language is not supported for symbol
/// extraction.
///
/// # Supported languages
///
/// | Language variant  | Grammar crate             |
/// |-------------------|---------------------------|
/// | Rust              | `tree-sitter-rust`        |
/// | Python            | `tree-sitter-python`      |
/// | TypeScript        | `tree-sitter-typescript`  |
/// | JavaScript        | `tree-sitter-typescript`  |
/// | Go                | `tree-sitter-go`          |
/// | C                 | `tree-sitter-c`           |
/// | Cpp               | `tree-sitter-c`           |
///
/// JavaScript reuses the TypeScript grammar (TSX), and C++ reuses the C grammar.
pub fn tree_sitter_language(lang: Language) -> Option<tree_sitter::Language> {
    match lang {
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::JavaScript => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
        Language::C | Language::Cpp => Some(tree_sitter_c::LANGUAGE.into()),
        _ => None,
    }
}

/// Returns `true` if the given language has tree-sitter grammar support for
/// symbol extraction.
pub fn supports_symbols(lang: Language) -> bool {
    tree_sitter_language(lang).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_grammar_loads() {
        let lang = tree_sitter_language(Language::Rust).expect("Rust should be supported");
        // Verify the language is functional by checking it has node kinds
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn test_python_grammar_loads() {
        let lang = tree_sitter_language(Language::Python).expect("Python should be supported");
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn test_typescript_grammar_loads() {
        let lang =
            tree_sitter_language(Language::TypeScript).expect("TypeScript should be supported");
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn test_javascript_grammar_loads() {
        let lang =
            tree_sitter_language(Language::JavaScript).expect("JavaScript should be supported");
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn test_go_grammar_loads() {
        let lang = tree_sitter_language(Language::Go).expect("Go should be supported");
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn test_c_grammar_loads() {
        let lang = tree_sitter_language(Language::C).expect("C should be supported");
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn test_cpp_uses_c_grammar() {
        let lang = tree_sitter_language(Language::Cpp).expect("Cpp should be supported");
        assert!(lang.node_kind_count() > 0);
    }

    #[test]
    fn test_unsupported_language_returns_none() {
        assert!(tree_sitter_language(Language::Ruby).is_none());
        assert!(tree_sitter_language(Language::Java).is_none());
        assert!(tree_sitter_language(Language::Shell).is_none());
        assert!(tree_sitter_language(Language::Markdown).is_none());
        assert!(tree_sitter_language(Language::Unknown).is_none());
    }

    #[test]
    fn test_supports_symbols_true() {
        assert!(supports_symbols(Language::Rust));
        assert!(supports_symbols(Language::Python));
        assert!(supports_symbols(Language::TypeScript));
        assert!(supports_symbols(Language::JavaScript));
        assert!(supports_symbols(Language::Go));
        assert!(supports_symbols(Language::C));
        assert!(supports_symbols(Language::Cpp));
    }

    #[test]
    fn test_supports_symbols_false() {
        assert!(!supports_symbols(Language::Ruby));
        assert!(!supports_symbols(Language::Java));
        assert!(!supports_symbols(Language::Haskell));
        assert!(!supports_symbols(Language::Unknown));
    }

    #[test]
    fn test_parser_can_be_configured() {
        // Verify that the language can actually be used with a parser
        let lang = tree_sitter_language(Language::Rust).unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang)
            .expect("should be able to set Rust language on parser");

        let source = b"fn main() {}";
        let tree = parser.parse(source, None).expect("should parse Rust code");
        let root = tree.root_node();
        assert_eq!(root.kind(), "source_file");
    }
}
