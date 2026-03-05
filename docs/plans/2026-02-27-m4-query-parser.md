# Query Parser Implementation Plan (HHC-46)

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a recursive descent parser for the ferret query language. The parser turns a query string into a `Query` AST that downstream modules (trigram extraction, query planner, verification) consume.

**Architecture:** A single `query.rs` module in `ferret-indexer-core` containing the `Query` AST enum, supporting structs (`LiteralQuery`, `RegexQuery`, `PhraseQuery`), a `parse_query()` public entry point, and internal parser helpers. The parser is hand-written recursive descent (~250 lines), no parser combinator library. The `regex` crate (already a dependency) is used to validate `/pattern/` syntax at parse time.

**Tech Stack:** Rust 2024, regex crate (already in Cargo.toml), existing `Language` enum and `IndexError::QueryParse` variant

**Prerequisite:** ASCII case-fold trigrams plan (`2026-02-27-ascii-casefold-trigrams.md`) must be implemented first. The index stores lowercase-folded trigrams, so:
- The `case_sensitive` flag on `LiteralQuery`/`RegexQuery`/`PhraseQuery` affects **verification only**, not trigram lookup. All trigram extraction produces lowercase trigrams regardless of case_sensitive.
- `case:yes` makes the verification step do exact-case matching, but candidates are still found via the case-folded index (with slightly more false positives).

**Grammar (informal):**

```
query      = or_expr
or_expr    = and_expr ("OR" and_expr)*
and_expr   = unary_expr+              // implicit AND (space-separated)
unary_expr = "NOT" unary_expr | primary
primary    = "/" REGEX_BODY "/"       // regex
           | '"' PHRASE_BODY '"'      // exact phrase
           | "path:" PATH_VALUE      // path filter
           | ("language:" | "lang:") LANG_VALUE  // language filter
           | "case:yes" unary_expr   // case-sensitive modifier
           | LITERAL_WORD            // bare text (until space or special char)
```

**Operator precedence (highest to lowest):** primary > NOT > AND (implicit) > OR

---

## Task 1: Add query module skeleton with AST types

**Files:**
- Create: `ferret-indexer-core/src/query.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Write the failing test

Create `ferret-indexer-core/src/query.rs` with AST types and a test that constructs them:

```rust
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

#[cfg(test)]
mod tests {
    use super::*;

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
            panic!("expected And");
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
        assert!(matches!(q, Query::Or(_, _)));
    }
}
```

### Step 2: Register the module in lib.rs

Add to `ferret-indexer-core/src/lib.rs` (in the module list, alphabetically):

```rust
pub mod query;
```

And add re-exports:

```rust
pub use query::{LiteralQuery, PhraseQuery, Query, RegexQuery};
```

### Step 3: Run test to verify it passes

Run: `cargo test -p ferret-indexer-core -- test_ast_construction -v`

Expected: PASS

### Step 4: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 5: Commit

```bash
git add ferret-indexer-core/src/query.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(query): add query module skeleton with AST types"
```

---

## Task 2: Implement the parser tokenizer / character-level helpers

**Files:**
- Modify: `ferret-indexer-core/src/query.rs`

The parser operates directly on the input string using a cursor-based approach (byte position into the string). No separate lexer/tokenizer phase -- the parser peeks/consumes characters directly.

### Step 1: Write failing tests for `parse_query` basic literal

Add tests to the `tests` module:

```rust
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
```

### Step 2: Implement the Parser struct and `parse_query` entry point

Add above the `tests` module:

```rust
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

/// Recursive descent parser state.
struct Parser<'a> {
    input: &'a str,
    pos: usize,
    /// Tracks whether the next term should be case-sensitive.
    case_sensitive: bool,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Parser {
            input,
            pos: 0,
            case_sensitive: false,
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    /// Check if the remaining input starts with the given string (case-sensitive).
    fn starts_with(&self, s: &str) -> bool {
        self.input[self.pos..].starts_with(s)
    }

    /// Check if the remaining input starts with the given keyword followed by
    /// whitespace or EOF (to avoid matching prefixes like "ORacle" as "OR").
    fn starts_with_keyword(&self, keyword: &str) -> bool {
        if !self.input[self.pos..].starts_with(keyword) {
            return false;
        }
        let after = self.pos + keyword.len();
        if after >= self.input.len() {
            return true;
        }
        let next_char = self.input[after..].chars().next().unwrap();
        next_char.is_ascii_whitespace() || next_char == '"' || next_char == '/'
    }

    // Grammar: or_expr = and_expr ("OR" and_expr)*
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

    // Grammar: and_expr = unary_expr+
    fn parse_and(&mut self) -> Result<Query, IndexError> {
        let mut children = Vec::new();
        children.push(self.parse_unary()?);

        loop {
            self.skip_whitespace();
            if self.is_eof() {
                break;
            }
            // Stop if we see "OR" or ")" — those belong to the parent
            if self.starts_with_keyword("OR") {
                break;
            }
            // Try to parse another unary; if it fails at the start, we're done
            let saved_pos = self.pos;
            match self.parse_unary() {
                Ok(q) => children.push(q),
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

    // Grammar: unary_expr = "NOT" unary_expr | primary
    fn parse_unary(&mut self) -> Result<Query, IndexError> {
        self.skip_whitespace();

        if self.starts_with_keyword("NOT") {
            self.pos += 3; // consume "NOT"
            self.skip_whitespace();
            let inner = self.parse_unary()?;
            return Ok(Query::Not(Box::new(inner)));
        }

        self.parse_primary()
    }

    // Grammar: primary = regex | phrase | path_filter | lang_filter | case_modifier | literal
    fn parse_primary(&mut self) -> Result<Query, IndexError> {
        self.skip_whitespace();

        if self.is_eof() {
            return Err(IndexError::QueryParse(
                "unexpected end of query".to_string(),
            ));
        }

        match self.peek() {
            Some('/') => self.parse_regex(),
            Some('"') => self.parse_phrase(),
            _ => {
                // Check for prefix filters
                if self.starts_with("path:") {
                    return self.parse_path_filter();
                }
                if self.starts_with("language:") || self.starts_with("lang:") {
                    return self.parse_language_filter();
                }
                if self.starts_with("case:yes") {
                    return self.parse_case_modifier();
                }
                self.parse_literal()
            }
        }
    }

    fn parse_literal(&mut self) -> Result<Query, IndexError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() || c == '"' || c == '/' {
                break;
            }
            self.pos += c.len_utf8();
        }

        if self.pos == start {
            return Err(IndexError::QueryParse(format!(
                "expected literal at position {}",
                self.pos
            )));
        }

        let text = self.input[start..self.pos].to_string();
        let case_sensitive = self.case_sensitive;
        self.case_sensitive = false; // reset after use
        Ok(Query::Literal(LiteralQuery {
            text,
            case_sensitive,
        }))
    }

    fn parse_phrase(&mut self) -> Result<Query, IndexError> {
        // Consume opening '"'
        self.pos += 1;
        let start = self.pos;

        while let Some(c) = self.peek() {
            if c == '"' {
                let text = self.input[start..self.pos].to_string();
                self.pos += 1; // consume closing '"'
                let case_sensitive = self.case_sensitive;
                self.case_sensitive = false;
                return Ok(Query::Phrase(PhraseQuery {
                    text,
                    case_sensitive,
                }));
            }
            self.pos += c.len_utf8();
        }

        Err(IndexError::QueryParse(
            "unterminated phrase (missing closing \")".to_string(),
        ))
    }

    fn parse_regex(&mut self) -> Result<Query, IndexError> {
        // Consume opening '/'
        self.pos += 1;
        let start = self.pos;

        while let Some(c) = self.peek() {
            if c == '/' {
                let pattern = self.input[start..self.pos].to_string();
                self.pos += 1; // consume closing '/'

                // Validate the regex pattern at parse time
                if regex::Regex::new(&pattern).is_err() {
                    return Err(IndexError::QueryParse(format!(
                        "invalid regex pattern: {pattern}"
                    )));
                }

                let case_sensitive = !self.case_sensitive;
                // For regex, default is case-sensitive (true); case:yes is a no-op,
                // but we follow the convention: case_sensitive field on the struct.
                // Actually: regex is case-sensitive by default. case:yes doesn't
                // change it. We store the flag for consistency.
                let case_sensitive_flag = if self.case_sensitive {
                    self.case_sensitive = false;
                    true
                } else {
                    true // regex is case-sensitive by default
                };

                return Ok(Query::Regex(RegexQuery {
                    pattern,
                    case_sensitive: case_sensitive_flag,
                }));
            }
            // Allow escaped forward slashes inside regex
            if c == '\\' {
                self.pos += c.len_utf8();
                if !self.is_eof() {
                    let next = self.peek().unwrap();
                    self.pos += next.len_utf8();
                }
                continue;
            }
            self.pos += c.len_utf8();
        }

        Err(IndexError::QueryParse(
            "unterminated regex (missing closing /)".to_string(),
        ))
    }

    fn parse_path_filter(&mut self) -> Result<Query, IndexError> {
        self.pos += "path:".len();
        let start = self.pos;

        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                break;
            }
            self.pos += c.len_utf8();
        }

        if self.pos == start {
            return Err(IndexError::QueryParse(
                "expected path value after 'path:'".to_string(),
            ));
        }

        Ok(Query::PathFilter(self.input[start..self.pos].to_string()))
    }

    fn parse_language_filter(&mut self) -> Result<Query, IndexError> {
        if self.starts_with("language:") {
            self.pos += "language:".len();
        } else {
            self.pos += "lang:".len();
        }
        let start = self.pos;

        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                break;
            }
            self.pos += c.len_utf8();
        }

        if self.pos == start {
            return Err(IndexError::QueryParse(
                "expected language value after 'language:' or 'lang:'".to_string(),
            ));
        }

        let lang_str = &self.input[start..self.pos];
        let language = match_language(lang_str)?;
        Ok(Query::LanguageFilter(language))
    }

    fn parse_case_modifier(&mut self) -> Result<Query, IndexError> {
        self.pos += "case:yes".len();

        // Check that case:yes is followed by whitespace or EOF
        if !self.is_eof() {
            if let Some(c) = self.peek() {
                if !c.is_ascii_whitespace() {
                    // It's something like "case:yesterday" — treat as literal
                    self.pos -= "case:yes".len();
                    return self.parse_literal();
                }
            }
        }

        self.skip_whitespace();
        self.case_sensitive = true;
        self.parse_unary()
    }
}

/// Match a language string to a `Language` enum variant.
///
/// Accepts both full names (case-insensitive) and common file extensions.
fn match_language(s: &str) -> Result<Language, IndexError> {
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
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
        _ => Err(IndexError::QueryParse(format!(
            "unknown language: {s}"
        ))),
    }
}
```

### Step 3: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_parse_single_literal test_parse_empty_query test_parse_whitespace_only -v`

Expected: PASS

### Step 4: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 5: Commit

```bash
git add ferret-indexer-core/src/query.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(query): implement recursive descent query parser"
```

---

## Task 3: Add comprehensive tests for all syntax variants

**Files:**
- Modify: `ferret-indexer-core/src/query.rs`

### Step 1: Write tests for phrase and regex parsing

Add to the `tests` module:

```rust
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
```

### Step 2: Write tests for filters

```rust
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
```

### Step 3: Write tests for boolean operators

```rust
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
```

### Step 4: Write tests for case sensitivity

```rust
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
```

### Step 5: Write tests for combined expressions

```rust
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
```

### Step 6: Run all query tests

Run: `cargo test -p ferret-indexer-core -- query -v`

Expected: All tests PASS.

### Step 7: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: No errors, no warnings, formatting OK.

### Step 8: Commit

```bash
git add ferret-indexer-core/src/query.rs
git commit -m "test(query): add comprehensive tests for all query syntax variants"
```

---

## Task 4: Add Display impl and edge case tests

**Files:**
- Modify: `ferret-indexer-core/src/query.rs`

### Step 1: Implement Display for Query

Add a `Display` implementation so queries can be pretty-printed for debugging and error messages:

```rust
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
```

### Step 2: Write Display tests

```rust
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
```

### Step 3: Write edge case tests

```rust
// ---- Edge case tests ----

#[test]
fn test_parse_literal_with_special_chars() {
    // Literals can contain underscores, hyphens, dots, colons (if not a known filter)
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
    // Verify a sample of language filters work
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
```

### Step 4: Run all tests

Run: `cargo test -p ferret-indexer-core -- query -v`

Expected: All tests PASS.

### Step 5: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: No errors, no warnings, formatting OK.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/query.rs
git commit -m "feat(query): add Display impl and edge case tests"
```

---

## Task 5: Update lib.rs re-exports and run final verification

**Files:**
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Ensure lib.rs has all re-exports

The following should be in `lib.rs`:

```rust
pub mod query;
// ... existing modules ...

pub use query::{parse_query, LiteralQuery, PhraseQuery, Query, RegexQuery};
```

### Step 2: Run full test suite

Run: `cargo test --workspace`

Expected: All tests pass (existing + new query tests).

### Step 3: Run lints and formatting

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: No warnings, formatting OK.

### Step 4: Verify the module structure

At this point, `ferret-indexer-core/src/query.rs` should contain (~250 lines):
- `Query` enum — the shared AST type for all downstream M4 modules
  - `Literal(LiteralQuery)` — bare text substring match
  - `Regex(RegexQuery)` — `/pattern/` regex match
  - `Phrase(PhraseQuery)` — `"exact phrase"` match
  - `PathFilter(String)` — `path:prefix` filter
  - `LanguageFilter(Language)` — `language:rust`/`lang:rs` filter
  - `Not(Box<Query>)` — exclusion
  - `Or(Box<Query>, Box<Query>)` — union
  - `And(Vec<Query>)` — implicit AND
- `LiteralQuery` struct — `text: String`, `case_sensitive: bool`
- `RegexQuery` struct — `pattern: String`, `case_sensitive: bool`
- `PhraseQuery` struct — `text: String`, `case_sensitive: bool`
- `parse_query(input: &str) -> Result<Query, IndexError>` — public entry point
- `match_language(s: &str) -> Result<Language, IndexError>` — language string resolver
- `Parser` struct — internal recursive descent parser
- `Display` impl for `Query`
- Comprehensive test suite covering all syntax variants

### Step 5: Final commit if any cleanup was needed

```bash
git add -A
git commit -m "chore(query): finalize query parser module"
```

---

## Reference: Grammar Summary

```
query       = or_expr
or_expr     = and_expr ("OR" and_expr)*
and_expr    = unary_expr (unary_expr)*    // implicit AND
unary_expr  = "NOT" unary_expr | primary
primary     = "/" REGEX "/"
            | '"' PHRASE '"'
            | "path:" VALUE
            | ("language:" | "lang:") LANG
            | "case:yes" unary_expr
            | LITERAL
```

Operator precedence (highest to lowest):
1. Primary (literals, phrases, regex, filters)
2. NOT (prefix unary)
3. AND (implicit, space-separated)
4. OR (explicit keyword)

## Reference: How Downstream Modules Consume the AST

| Module | Query variants it cares about | What it does |
|--------|-------------------------------|--------------|
| Trigram extraction (HHC-47) | `Literal`, `Phrase`, `Regex`, `And`, `Or`, `Not` | Extracts trigrams for index lookups. `Not` produces no trigrams. `Or` unions, `And` intersects. |
| Query planner (HHC-48) | All variants | Separates into trigram lookups, metadata filters, and boolean structure. Orders by selectivity. |
| Verification (HHC-49) | `Literal`, `Phrase`, `Regex`, `And`, `Or`, `Not`, `PathFilter`, `LanguageFilter` | Confirms matches in actual content. Handles case sensitivity. |
| Result formatting (HHC-50) | None directly (consumes `SearchResult`) | Formats results; may display the original query string. |
