//! Query planner: converts a Query AST into an optimized execution plan.
//!
//! The planner inspects the parsed query to determine:
//! 1. Which pre-filters to apply (language, path)
//! 2. Which trigrams to intersect and in what order (smallest posting list first)
//! 3. What verification step to use (literal substring, regex, phrase)
//!
//! The plan is segment-specific because posting list sizes vary per segment.

use crate::types::{Language, Trigram};

/// A pre-filter step that cheaply narrows the candidate set before trigram intersection.
///
/// Pre-filters scan metadata (language, path) to produce a bitmap of candidate FileIds.
/// They run before the more expensive trigram posting list intersection.
#[derive(Debug, Clone, PartialEq)]
pub enum PreFilter {
    /// Filter to files matching the given language.
    Language(Language),
    /// Filter to files whose path matches the given glob pattern.
    PathGlob(String),
}

/// Describes how to verify that a candidate file actually matches the query.
///
/// After trigram intersection narrows candidates, each file's content is read
/// and checked against this verification step.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyStep {
    /// Exact substring match (case-sensitive).
    Literal(String),
    /// Regex pattern match.
    Regex(String),
}

/// A trigram with its estimated posting list size for intersection ordering.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredTrigram {
    /// The trigram to look up.
    pub trigram: Trigram,
    /// Estimated number of files containing this trigram (from the trigram table's
    /// `file_list_len` field). Used to order intersections smallest-first.
    pub estimated_count: u32,
}

/// The optimized execution plan for a query against a single segment.
///
/// Execution proceeds in order:
/// 1. Apply all `pre_filters` to build a candidate FileId bitmap
/// 2. Look up trigrams in `trigram_plan` order (smallest estimated count first),
///    intersecting with the candidate set at each step
/// 3. For each surviving candidate, run the `verify` step against file content
///
/// If `trigram_plan` is empty (query too short for trigrams), the plan is
/// considered a "no-op" and should return empty results (unless future full-scan
/// support is added for very short queries).
#[derive(Debug, Clone, PartialEq)]
pub struct QueryPlan {
    /// Pre-filter steps to apply before trigram intersection.
    /// Applied in order; each narrows the candidate set.
    pub pre_filters: Vec<PreFilter>,

    /// Trigrams to intersect, ordered by estimated posting list size (ascending).
    /// Smallest lists first minimizes intersection work.
    pub trigram_plan: Vec<ScoredTrigram>,

    /// How to verify candidate matches against actual file content.
    pub verify: VerifyStep,

    /// Whether this plan can produce results. False when the query is too short
    /// to extract trigrams and no full-scan fallback is available.
    pub is_empty: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Language, Trigram};

    #[test]
    fn test_query_plan_construction() {
        let plan = QueryPlan {
            pre_filters: vec![PreFilter::Language(Language::Rust)],
            trigram_plan: vec![
                ScoredTrigram {
                    trigram: Trigram::from_bytes(b'f', b'o', b'o'),
                    estimated_count: 10,
                },
                ScoredTrigram {
                    trigram: Trigram::from_bytes(b'o', b'o', b'b'),
                    estimated_count: 50,
                },
            ],
            verify: VerifyStep::Literal("foobar".to_string()),
            is_empty: false,
        };

        assert_eq!(plan.pre_filters.len(), 1);
        assert_eq!(plan.trigram_plan.len(), 2);
        // Verify smallest-first ordering
        assert!(plan.trigram_plan[0].estimated_count <= plan.trigram_plan[1].estimated_count);
        assert!(!plan.is_empty);
    }

    #[test]
    fn test_empty_plan_for_short_query() {
        let plan = QueryPlan {
            pre_filters: vec![],
            trigram_plan: vec![],
            verify: VerifyStep::Literal("fn".to_string()),
            is_empty: true,
        };
        assert!(plan.is_empty);
        assert!(plan.trigram_plan.is_empty());
    }

    #[test]
    fn test_plan_with_path_filter() {
        let plan = QueryPlan {
            pre_filters: vec![
                PreFilter::Language(Language::Rust),
                PreFilter::PathGlob("src/**/*.rs".to_string()),
            ],
            trigram_plan: vec![ScoredTrigram {
                trigram: Trigram::from_bytes(b'f', b'n', b' '),
                estimated_count: 100,
            }],
            verify: VerifyStep::Literal("fn ".to_string()),
            is_empty: false,
        };

        assert_eq!(plan.pre_filters.len(), 2);
        assert!(matches!(plan.pre_filters[0], PreFilter::Language(Language::Rust)));
        assert!(matches!(&plan.pre_filters[1], PreFilter::PathGlob(p) if p == "src/**/*.rs"));
    }

    #[test]
    fn test_plan_with_regex_verify() {
        let plan = QueryPlan {
            pre_filters: vec![],
            trigram_plan: vec![ScoredTrigram {
                trigram: Trigram::from_bytes(b'f', b'n', b' '),
                estimated_count: 5,
            }],
            verify: VerifyStep::Regex(r"fn\s+\w+".to_string()),
            is_empty: false,
        };

        assert!(matches!(plan.verify, VerifyStep::Regex(_)));
    }
}
