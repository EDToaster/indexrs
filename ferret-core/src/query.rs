//! Query parser for the ferret search query language.
//!
//! Parses a query string into a [`Query`] AST using recursive descent.
//! The grammar supports literal text, regex patterns, exact phrases,
//! path/language filters, case sensitivity, and boolean operators (NOT, OR,
//! implicit AND).
//!
//! # Syntax
//!
//! - `foo bar` — implicit AND: files must contain both "foo" and "bar"
//! - `foo OR bar` — union: files containing either "foo" or "bar"
//! - `NOT foo` — exclude files containing "foo"
//! - `"exact phrase"` — exact phrase match
//! - `/pattern/` — regex match
//! - `path:src/` — path prefix filter
//! - `language:rust` or `lang:rs` — language filter
//! - `case:yes foo` — case-sensitive match for "foo"

use crate::error::IndexError;
use crate::types::Language;

/// Parsed query AST node.
///
/// This is the shared type consumed by trigram extraction (HHC-47),
/// query planning (HHC-48), and candidate verification (HHC-49).
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// Bare text literal substring match.
    /// Case-insensitive by default.
    Literal(LiteralQuery),
    /// `/pattern/` regex match.
    Regex(RegexQuery),
    /// `"exact phrase"` match.
    Phrase(PhraseQuery),
    /// `path:prefix` — path prefix filter (metadata-level, no trigrams).
    PathFilter(String),
    /// `language:rust` / `lang:rs` — language filter (metadata-level).
    LanguageFilter(Language),
    /// `NOT term` — exclusion (inverts match).
    Not(Box<Query>),
    /// `term1 OR term2` — union of two sub-queries.
    Or(Box<Query>, Box<Query>),
    /// Implicit AND between space-separated terms.
    And(Vec<Query>),
}

/// A literal substring query.
#[derive(Debug, Clone, PartialEq)]
pub struct LiteralQuery {
    /// The text to search for.
    pub text: String,
    /// Whether the match is case-sensitive (`false` by default).
    pub case_sensitive: bool,
}

/// A regex pattern query.
#[derive(Debug, Clone, PartialEq)]
pub struct RegexQuery {
    /// The regex pattern (without surrounding `/` delimiters).
    pub pattern: String,
    /// Whether the match is case-sensitive (`true` by default for regex).
    pub case_sensitive: bool,
}

/// An exact phrase query.
#[derive(Debug, Clone, PartialEq)]
pub struct PhraseQuery {
    /// The exact phrase text (without surrounding `"` delimiters).
    pub text: String,
    /// Whether the match is case-sensitive (`false` by default).
    pub case_sensitive: bool,
}

/// Parse a query string into a [`Query`] AST.
///
/// Returns `IndexError::QueryParse` if the query is empty or malformed.
///
/// # Examples
///
/// ```
/// use ferret_indexer_core::query::parse_query;
/// use ferret_indexer_core::query::{Query, LiteralQuery};
///
/// let q = parse_query("hello").unwrap();
/// assert_eq!(q, Query::Literal(LiteralQuery {
///     text: "hello".to_string(),
///     case_sensitive: false,
/// }));
/// ```
pub fn parse_query(input: &str) -> Result<Query, IndexError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(IndexError::QueryParse("empty query".to_string()));
    }
    let mut parser = Parser::new(input);
    let query = parser.parse_or()?;
    parser.skip_whitespace();
    if !parser.is_eof() {
        return Err(IndexError::QueryParse(format!(
            "unexpected input at position {}",
            parser.pos
        )));
    }
    Ok(query)
}

/// Match a language name or file extension to a [`Language`] variant.
///
/// Accepts both full names (case-insensitive) and common file extensions.
/// Returns `IndexError::QueryParse` for unrecognized language strings.
pub fn match_language(s: &str) -> Result<Language, IndexError> {
    match s.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Ok(Language::Rust),
        "python" | "py" => Ok(Language::Python),
        "typescript" | "ts" => Ok(Language::TypeScript),
        "javascript" | "js" => Ok(Language::JavaScript),
        "go" => Ok(Language::Go),
        "c" => Ok(Language::C),
        "cpp" | "c++" | "cxx" => Ok(Language::Cpp),
        "java" => Ok(Language::Java),
        "ruby" | "rb" => Ok(Language::Ruby),
        "shell" | "sh" | "bash" => Ok(Language::Shell),
        "markdown" | "md" => Ok(Language::Markdown),
        "yaml" | "yml" => Ok(Language::Yaml),
        "toml" => Ok(Language::Toml),
        "json" => Ok(Language::Json),
        "xml" => Ok(Language::Xml),
        "html" | "htm" => Ok(Language::Html),
        "css" => Ok(Language::Css),
        "scss" => Ok(Language::Scss),
        "sass" => Ok(Language::Sass),
        "sql" => Ok(Language::Sql),
        "protobuf" | "proto" => Ok(Language::Protobuf),
        "dockerfile" | "docker" => Ok(Language::Dockerfile),
        "hcl" | "terraform" | "tf" => Ok(Language::Hcl),
        "kotlin" | "kt" => Ok(Language::Kotlin),
        "swift" => Ok(Language::Swift),
        "scala" | "sc" => Ok(Language::Scala),
        "elixir" | "ex" => Ok(Language::Elixir),
        "erlang" | "erl" => Ok(Language::Erlang),
        "haskell" | "hs" => Ok(Language::Haskell),
        "ocaml" | "ml" => Ok(Language::OCaml),
        "lua" => Ok(Language::Lua),
        "perl" | "pl" => Ok(Language::Perl),
        "r" => Ok(Language::R),
        "dart" => Ok(Language::Dart),
        "zig" => Ok(Language::Zig),
        "nix" => Ok(Language::Nix),
        "plaintext" | "text" | "txt" => Ok(Language::PlainText),
        "starlark" | "bazel" | "bzl" => Ok(Language::Starlark),
        "jsonnet" => Ok(Language::Jsonnet),
        "haml" => Ok(Language::Haml),
        "csv" => Ok(Language::Csv),
        "graphql" | "gql" => Ok(Language::GraphQL),
        "erb" => Ok(Language::Erb),
        "template" | "jinja" | "jinja2" | "j2" => Ok(Language::Template),
        "restructuredtext" | "rst" => Ok(Language::ReStructuredText),
        "ejs" => Ok(Language::Ejs),
        "groovy" | "gradle" => Ok(Language::Groovy),
        "batch" | "bat" | "cmd" => Ok(Language::Batch),
        "csharp" | "c#" | "cs" => Ok(Language::CSharp),
        "vue" => Ok(Language::Vue),
        "svelte" => Ok(Language::Svelte),
        "powershell" | "ps1" => Ok(Language::PowerShell),
        "less" => Ok(Language::Less),
        "coffeescript" | "coffee" => Ok(Language::CoffeeScript),
        "solidity" | "sol" => Ok(Language::Solidity),
        "clojure" | "clj" => Ok(Language::Clojure),
        "julia" | "jl" => Ok(Language::Julia),
        "assembly" | "asm" => Ok(Language::Assembly),
        "nim" => Ok(Language::Nim),
        _ => Err(IndexError::QueryParse(format!("unknown language: {s}"))),
    }
}

/// Maximum recursion depth for the parser. Prevents stack overflow from
/// deeply nested queries like `NOT NOT NOT ... NOT foo`.
const MAX_PARSE_DEPTH: usize = 128;

/// Cursor-based recursive descent parser.
struct Parser<'a> {
    input: &'a str,
    pos: usize,
    /// Whether the next primary term should be case-sensitive.
    /// Set by `case:yes`, reset after consuming one primary.
    case_sensitive: bool,
    /// Current recursion depth, used to prevent stack overflow.
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            case_sensitive: false,
            depth: 0,
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn remaining(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len() {
            let b = self.input.as_bytes()[self.pos];
            if b.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        self.remaining().starts_with(s)
    }

    /// Check if the remaining input starts with `keyword` followed by a
    /// word boundary (whitespace, EOF, `"`, or `/`). This prevents matching
    /// "ORacle" as the "OR" keyword.
    fn starts_with_keyword(&self, keyword: &str) -> bool {
        if !self.starts_with(keyword) {
            return false;
        }
        let after = self.pos + keyword.len();
        if after >= self.input.len() {
            return true;
        }
        let next = self.input.as_bytes()[after];
        next.is_ascii_whitespace() || next == b'"' || next == b'/'
    }

    // ── Grammar methods ──

    /// `or_expr := and_expr ("OR" and_expr)*`
    fn parse_or(&mut self) -> Result<Query, IndexError> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_whitespace();
            if self.starts_with_keyword("OR") {
                self.pos += 2; // consume "OR"
                self.skip_whitespace();
                let right = self.parse_and()?;
                left = Query::Or(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// `and_expr := unary_expr+` (implicit AND, stops at EOF or "OR" keyword)
    fn parse_and(&mut self) -> Result<Query, IndexError> {
        self.skip_whitespace();
        let first = self.parse_unary()?;
        let mut children = vec![first];
        loop {
            self.skip_whitespace();
            if self.is_eof() {
                break;
            }
            // Stop before OR keyword so parse_or can handle it
            if self.starts_with_keyword("OR") {
                break;
            }
            // Try to parse another unary; if it fails, restore position and stop
            let saved_pos = self.pos;
            match self.parse_unary() {
                Ok(child) => children.push(child),
                Err(_) => {
                    self.pos = saved_pos;
                    break;
                }
            }
        }
        if children.len() == 1 {
            Ok(children.into_iter().next().unwrap())
        } else {
            Ok(Query::And(children))
        }
    }

    /// `unary_expr := "NOT" unary_expr | primary`
    fn parse_unary(&mut self) -> Result<Query, IndexError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(IndexError::QueryParse(format!(
                "query too deeply nested (max depth {MAX_PARSE_DEPTH})"
            )));
        }
        let result = self.parse_unary_inner();
        self.depth -= 1;
        result
    }

    fn parse_unary_inner(&mut self) -> Result<Query, IndexError> {
        self.skip_whitespace();
        if self.starts_with_keyword("NOT") {
            self.pos += 3; // consume "NOT"
            self.skip_whitespace();
            let inner = self.parse_unary()?;
            Ok(Query::Not(Box::new(inner)))
        } else {
            self.parse_primary()
        }
    }

    /// Dispatch to the appropriate primary parser based on the next character
    /// or prefix.
    fn parse_primary(&mut self) -> Result<Query, IndexError> {
        self.skip_whitespace();
        if self.is_eof() {
            return Err(IndexError::QueryParse(
                "unexpected end of query".to_string(),
            ));
        }

        // Regex: /pattern/
        if self.peek() == Some('/') {
            return self.parse_regex();
        }

        // Phrase: "exact phrase"
        if self.peek() == Some('"') {
            return self.parse_phrase();
        }

        // path:prefix
        if self.starts_with("path:") {
            return self.parse_path_filter();
        }

        // language:lang or lang:lang
        if self.starts_with("language:") || self.starts_with("lang:") {
            return self.parse_language_filter();
        }

        // case:yes modifier — must check word boundary
        if self.starts_with("case:yes") {
            return self.parse_case_modifier();
        }

        // Default: literal
        self.parse_literal()
    }

    /// Consume a literal token: non-whitespace, non-quote, non-slash chars.
    fn parse_literal(&mut self) -> Result<Query, IndexError> {
        let start = self.pos;
        while self.pos < self.input.len() {
            let b = self.input.as_bytes()[self.pos];
            if b.is_ascii_whitespace() || b == b'"' || b == b'/' {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            return Err(IndexError::QueryParse(format!(
                "expected literal at position {}",
                self.pos
            )));
        }
        let text = self.input[start..self.pos].to_string();
        let cs = self.case_sensitive;
        self.case_sensitive = false; // reset after use
        Ok(Query::Literal(LiteralQuery {
            text,
            case_sensitive: cs,
        }))
    }

    /// Consume `"exact phrase"`. Returns error for unterminated quotes.
    fn parse_phrase(&mut self) -> Result<Query, IndexError> {
        debug_assert_eq!(self.peek(), Some('"'));
        self.pos += 1; // consume opening "
        let start = self.pos;
        while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b'"' {
            self.pos += 1;
        }
        if self.pos >= self.input.len() {
            return Err(IndexError::QueryParse(
                "unterminated phrase (missing closing \")".to_string(),
            ));
        }
        let text = self.input[start..self.pos].to_string();
        self.pos += 1; // consume closing "
        let cs = self.case_sensitive;
        self.case_sensitive = false;
        Ok(Query::Phrase(PhraseQuery {
            text,
            case_sensitive: cs,
        }))
    }

    /// Consume `/pattern/` with backslash escapes. Validates regex at parse time.
    fn parse_regex(&mut self) -> Result<Query, IndexError> {
        debug_assert_eq!(self.peek(), Some('/'));
        self.pos += 1; // consume opening /
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '/' {
                break;
            }
            if c == '\\' {
                self.pos += c.len_utf8();
                // Skip the escaped character too
                if let Some(next) = self.peek() {
                    self.pos += next.len_utf8();
                }
                continue;
            }
            self.pos += c.len_utf8();
        }
        if self.is_eof() {
            return Err(IndexError::QueryParse(
                "unterminated regex (missing closing /)".to_string(),
            ));
        }
        let pattern = self.input[start..self.pos].to_string();
        self.pos += 1; // consume closing /

        // Validate the regex pattern at parse time (1 MB size limit to prevent ReDoS)
        if let Err(e) = regex::RegexBuilder::new(&pattern)
            .size_limit(1 << 20)
            .build()
        {
            return Err(IndexError::QueryParse(format!(
                "invalid regex at position {start}: {e}"
            )));
        }

        // Regex is case-sensitive by default; case:yes is a no-op but
        // we still honor the flag for consistency.
        let cs = if self.case_sensitive {
            self.case_sensitive = false;
            true
        } else {
            true // regex default
        };
        Ok(Query::Regex(RegexQuery {
            pattern,
            case_sensitive: cs,
        }))
    }

    /// Consume `path:value` where value runs until whitespace.
    fn parse_path_filter(&mut self) -> Result<Query, IndexError> {
        debug_assert!(self.starts_with("path:"));
        self.pos += 5; // consume "path:"
        let start = self.pos;
        while self.pos < self.input.len() && !self.input.as_bytes()[self.pos].is_ascii_whitespace()
        {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(IndexError::QueryParse(
                "expected path value after 'path:'".to_string(),
            ));
        }
        let value = self.input[start..self.pos].to_string();
        Ok(Query::PathFilter(value))
    }

    /// Consume `language:value` or `lang:value`.
    fn parse_language_filter(&mut self) -> Result<Query, IndexError> {
        let prefix_len = if self.starts_with("language:") {
            9
        } else {
            debug_assert!(self.starts_with("lang:"));
            5
        };
        self.pos += prefix_len;
        let start = self.pos;
        while self.pos < self.input.len() && !self.input.as_bytes()[self.pos].is_ascii_whitespace()
        {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(IndexError::QueryParse(
                "expected language value after filter prefix".to_string(),
            ));
        }
        let value = &self.input[start..self.pos];
        let lang = match_language(value)?;
        Ok(Query::LanguageFilter(lang))
    }

    /// Consume `case:yes` modifier. If followed by non-whitespace (e.g.,
    /// "case:yesterday"), treat the whole token as a literal instead.
    fn parse_case_modifier(&mut self) -> Result<Query, IndexError> {
        debug_assert!(self.starts_with("case:yes"));
        let after = self.pos + 8; // length of "case:yes"
        // Check word boundary: must be followed by whitespace, EOF, `"`, or `/`
        if after < self.input.len() {
            let next = self.input.as_bytes()[after];
            if !next.is_ascii_whitespace() && next != b'"' && next != b'/' {
                // Not a modifier, treat as literal (e.g., "case:yesterday")
                return self.parse_literal();
            }
        }
        self.pos = after; // consume "case:yes"
        self.case_sensitive = true;
        self.skip_whitespace();
        self.parse_unary()
    }
}

impl std::fmt::Display for Query {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Query::Literal(lit) => {
                if lit.case_sensitive {
                    write!(f, "case:{}", lit.text)
                } else {
                    write!(f, "{}", lit.text)
                }
            }
            Query::Regex(re) => write!(f, "/{}/", re.pattern),
            Query::Phrase(ph) => write!(f, "\"{}\"", ph.text),
            Query::PathFilter(p) => write!(f, "path:{p}"),
            Query::LanguageFilter(l) => write!(f, "language:{l}"),
            Query::Not(inner) => write!(f, "NOT {inner}"),
            Query::Or(a, b) => write!(f, "{a} OR {b}"),
            Query::And(children) => {
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{child}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Task 1: AST construction tests ──

    #[test]
    fn test_ast_construction() {
        let q = Query::Literal(LiteralQuery {
            text: "hello".to_string(),
            case_sensitive: false,
        });
        assert_eq!(
            q,
            Query::Literal(LiteralQuery {
                text: "hello".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_ast_and() {
        let q = Query::And(vec![
            Query::Literal(LiteralQuery {
                text: "foo".to_string(),
                case_sensitive: false,
            }),
            Query::Literal(LiteralQuery {
                text: "bar".to_string(),
                case_sensitive: false,
            }),
        ]);
        if let Query::And(children) = &q {
            assert_eq!(children.len(), 2);
        } else {
            panic!("expected And variant");
        }
    }

    #[test]
    fn test_ast_nested() {
        let q = Query::Or(
            Box::new(Query::Literal(LiteralQuery {
                text: "foo".to_string(),
                case_sensitive: false,
            })),
            Box::new(Query::Not(Box::new(Query::Literal(LiteralQuery {
                text: "bar".to_string(),
                case_sensitive: false,
            })))),
        );
        match &q {
            Query::Or(left, right) => {
                assert_eq!(
                    **left,
                    Query::Literal(LiteralQuery {
                        text: "foo".to_string(),
                        case_sensitive: false,
                    })
                );
                assert!(matches!(**right, Query::Not(_)));
            }
            _ => panic!("expected Or variant"),
        }
    }

    // ── Task 2: Parser tests ──

    #[test]
    fn test_parse_single_literal() {
        let q = parse_query("hello").unwrap();
        assert_eq!(
            q,
            Query::Literal(LiteralQuery {
                text: "hello".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_parse_empty_query() {
        let err = parse_query("").unwrap_err();
        assert!(matches!(err, IndexError::QueryParse(_)));
    }

    #[test]
    fn test_parse_whitespace_only() {
        let err = parse_query("   ").unwrap_err();
        assert!(matches!(err, IndexError::QueryParse(_)));
    }

    // ---- Phrase tests ----

    #[test]
    fn test_parse_phrase() {
        let q = parse_query("\"hello world\"").unwrap();
        assert_eq!(
            q,
            Query::Phrase(PhraseQuery {
                text: "hello world".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_parse_phrase_empty() {
        let q = parse_query("\"\"").unwrap();
        assert_eq!(
            q,
            Query::Phrase(PhraseQuery {
                text: "".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_parse_phrase_unterminated() {
        let err = parse_query("\"hello").unwrap_err();
        assert!(matches!(err, IndexError::QueryParse(_)));
        assert!(err.to_string().contains("unterminated phrase"));
    }

    // ---- Regex tests ----

    #[test]
    fn test_parse_regex() {
        let q = parse_query("/foo.*bar/").unwrap();
        assert_eq!(
            q,
            Query::Regex(RegexQuery {
                pattern: "foo.*bar".to_string(),
                case_sensitive: true,
            })
        );
    }

    #[test]
    fn test_parse_regex_simple() {
        let q = parse_query("/\\d+/").unwrap();
        assert_eq!(
            q,
            Query::Regex(RegexQuery {
                pattern: "\\d+".to_string(),
                case_sensitive: true,
            })
        );
    }

    #[test]
    fn test_parse_regex_with_escaped_slash() {
        let q = parse_query("/foo\\/bar/").unwrap();
        assert_eq!(
            q,
            Query::Regex(RegexQuery {
                pattern: "foo\\/bar".to_string(),
                case_sensitive: true,
            })
        );
    }

    #[test]
    fn test_parse_regex_unterminated() {
        let err = parse_query("/hello").unwrap_err();
        assert!(matches!(err, IndexError::QueryParse(_)));
        assert!(err.to_string().contains("unterminated regex"));
    }

    #[test]
    fn test_parse_regex_invalid_pattern() {
        let err = parse_query("/[invalid/").unwrap_err();
        assert!(matches!(err, IndexError::QueryParse(_)));
        assert!(err.to_string().contains("invalid regex"));
    }

    // ---- Path filter tests ----

    #[test]
    fn test_parse_path_filter() {
        let q = parse_query("path:src/").unwrap();
        assert_eq!(q, Query::PathFilter("src/".to_string()));
    }

    #[test]
    fn test_parse_path_filter_deep() {
        let q = parse_query("path:src/core/lib.rs").unwrap();
        assert_eq!(q, Query::PathFilter("src/core/lib.rs".to_string()));
    }

    #[test]
    fn test_parse_path_filter_empty_value() {
        let err = parse_query("path: foo").unwrap_err();
        assert!(matches!(err, IndexError::QueryParse(_)));
    }

    // ---- Language filter tests ----

    #[test]
    fn test_parse_language_filter_full_name() {
        let q = parse_query("language:rust").unwrap();
        assert_eq!(q, Query::LanguageFilter(Language::Rust));
    }

    #[test]
    fn test_parse_language_filter_short() {
        let q = parse_query("lang:rs").unwrap();
        assert_eq!(q, Query::LanguageFilter(Language::Rust));
    }

    #[test]
    fn test_parse_language_filter_python() {
        let q = parse_query("language:python").unwrap();
        assert_eq!(q, Query::LanguageFilter(Language::Python));
    }

    #[test]
    fn test_parse_language_filter_py() {
        let q = parse_query("lang:py").unwrap();
        assert_eq!(q, Query::LanguageFilter(Language::Python));
    }

    #[test]
    fn test_parse_language_filter_case_insensitive() {
        let q = parse_query("language:Rust").unwrap();
        assert_eq!(q, Query::LanguageFilter(Language::Rust));
    }

    #[test]
    fn test_parse_language_filter_unknown() {
        let err = parse_query("language:brainfuck").unwrap_err();
        assert!(matches!(err, IndexError::QueryParse(_)));
        assert!(err.to_string().contains("unknown language"));
    }

    #[test]
    fn test_parse_language_filter_typescript() {
        let q = parse_query("lang:ts").unwrap();
        assert_eq!(q, Query::LanguageFilter(Language::TypeScript));
    }

    // ---- AND tests (implicit) ----

    #[test]
    fn test_parse_implicit_and() {
        let q = parse_query("foo bar").unwrap();
        assert_eq!(
            q,
            Query::And(vec![
                Query::Literal(LiteralQuery {
                    text: "foo".to_string(),
                    case_sensitive: false,
                }),
                Query::Literal(LiteralQuery {
                    text: "bar".to_string(),
                    case_sensitive: false,
                }),
            ])
        );
    }

    #[test]
    fn test_parse_three_term_and() {
        let q = parse_query("foo bar baz").unwrap();
        assert_eq!(
            q,
            Query::And(vec![
                Query::Literal(LiteralQuery {
                    text: "foo".to_string(),
                    case_sensitive: false,
                }),
                Query::Literal(LiteralQuery {
                    text: "bar".to_string(),
                    case_sensitive: false,
                }),
                Query::Literal(LiteralQuery {
                    text: "baz".to_string(),
                    case_sensitive: false,
                }),
            ])
        );
    }

    // ---- OR tests ----

    #[test]
    fn test_parse_or() {
        let q = parse_query("foo OR bar").unwrap();
        assert_eq!(
            q,
            Query::Or(
                Box::new(Query::Literal(LiteralQuery {
                    text: "foo".to_string(),
                    case_sensitive: false,
                })),
                Box::new(Query::Literal(LiteralQuery {
                    text: "bar".to_string(),
                    case_sensitive: false,
                })),
            )
        );
    }

    #[test]
    fn test_parse_chained_or() {
        // "a OR b OR c" should be left-associative: (a OR b) OR c
        let q = parse_query("a OR b OR c").unwrap();
        assert_eq!(
            q,
            Query::Or(
                Box::new(Query::Or(
                    Box::new(Query::Literal(LiteralQuery {
                        text: "a".to_string(),
                        case_sensitive: false,
                    })),
                    Box::new(Query::Literal(LiteralQuery {
                        text: "b".to_string(),
                        case_sensitive: false,
                    })),
                )),
                Box::new(Query::Literal(LiteralQuery {
                    text: "c".to_string(),
                    case_sensitive: false,
                })),
            )
        );
    }

    // ---- NOT tests ----

    #[test]
    fn test_parse_not() {
        let q = parse_query("NOT foo").unwrap();
        assert_eq!(
            q,
            Query::Not(Box::new(Query::Literal(LiteralQuery {
                text: "foo".to_string(),
                case_sensitive: false,
            })))
        );
    }

    #[test]
    fn test_parse_double_not() {
        let q = parse_query("NOT NOT foo").unwrap();
        assert_eq!(
            q,
            Query::Not(Box::new(Query::Not(Box::new(Query::Literal(
                LiteralQuery {
                    text: "foo".to_string(),
                    case_sensitive: false,
                }
            )))))
        );
    }

    // ---- Case sensitivity tests ----

    #[test]
    fn test_parse_case_sensitive_literal() {
        let q = parse_query("case:yes FooBar").unwrap();
        assert_eq!(
            q,
            Query::Literal(LiteralQuery {
                text: "FooBar".to_string(),
                case_sensitive: true,
            })
        );
    }

    #[test]
    fn test_parse_case_sensitive_phrase() {
        let q = parse_query("case:yes \"Hello World\"").unwrap();
        assert_eq!(
            q,
            Query::Phrase(PhraseQuery {
                text: "Hello World".to_string(),
                case_sensitive: true,
            })
        );
    }

    #[test]
    fn test_parse_case_sensitive_only_affects_next_term() {
        // case:yes only applies to the immediately following term
        let q = parse_query("case:yes foo bar").unwrap();
        assert_eq!(
            q,
            Query::And(vec![
                Query::Literal(LiteralQuery {
                    text: "foo".to_string(),
                    case_sensitive: true,
                }),
                Query::Literal(LiteralQuery {
                    text: "bar".to_string(),
                    case_sensitive: false,
                }),
            ])
        );
    }

    // ---- Combined expression tests ----

    #[test]
    fn test_parse_and_with_or_precedence() {
        // "a b OR c d" should parse as "(a AND b) OR (c AND d)"
        let q = parse_query("a b OR c d").unwrap();
        assert_eq!(
            q,
            Query::Or(
                Box::new(Query::And(vec![
                    Query::Literal(LiteralQuery {
                        text: "a".to_string(),
                        case_sensitive: false,
                    }),
                    Query::Literal(LiteralQuery {
                        text: "b".to_string(),
                        case_sensitive: false,
                    }),
                ])),
                Box::new(Query::And(vec![
                    Query::Literal(LiteralQuery {
                        text: "c".to_string(),
                        case_sensitive: false,
                    }),
                    Query::Literal(LiteralQuery {
                        text: "d".to_string(),
                        case_sensitive: false,
                    }),
                ])),
            )
        );
    }

    #[test]
    fn test_parse_not_with_and() {
        // "foo NOT bar" = AND(foo, NOT(bar))
        let q = parse_query("foo NOT bar").unwrap();
        assert_eq!(
            q,
            Query::And(vec![
                Query::Literal(LiteralQuery {
                    text: "foo".to_string(),
                    case_sensitive: false,
                }),
                Query::Not(Box::new(Query::Literal(LiteralQuery {
                    text: "bar".to_string(),
                    case_sensitive: false,
                }))),
            ])
        );
    }

    #[test]
    fn test_parse_filter_with_literal() {
        let q = parse_query("language:rust parse_query").unwrap();
        assert_eq!(
            q,
            Query::And(vec![
                Query::LanguageFilter(Language::Rust),
                Query::Literal(LiteralQuery {
                    text: "parse_query".to_string(),
                    case_sensitive: false,
                }),
            ])
        );
    }

    #[test]
    fn test_parse_path_and_language_filters() {
        let q = parse_query("path:src/ lang:rs struct").unwrap();
        assert_eq!(
            q,
            Query::And(vec![
                Query::PathFilter("src/".to_string()),
                Query::LanguageFilter(Language::Rust),
                Query::Literal(LiteralQuery {
                    text: "struct".to_string(),
                    case_sensitive: false,
                }),
            ])
        );
    }

    #[test]
    fn test_parse_regex_with_literal() {
        let q = parse_query("/fn\\s+/ parse").unwrap();
        assert_eq!(
            q,
            Query::And(vec![
                Query::Regex(RegexQuery {
                    pattern: "fn\\s+".to_string(),
                    case_sensitive: true,
                }),
                Query::Literal(LiteralQuery {
                    text: "parse".to_string(),
                    case_sensitive: false,
                }),
            ])
        );
    }

    #[test]
    fn test_parse_phrase_and_literal() {
        let q = parse_query("\"fn main\" args").unwrap();
        assert_eq!(
            q,
            Query::And(vec![
                Query::Phrase(PhraseQuery {
                    text: "fn main".to_string(),
                    case_sensitive: false,
                }),
                Query::Literal(LiteralQuery {
                    text: "args".to_string(),
                    case_sensitive: false,
                }),
            ])
        );
    }

    #[test]
    fn test_parse_or_not_word_boundary() {
        // "ORacle" should be a literal, not parsed as OR + "acle"
        let q = parse_query("ORacle").unwrap();
        assert_eq!(
            q,
            Query::Literal(LiteralQuery {
                text: "ORacle".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_parse_not_not_word_boundary() {
        // "NOThing" should be a literal, not parsed as NOT + "hing"
        let q = parse_query("NOThing").unwrap();
        assert_eq!(
            q,
            Query::Literal(LiteralQuery {
                text: "NOThing".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_parse_leading_trailing_whitespace() {
        let q = parse_query("  hello  ").unwrap();
        assert_eq!(
            q,
            Query::Literal(LiteralQuery {
                text: "hello".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_parse_complex_query() {
        // language:rust "fn main" OR /async fn/ NOT test
        let q = parse_query("language:rust \"fn main\" OR /async fn/ NOT test").unwrap();
        // This should parse as: (lang:rust AND "fn main") OR (/async fn/ AND NOT test)
        assert!(matches!(q, Query::Or(_, _)));
    }

    // ---- Display tests ----

    #[test]
    fn test_display_literal() {
        let q = Query::Literal(LiteralQuery {
            text: "hello".to_string(),
            case_sensitive: false,
        });
        assert_eq!(q.to_string(), "hello");
    }

    #[test]
    fn test_display_phrase() {
        let q = Query::Phrase(PhraseQuery {
            text: "fn main".to_string(),
            case_sensitive: false,
        });
        assert_eq!(q.to_string(), "\"fn main\"");
    }

    #[test]
    fn test_display_regex() {
        let q = Query::Regex(RegexQuery {
            pattern: "foo.*bar".to_string(),
            case_sensitive: true,
        });
        assert_eq!(q.to_string(), "/foo.*bar/");
    }

    #[test]
    fn test_display_and() {
        let q = Query::And(vec![
            Query::Literal(LiteralQuery {
                text: "foo".to_string(),
                case_sensitive: false,
            }),
            Query::Literal(LiteralQuery {
                text: "bar".to_string(),
                case_sensitive: false,
            }),
        ]);
        assert_eq!(q.to_string(), "foo bar");
    }

    #[test]
    fn test_display_or() {
        let q = parse_query("foo OR bar").unwrap();
        assert_eq!(q.to_string(), "foo OR bar");
    }

    #[test]
    fn test_display_not() {
        let q = parse_query("NOT foo").unwrap();
        assert_eq!(q.to_string(), "NOT foo");
    }

    // ---- Edge case tests ----

    #[test]
    fn test_parse_literal_with_special_chars() {
        let q = parse_query("foo_bar").unwrap();
        assert_eq!(
            q,
            Query::Literal(LiteralQuery {
                text: "foo_bar".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_parse_literal_with_dots() {
        let q = parse_query("main.rs").unwrap();
        assert_eq!(
            q,
            Query::Literal(LiteralQuery {
                text: "main.rs".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_parse_all_languages() {
        let languages = vec![
            ("rust", Language::Rust),
            ("python", Language::Python),
            ("go", Language::Go),
            ("java", Language::Java),
            ("ruby", Language::Ruby),
            ("shell", Language::Shell),
            ("kotlin", Language::Kotlin),
            ("swift", Language::Swift),
            ("zig", Language::Zig),
        ];
        for (name, expected) in languages {
            let q = parse_query(&format!("lang:{name}")).unwrap();
            assert_eq!(q, Query::LanguageFilter(expected), "failed for lang:{name}");
        }
    }

    #[test]
    fn test_parse_multiple_spaces_between_terms() {
        let q = parse_query("foo   bar").unwrap();
        assert_eq!(
            q,
            Query::And(vec![
                Query::Literal(LiteralQuery {
                    text: "foo".to_string(),
                    case_sensitive: false,
                }),
                Query::Literal(LiteralQuery {
                    text: "bar".to_string(),
                    case_sensitive: false,
                }),
            ])
        );
    }
}
