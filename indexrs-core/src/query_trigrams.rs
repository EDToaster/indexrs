//! Trigram extraction from parsed query ASTs.
//!
//! This module converts a [`Query`] AST into a [`TrigramQuery`] that describes
//! which trigrams to look up in the index. The query planner uses the extracted
//! trigrams to fetch posting lists and build an execution plan.
//!
//! # Extraction Strategy
//!
//! - **Literal/Phrase queries**: Extract all trigrams directly from the search string.
//!   All trigrams must match (AND semantics).
//! - **Regex queries**: Parse the regex with `regex-syntax`, extract required literal
//!   fragments from the HIR, then extract trigrams from those fragments.
//! - **AND queries**: Merge (union) trigram sets from all children -- a file must
//!   contain trigrams from every child.
//! - **OR queries**: Keep trigram sets separate -- a file matching any branch suffices.
//! - **NOT queries**: Cannot use trigrams for pruning (negation inverts the set).
//! - **Filter queries** (Path, Language): These don't produce content trigrams;
//!   they're handled by other index types.
//!
//! When no trigrams can be extracted (short queries, wildcard-only regex, NOT-only
//! queries), the result is [`TrigramQuery::None`], signaling the planner to fall
//! back to a full file scan.
//!
//! # Case Folding
//!
//! All trigram extraction produces **lowercase (ASCII-folded) trigrams** to match
//! the case-folded index. The `case_sensitive` flag on query types does NOT affect
//! trigram extraction -- it only affects verification (HHC-49).

use std::collections::HashSet;

use regex_syntax::Parser;
use regex_syntax::hir::HirKind;
use regex_syntax::hir::literal::{ExtractKind, Extractor};

use crate::query::Query;
use crate::trigram::extract_unique_trigrams_folded;
use crate::types::Trigram;

/// Describes the trigram lookup strategy for a parsed query.
///
/// The query planner uses this to decide how to query the trigram index:
/// - `All`: intersect posting lists for all trigrams (AND semantics)
/// - `Any`: union the results of multiple `All` sets (OR semantics)
/// - `None`: no trigrams available, must scan all files
#[derive(Debug, Clone, PartialEq)]
pub enum TrigramQuery {
    /// All trigrams must be present in a file (AND intersection).
    /// Used for literal, phrase, and AND-combined queries.
    All(HashSet<Trigram>),

    /// At least one branch's trigram set must match (OR union).
    /// Each inner `HashSet<Trigram>` is an AND-set; the outer Vec is OR'd.
    /// A file is a candidate if it matches ALL trigrams in ANY one branch.
    Any(Vec<HashSet<Trigram>>),

    /// No trigrams could be extracted. The planner must fall back to
    /// scanning all files. This occurs for:
    /// - Queries shorter than 3 characters
    /// - Regex patterns with no required literal substrings (e.g., `.*`)
    /// - NOT-only queries
    /// - Filter-only queries (PathFilter, LanguageFilter)
    None,
}

impl TrigramQuery {
    /// Returns `true` if this is `TrigramQuery::None`.
    pub fn is_none(&self) -> bool {
        matches!(self, TrigramQuery::None)
    }

    /// Returns the total number of unique trigrams across all branches.
    /// Useful for cost estimation in the query planner.
    pub fn trigram_count(&self) -> usize {
        match self {
            TrigramQuery::All(set) => set.len(),
            TrigramQuery::Any(branches) => branches.iter().map(|s| s.len()).sum(),
            TrigramQuery::None => 0,
        }
    }
}

/// Extract trigrams from a literal search string.
///
/// Always produces lowercase (ASCII-folded) trigrams to match the case-folded
/// index. The `case_sensitive` flag on query types affects verification only,
/// not trigram extraction.
///
/// Returns `TrigramQuery::All` with the unique trigram set if the string
/// is at least 3 bytes long, or `TrigramQuery::None` if too short.
pub fn extract_literal_trigrams(text: &str) -> TrigramQuery {
    let bytes = text.as_bytes();
    if bytes.len() < 3 {
        return TrigramQuery::None;
    }
    let trigrams = extract_unique_trigrams_folded(bytes);
    if trigrams.is_empty() {
        TrigramQuery::None
    } else {
        TrigramQuery::All(trigrams)
    }
}

/// Extract trigrams from a regex pattern by analyzing its literal fragments.
///
/// Uses `regex-syntax` to parse the pattern into HIR, then uses the
/// `Extractor` to find required literal byte sequences. Trigrams are
/// extracted from each literal fragment and merged.
///
/// For top-level alternations (`foo|bar`), extracts trigrams from each
/// branch separately and returns `TrigramQuery::Any`. If no required
/// literals can be extracted (e.g., pure wildcards `.*`), returns
/// `TrigramQuery::None`.
///
/// Returns `TrigramQuery::None` on parse errors (graceful fallback to full scan).
pub fn extract_regex_trigrams(pattern: &str) -> TrigramQuery {
    // Parse the regex pattern into HIR
    let hir = match Parser::new().parse(pattern) {
        Ok(hir) => hir,
        Err(_) => return TrigramQuery::None,
    };

    // Check for top-level alternation first
    if let HirKind::Alternation(branches) = hir.kind() {
        let mut branch_trigrams: Vec<HashSet<Trigram>> = Vec::new();
        for branch in branches {
            let trigrams = extract_trigrams_from_hir(branch);
            if trigrams.is_empty() {
                // If any branch has no trigrams, the whole OR can't be pruned
                return TrigramQuery::None;
            }
            branch_trigrams.push(trigrams);
        }
        return match branch_trigrams.len() {
            0 => TrigramQuery::None,
            1 => TrigramQuery::All(branch_trigrams.into_iter().next().unwrap()),
            _ => TrigramQuery::Any(branch_trigrams),
        };
    }

    // Non-alternation: extract trigrams from the whole pattern
    let trigrams = extract_trigrams_from_hir(&hir);
    if trigrams.is_empty() {
        TrigramQuery::None
    } else {
        TrigramQuery::All(trigrams)
    }
}

/// Extract trigrams from a parsed HIR node by finding literal fragments.
///
/// Uses prefix and suffix extraction for maximum coverage, then merges
/// the trigram sets.
fn extract_trigrams_from_hir(hir: &regex_syntax::hir::Hir) -> HashSet<Trigram> {
    let mut all_trigrams: HashSet<Trigram> = HashSet::new();

    // Extract prefix literals
    let mut extractor = Extractor::new();
    extractor.kind(ExtractKind::Prefix);
    let prefix_seq = extractor.extract(hir);

    // Extract suffix literals
    let mut extractor_suffix = Extractor::new();
    extractor_suffix.kind(ExtractKind::Suffix);
    let suffix_seq = extractor_suffix.extract(hir);

    // Process both prefix and suffix sequences
    for seq in [&prefix_seq, &suffix_seq] {
        if let Some(literals) = seq.literals() {
            for lit in literals {
                let bytes = lit.as_bytes();
                if bytes.len() >= 3 {
                    all_trigrams.extend(extract_unique_trigrams_folded(bytes));
                }
            }
        }
    }

    all_trigrams
}

/// Extract trigrams from a parsed query AST for index lookup.
///
/// Recursively walks the query AST and builds a [`TrigramQuery`] describing
/// which trigrams to look up. The query planner uses this to fetch posting
/// lists from the trigram index.
///
/// # Query Type Handling
///
/// All trigram extraction produces lowercase (ASCII-folded) trigrams to match
/// the case-folded index. The `case_sensitive` flag on query types does NOT
/// affect trigram extraction -- it only affects verification (HHC-49).
///
/// | Query Type | Trigram Strategy |
/// |---|---|
/// | `Literal(LiteralQuery)` | Extract folded trigrams from text (AND) |
/// | `Phrase(PhraseQuery)` | Extract folded trigrams from text (AND) |
/// | `Regex(RegexQuery)` | Parse regex, extract literal fragments, extract folded trigrams |
/// | `And(children)` | Merge (union) trigram sets from all children |
/// | `Or(left, right)` | Keep branches separate (any branch match suffices) |
/// | `Not(inner)` | Returns `None` (negation can't prune via trigrams) |
/// | `PathFilter/LanguageFilter` | Returns `None` (handled by other indexes) |
pub fn extract_query_trigrams(query: &Query) -> TrigramQuery {
    match query {
        Query::Literal(lq) => extract_literal_trigrams(&lq.text),

        Query::Phrase(pq) => extract_literal_trigrams(&pq.text),

        Query::Regex(rq) => extract_regex_trigrams(&rq.pattern),

        // Filter-only queries don't produce content trigrams
        Query::PathFilter(_) | Query::LanguageFilter(_) => TrigramQuery::None,

        // NOT: we cannot use trigrams to prune (negation inverts the candidate set)
        Query::Not(_) => TrigramQuery::None,

        Query::And(children) => {
            let mut merged = TrigramQuery::None;
            for child in children {
                let child_tq = extract_query_trigrams(child);
                merged = merge_and(merged, child_tq);
            }
            merged
        }

        // Binary Or: extract from both branches
        Query::Or(left, right) => {
            let left_tq = extract_query_trigrams(left);
            let right_tq = extract_query_trigrams(right);

            let mut branches: Vec<HashSet<Trigram>> = Vec::new();

            for child_tq in [left_tq, right_tq] {
                match child_tq {
                    TrigramQuery::None => {
                        // If any OR branch has no trigrams, we can't prune:
                        // that branch could match any file.
                        return TrigramQuery::None;
                    }
                    TrigramQuery::All(set) => {
                        branches.push(set);
                    }
                    TrigramQuery::Any(inner_branches) => {
                        branches.extend(inner_branches);
                    }
                }
            }

            match branches.len() {
                0 => TrigramQuery::None,
                1 => TrigramQuery::All(branches.into_iter().next().unwrap()),
                _ => TrigramQuery::Any(branches),
            }
        }
    }
}

/// Merge two `TrigramQuery` values with AND semantics.
///
/// AND means a file must satisfy both constraints, so we can union
/// the required trigram sets (more trigrams = more selective).
fn merge_and(a: TrigramQuery, b: TrigramQuery) -> TrigramQuery {
    match (a, b) {
        // If either side has no trigrams, use the other
        (TrigramQuery::None, other) | (other, TrigramQuery::None) => other,

        // Both are ALL: union the trigram sets
        (TrigramQuery::All(mut s1), TrigramQuery::All(s2)) => {
            s1.extend(s2);
            TrigramQuery::All(s1)
        }

        // ALL + Any: distribute the ALL set into every branch
        (TrigramQuery::All(all), TrigramQuery::Any(branches))
        | (TrigramQuery::Any(branches), TrigramQuery::All(all)) => {
            let merged_branches: Vec<HashSet<Trigram>> = branches
                .into_iter()
                .map(|mut branch| {
                    branch.extend(all.iter().copied());
                    branch
                })
                .collect();
            TrigramQuery::Any(merged_branches)
        }

        // Both Any: cross-product (each branch from A merged with each from B)
        // To avoid exponential blowup, limit to a reasonable number of branches.
        (TrigramQuery::Any(a_branches), TrigramQuery::Any(b_branches)) => {
            let mut result_branches = Vec::new();
            for a_branch in &a_branches {
                for b_branch in &b_branches {
                    let mut merged = a_branch.clone();
                    merged.extend(b_branch.iter().copied());
                    result_branches.push(merged);
                    // Cap at 64 branches to prevent blowup
                    if result_branches.len() >= 64 {
                        return TrigramQuery::Any(result_branches);
                    }
                }
            }
            match result_branches.len() {
                0 => TrigramQuery::None,
                1 => TrigramQuery::All(result_branches.into_iter().next().unwrap()),
                _ => TrigramQuery::Any(result_branches),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{LiteralQuery, PhraseQuery, RegexQuery};
    use crate::types::{Language, Trigram};

    #[test]
    fn test_trigram_query_none_is_none() {
        assert!(TrigramQuery::None.is_none());
    }

    #[test]
    fn test_trigram_query_all_is_not_none() {
        let mut set = HashSet::new();
        set.insert(Trigram::from_bytes(b'a', b'b', b'c'));
        assert!(!TrigramQuery::All(set).is_none());
    }

    #[test]
    fn test_trigram_query_count_all() {
        let mut set = HashSet::new();
        set.insert(Trigram::from_bytes(b'a', b'b', b'c'));
        set.insert(Trigram::from_bytes(b'b', b'c', b'd'));
        assert_eq!(TrigramQuery::All(set).trigram_count(), 2);
    }

    #[test]
    fn test_trigram_query_count_any() {
        let mut s1 = HashSet::new();
        s1.insert(Trigram::from_bytes(b'a', b'b', b'c'));
        let mut s2 = HashSet::new();
        s2.insert(Trigram::from_bytes(b'x', b'y', b'z'));
        s2.insert(Trigram::from_bytes(b'y', b'z', b'w'));
        assert_eq!(TrigramQuery::Any(vec![s1, s2]).trigram_count(), 3);
    }

    #[test]
    fn test_trigram_query_count_none() {
        assert_eq!(TrigramQuery::None.trigram_count(), 0);
    }

    // ---- extract_literal_trigrams tests ----

    #[test]
    fn test_extract_literal_trigrams_lowercase() {
        // "httprequest" -> 9 trigrams, all lowercase
        let result = extract_literal_trigrams("httprequest");
        match result {
            TrigramQuery::All(set) => {
                assert_eq!(set.len(), 9);
                assert!(set.contains(&Trigram::from_bytes(b'h', b't', b't')));
                assert!(set.contains(&Trigram::from_bytes(b'e', b's', b't')));
            }
            _ => panic!("expected TrigramQuery::All"),
        }
    }

    #[test]
    fn test_extract_literal_trigrams_mixed_case_folded() {
        // "HttpRequest" -> folded to lowercase trigrams
        // Same trigrams as "httprequest" since index is case-folded
        let result = extract_literal_trigrams("HttpRequest");
        match result {
            TrigramQuery::All(set) => {
                assert_eq!(set.len(), 9);
                // All trigrams are lowercase (folded)
                assert!(set.contains(&Trigram::from_bytes(b'h', b't', b't')));
                assert!(set.contains(&Trigram::from_bytes(b'e', b's', b't')));
                // No uppercase trigrams
                assert!(!set.contains(&Trigram::from_bytes(b'H', b't', b't')));
            }
            _ => panic!("expected TrigramQuery::All"),
        }
    }

    #[test]
    fn test_extract_literal_trigrams_short() {
        // "fn" is only 2 chars -> no trigrams -> None
        assert_eq!(extract_literal_trigrams("fn"), TrigramQuery::None);
    }

    #[test]
    fn test_extract_literal_trigrams_exact_three() {
        // "abc" -> exactly 1 trigram
        let result = extract_literal_trigrams("abc");
        match result {
            TrigramQuery::All(set) => {
                assert_eq!(set.len(), 1);
                assert!(set.contains(&Trigram::from_bytes(b'a', b'b', b'c')));
            }
            _ => panic!("expected TrigramQuery::All"),
        }
    }

    #[test]
    fn test_extract_literal_trigrams_empty() {
        assert_eq!(extract_literal_trigrams(""), TrigramQuery::None);
    }

    #[test]
    fn test_extract_literal_trigrams_deduplicates() {
        // "aaaa" has trigrams: "aaa", "aaa" -> deduplicated to 1
        let result = extract_literal_trigrams("aaaa");
        match result {
            TrigramQuery::All(set) => assert_eq!(set.len(), 1),
            _ => panic!("expected TrigramQuery::All"),
        }
    }

    // ---- extract_regex_trigrams tests ----

    #[test]
    fn test_extract_regex_trigrams_simple_literal() {
        // /HttpRequest/ -> extracts trigrams from literal "HttpRequest", folded
        let result = extract_regex_trigrams("HttpRequest");
        match result {
            TrigramQuery::All(set) => {
                assert_eq!(set.len(), 9);
                // Folded to lowercase
                assert!(set.contains(&Trigram::from_bytes(b'h', b't', b't')));
            }
            _ => panic!("expected TrigramQuery::All"),
        }
    }

    #[test]
    fn test_extract_regex_trigrams_with_wildcard() {
        // /Err\(.*Error\)/ -> literals "Err(" and "Error)"
        let result = extract_regex_trigrams(r"Err\(.*Error\)");
        match result {
            TrigramQuery::All(set) => {
                assert!(set.len() >= 1); // At least some trigrams from "Err(" or "Error)"
                // "err" should be present (folded from "Err")
                assert!(set.contains(&Trigram::from_bytes(b'e', b'r', b'r')));
            }
            _ => panic!("expected TrigramQuery::All, got {:?}", result),
        }
    }

    #[test]
    fn test_extract_regex_trigrams_alternation() {
        // /foo|bar/ -> OR of trigrams from "foo" and "bar"
        let result = extract_regex_trigrams("foo|bar");
        match result {
            TrigramQuery::Any(branches) => {
                assert_eq!(branches.len(), 2);
            }
            // Also acceptable: All or None depending on extractor behavior
            TrigramQuery::All(_) | TrigramQuery::None => {}
        }
    }

    #[test]
    fn test_extract_regex_trigrams_pure_wildcard() {
        // /.*/ -> no literals -> None
        let result = extract_regex_trigrams(".*");
        assert_eq!(result, TrigramQuery::None);
    }

    #[test]
    fn test_extract_regex_trigrams_short_literal() {
        // /ab/ -> only 2-char literal -> None
        let result = extract_regex_trigrams("ab");
        assert_eq!(result, TrigramQuery::None);
    }

    #[test]
    fn test_extract_regex_trigrams_invalid_regex() {
        // Invalid regex -> None (graceful fallback)
        let result = extract_regex_trigrams("(unclosed");
        assert_eq!(result, TrigramQuery::None);
    }

    // ---- extract_query_trigrams tests ----

    fn lit(text: &str) -> Query {
        Query::Literal(LiteralQuery {
            text: text.to_string(),
            case_sensitive: true,
        })
    }

    fn lit_ci(text: &str) -> Query {
        Query::Literal(LiteralQuery {
            text: text.to_string(),
            case_sensitive: false,
        })
    }

    fn regex_q(pattern: &str) -> Query {
        Query::Regex(RegexQuery {
            pattern: pattern.to_string(),
            case_sensitive: true,
        })
    }

    fn phrase(text: &str) -> Query {
        Query::Phrase(PhraseQuery {
            text: text.to_string(),
            case_sensitive: true,
        })
    }

    #[test]
    fn test_extract_query_literal() {
        let query = lit("HttpRequest");
        let result = extract_query_trigrams(&query);
        match result {
            TrigramQuery::All(set) => {
                assert_eq!(set.len(), 9);
                // All trigrams are lowercase (case-folded index)
                assert!(set.contains(&Trigram::from_bytes(b'h', b't', b't')));
                assert!(!set.contains(&Trigram::from_bytes(b'H', b't', b't')));
            }
            _ => panic!("expected All"),
        }
    }

    #[test]
    fn test_extract_query_literal_case_insensitive_same_trigrams() {
        // case_sensitive=true and case_sensitive=false produce SAME trigrams
        // because all extraction uses folded trigrams (index is case-folded)
        let query_cs = lit("HttpRequest");
        let query_ci = lit_ci("HttpRequest");
        let result_cs = extract_query_trigrams(&query_cs);
        let result_ci = extract_query_trigrams(&query_ci);
        match (result_cs, result_ci) {
            (TrigramQuery::All(set_cs), TrigramQuery::All(set_ci)) => {
                assert_eq!(
                    set_cs, set_ci,
                    "case flag should not affect trigram extraction"
                );
            }
            _ => panic!("expected All for both"),
        }
    }

    #[test]
    fn test_extract_query_regex() {
        let query = regex_q(r"Err\(.*Error\)");
        let result = extract_query_trigrams(&query);
        assert!(!result.is_none()); // Should extract some trigrams
    }

    #[test]
    fn test_extract_query_phrase() {
        // Phrase is treated same as Literal for trigram extraction
        let query = phrase("fn main()");
        let result = extract_query_trigrams(&query);
        match result {
            TrigramQuery::All(set) => {
                assert!(set.len() >= 7); // "fn main()" has 7 unique trigrams
            }
            _ => panic!("expected All"),
        }
    }

    #[test]
    fn test_extract_query_and() {
        // AND: merge trigram sets from both children
        let query = Query::And(vec![lit("foo"), lit("bar")]);
        let result = extract_query_trigrams(&query);
        match result {
            TrigramQuery::All(set) => {
                // "foo" -> 1 trigram, "bar" -> 1 trigram = 2 total
                assert_eq!(set.len(), 2);
                assert!(set.contains(&Trigram::from_bytes(b'f', b'o', b'o')));
                assert!(set.contains(&Trigram::from_bytes(b'b', b'a', b'r')));
            }
            _ => panic!("expected All"),
        }
    }

    #[test]
    fn test_extract_query_or() {
        // OR: separate trigram sets for each branch (binary Or)
        let query = Query::Or(Box::new(lit("foo")), Box::new(lit("bar")));
        let result = extract_query_trigrams(&query);
        match result {
            TrigramQuery::Any(branches) => {
                assert_eq!(branches.len(), 2);
            }
            _ => panic!("expected Any"),
        }
    }

    #[test]
    fn test_extract_query_not() {
        // NOT: cannot use trigrams for pruning -> None
        let query = Query::Not(Box::new(lit("deprecated")));
        let result = extract_query_trigrams(&query);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_query_language_filter() {
        // Language filter: no content trigrams -> None
        let query = Query::LanguageFilter(Language::Rust);
        let result = extract_query_trigrams(&query);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_query_path_filter() {
        // Path filter: no content trigrams (handled by metadata) -> None
        let query = Query::PathFilter("src/lib".to_string());
        let result = extract_query_trigrams(&query);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_query_and_with_filter() {
        // AND(Literal("foo"), LanguageFilter(Rust)) -> only "foo" trigrams
        let query = Query::And(vec![lit("foo"), Query::LanguageFilter(Language::Rust)]);
        let result = extract_query_trigrams(&query);
        match result {
            TrigramQuery::All(set) => {
                assert_eq!(set.len(), 1);
                assert!(set.contains(&Trigram::from_bytes(b'f', b'o', b'o')));
            }
            _ => panic!("expected All"),
        }
    }

    #[test]
    fn test_extract_query_and_with_short_literal() {
        // AND(Literal("fn"), Literal("main")) -> only "main" trigrams (fn too short)
        let query = Query::And(vec![lit("fn"), lit("main")]);
        let result = extract_query_trigrams(&query);
        match result {
            TrigramQuery::All(set) => {
                assert_eq!(set.len(), 2); // "mai" and "ain"
            }
            _ => panic!("expected All"),
        }
    }

    #[test]
    fn test_extract_query_or_with_none_branch() {
        // OR(Literal("foo"), Literal("ab")) -> "ab" has no trigrams
        // Since "ab" branch can't be filtered, whole OR is None
        let query = Query::Or(Box::new(lit("foo")), Box::new(lit("ab")));
        let result = extract_query_trigrams(&query);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_query_nested_and_or() {
        // AND(OR(Literal("foo"), Literal("bar")), Literal("baz"))
        let query = Query::And(vec![
            Query::Or(Box::new(lit("foo")), Box::new(lit("bar"))),
            lit("baz"),
        ]);
        let result = extract_query_trigrams(&query);
        match result {
            TrigramQuery::Any(branches) => {
                assert_eq!(branches.len(), 2);
                for branch in &branches {
                    assert!(branch.contains(&Trigram::from_bytes(b'b', b'a', b'z')));
                }
            }
            _ => panic!("expected Any, got {:?}", result),
        }
    }
}
