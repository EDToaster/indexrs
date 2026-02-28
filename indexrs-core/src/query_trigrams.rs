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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Trigram;

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
}
