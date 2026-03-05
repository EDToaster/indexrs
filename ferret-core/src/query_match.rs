//! Recursive query AST verifier for content matching.
//!
//! [`QueryMatcher`] evaluates a [`Query`] AST against raw file content by
//! recursively walking the AST. Leaf nodes (literals, phrases, regexes) are
//! verified using [`ContentVerifier`], while boolean nodes (AND, OR, NOT) apply
//! the appropriate set logic. Metadata-level filters (path, language) pass
//! through as "no content constraint."

use crate::query::Query;
use crate::search::{LineMatch, MatchPattern};
use crate::verify::ContentVerifier;

/// Evaluates a [`Query`] AST against raw file content.
///
/// Reuses [`ContentVerifier`] for leaf-node matching and adds recursive boolean
/// logic for AND, OR, and NOT nodes.
pub struct QueryMatcher<'a> {
    query: &'a Query,
    context_lines: u32,
}

impl<'a> QueryMatcher<'a> {
    /// Create a new `QueryMatcher` for the given query and context line count.
    pub fn new(query: &'a Query, context_lines: u32) -> Self {
        Self {
            query,
            context_lines,
        }
    }

    /// Match the query against content. Returns `Some(lines)` if the file
    /// matches, or `None` if no match.
    pub fn matches(&self, content: &[u8]) -> Option<Vec<LineMatch>> {
        self.eval(self.query, content)
    }

    /// Recursively evaluate a query node against content.
    fn eval(&self, query: &Query, content: &[u8]) -> Option<Vec<LineMatch>> {
        match query {
            Query::Literal(lit) => {
                let pattern = if lit.case_sensitive {
                    MatchPattern::Literal(lit.text.clone())
                } else {
                    MatchPattern::LiteralCaseInsensitive(lit.text.clone())
                };
                let verifier = ContentVerifier::new(pattern, self.context_lines);
                let lines = verifier.verify(content);
                if lines.is_empty() { None } else { Some(lines) }
            }
            Query::Phrase(ph) => {
                let pattern = if ph.case_sensitive {
                    MatchPattern::Literal(ph.text.clone())
                } else {
                    MatchPattern::LiteralCaseInsensitive(ph.text.clone())
                };
                let verifier = ContentVerifier::new(pattern, self.context_lines);
                let lines = verifier.verify(content);
                if lines.is_empty() { None } else { Some(lines) }
            }
            Query::Regex(re) => {
                let effective_pattern = if re.case_sensitive {
                    re.pattern.clone()
                } else {
                    format!("(?i){}", re.pattern)
                };
                let pattern = MatchPattern::Regex(effective_pattern);
                let verifier = ContentVerifier::new(pattern, self.context_lines);
                let lines = verifier.verify(content);
                if lines.is_empty() { None } else { Some(lines) }
            }
            Query::PathFilter(_) | Query::LanguageFilter(_) => {
                // Metadata-level filters have no content constraint.
                Some(vec![])
            }
            Query::Not(inner) => {
                let inner_result = self.eval(inner, content);
                match inner_result {
                    Some(_) => None,
                    None => Some(vec![]),
                }
            }
            Query::Or(left, right) => {
                let left_result = self.eval(left, content);
                let right_result = self.eval(right, content);
                match (left_result, right_result) {
                    (None, None) => None,
                    (Some(lines), None) | (None, Some(lines)) => Some(lines),
                    (Some(left_lines), Some(right_lines)) => {
                        Some(merge_line_matches(left_lines, right_lines))
                    }
                }
            }
            Query::And(children) => {
                let mut merged: Vec<LineMatch> = Vec::new();
                for child in children {
                    match self.eval(child, content) {
                        None => return None,
                        Some(lines) => {
                            merged.extend(lines);
                        }
                    }
                }
                // Sort and dedup
                merged.sort_by_key(|m| m.line_number);
                merged.dedup_by_key(|m| m.line_number);
                Some(merged)
            }
        }
    }
}

/// Merge two sets of line matches, sort by line number, and deduplicate.
fn merge_line_matches(mut left: Vec<LineMatch>, right: Vec<LineMatch>) -> Vec<LineMatch> {
    left.extend(right);
    left.sort_by_key(|m| m.line_number);
    left.dedup_by_key(|m| m.line_number);
    left
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{LiteralQuery, PhraseQuery, RegexQuery};
    use crate::types::Language;

    #[test]
    fn test_literal_match() {
        let query = Query::Literal(LiteralQuery {
            text: "hello".to_string(),
            case_sensitive: false,
        });
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"say hello world\n";
        let result = matcher.matches(content);
        assert!(result.is_some());
        let lines = result.unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_number, 1);
        assert!(lines[0].content.contains("hello"));
    }

    #[test]
    fn test_literal_no_match() {
        let query = Query::Literal(LiteralQuery {
            text: "foobar".to_string(),
            case_sensitive: false,
        });
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"nothing relevant here\n";
        let result = matcher.matches(content);
        assert!(result.is_none());
    }

    #[test]
    fn test_and_both_match() {
        let query = Query::And(vec![
            Query::Literal(LiteralQuery {
                text: "hello".to_string(),
                case_sensitive: false,
            }),
            Query::Literal(LiteralQuery {
                text: "world".to_string(),
                case_sensitive: false,
            }),
        ]);
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"hello there\nworld here\n";
        let result = matcher.matches(content);
        assert!(result.is_some());
        let lines = result.unwrap();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_and_one_missing() {
        let query = Query::And(vec![
            Query::Literal(LiteralQuery {
                text: "hello".to_string(),
                case_sensitive: false,
            }),
            Query::Literal(LiteralQuery {
                text: "nonexistent".to_string(),
                case_sensitive: false,
            }),
        ]);
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"hello there\n";
        let result = matcher.matches(content);
        assert!(result.is_none());
    }

    #[test]
    fn test_or_one_matches() {
        let query = Query::Or(
            Box::new(Query::Literal(LiteralQuery {
                text: "hello".to_string(),
                case_sensitive: false,
            })),
            Box::new(Query::Literal(LiteralQuery {
                text: "nonexistent".to_string(),
                case_sensitive: false,
            })),
        );
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"hello there\n";
        let result = matcher.matches(content);
        assert!(result.is_some());
        let lines = result.unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].content.contains("hello"));
    }

    #[test]
    fn test_or_neither_matches() {
        let query = Query::Or(
            Box::new(Query::Literal(LiteralQuery {
                text: "nonexistent".to_string(),
                case_sensitive: false,
            })),
            Box::new(Query::Literal(LiteralQuery {
                text: "alsonothere".to_string(),
                case_sensitive: false,
            })),
        );
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"hello world\n";
        let result = matcher.matches(content);
        assert!(result.is_none());
    }

    #[test]
    fn test_not_excludes() {
        let query = Query::Not(Box::new(Query::Literal(LiteralQuery {
            text: "hello".to_string(),
            case_sensitive: false,
        })));
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"hello world\n";
        let result = matcher.matches(content);
        assert!(result.is_none());
    }

    #[test]
    fn test_not_includes() {
        let query = Query::Not(Box::new(Query::Literal(LiteralQuery {
            text: "nonexistent".to_string(),
            case_sensitive: false,
        })));
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"hello world\n";
        let result = matcher.matches(content);
        assert!(result.is_some());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_regex_match() {
        let query = Query::Regex(RegexQuery {
            pattern: r"fn\s+\w+".to_string(),
            case_sensitive: true,
        });
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"fn main() {}\nlet x = 1;\n";
        let result = matcher.matches(content);
        assert!(result.is_some());
        let lines = result.unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_number, 1);
    }

    #[test]
    fn test_phrase_match() {
        let query = Query::Phrase(PhraseQuery {
            text: "hello world".to_string(),
            case_sensitive: false,
        });
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"say Hello World to all\n";
        let result = matcher.matches(content);
        assert!(result.is_some());
        let lines = result.unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].content.contains("Hello World"));
    }

    #[test]
    fn test_language_filter_passes_through() {
        let query = Query::LanguageFilter(Language::Rust);
        let matcher = QueryMatcher::new(&query, 0);
        let content = b"fn main() {}\n";
        let result = matcher.matches(content);
        assert!(result.is_some());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_complex_and_or_not() {
        // (println OR eprintln) AND NOT deprecated
        let query = Query::And(vec![
            Query::Or(
                Box::new(Query::Literal(LiteralQuery {
                    text: "println".to_string(),
                    case_sensitive: false,
                })),
                Box::new(Query::Literal(LiteralQuery {
                    text: "eprintln".to_string(),
                    case_sensitive: false,
                })),
            ),
            Query::Not(Box::new(Query::Literal(LiteralQuery {
                text: "deprecated".to_string(),
                case_sensitive: false,
            }))),
        ]);
        let matcher = QueryMatcher::new(&query, 0);

        // File with println but no "deprecated" -> matches
        let content1 = b"fn main() {\n    println!(\"hi\");\n}\n";
        let result1 = matcher.matches(content1);
        assert!(result1.is_some());

        // File with eprintln but also "deprecated" -> no match
        let content2 = b"// deprecated\neprintln!(\"error\");\n";
        let result2 = matcher.matches(content2);
        assert!(result2.is_none());

        // File with neither println nor eprintln -> no match
        let content3 = b"fn helper() {}\n";
        let result3 = matcher.matches(content3);
        assert!(result3.is_none());
    }
}
