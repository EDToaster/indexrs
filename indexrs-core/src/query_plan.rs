//! Query planner: converts a Query AST into an optimized execution plan.
//!
//! The planner inspects the parsed query to determine:
//! 1. Which pre-filters to apply (language, path)
//! 2. Which trigrams to intersect and in what order (smallest posting list first)
//! 3. What verification step to use (literal substring, regex, phrase)
//!
//! The plan is segment-specific because posting list sizes vary per segment.

use crate::segment::Segment;
use crate::trigram::extract_unique_trigrams;
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

/// Build a query plan for a literal substring query against a single segment.
///
/// This is the core planning function. It:
/// 1. Accepts pre-filters from the parsed query (language, path)
/// 2. Extracts trigrams from the literal query string
/// 3. Looks up each trigram's estimated posting list size via
///    `TrigramIndexReader::estimate_posting_list_size()` (O(log n), no decoding)
/// 4. Sorts trigrams by estimated count (smallest first) for efficient intersection
/// 5. Returns a `QueryPlan` with the verification step set to literal matching
///
/// If the query is shorter than 3 characters (no trigrams extractable), the plan's
/// `is_empty` flag is set to true and `trigram_plan` is empty.
pub fn plan_literal_query(query: &str, pre_filters: &[PreFilter], segment: &Segment) -> QueryPlan {
    if query.len() < 3 {
        return QueryPlan {
            pre_filters: pre_filters.to_vec(),
            trigram_plan: vec![],
            verify: VerifyStep::Literal(query.to_string()),
            is_empty: true,
        };
    }

    let trigrams = extract_unique_trigrams(query.as_bytes());
    let reader = segment.trigram_reader();

    let mut scored: Vec<ScoredTrigram> = trigrams
        .into_iter()
        .map(|trigram| {
            let estimated_count = reader.estimate_posting_list_size(trigram);
            ScoredTrigram {
                trigram,
                estimated_count,
            }
        })
        .collect();

    // Sort by estimated count ascending (smallest posting list first)
    scored.sort_by_key(|s| s.estimated_count);

    QueryPlan {
        pre_filters: pre_filters.to_vec(),
        trigram_plan: scored,
        verify: VerifyStep::Literal(query.to_string()),
        is_empty: false,
    }
}

/// Build a query plan for a regex pattern query against a single segment.
///
/// For regex queries, the planner extracts required literal substrings from
/// the regex pattern (substrings that MUST appear for the regex to match),
/// generates trigrams from those substrings, and uses them for candidate
/// filtering. The verification step uses the full regex.
///
/// If no required literal substrings >= 3 characters can be extracted from
/// the regex, the plan is marked as empty (no trigram-based filtering possible).
///
/// # Required literal extraction
///
/// This function uses a simple heuristic: it scans the regex for runs of
/// literal characters (non-metacharacters) and extracts trigrams from runs
/// of length >= 3. This covers common patterns like `println!\(` where
/// "println" and "(" are literal. For full regex trigram extraction, this
/// will be enhanced by HHC-47's `extract_query_trigrams()` function.
pub fn plan_regex_query(
    pattern: &str,
    pre_filters: &[PreFilter],
    segment: &Segment,
) -> QueryPlan {
    let literal_runs = extract_literal_runs(pattern);
    let reader = segment.trigram_reader();

    let mut scored: Vec<ScoredTrigram> = Vec::new();
    for run in &literal_runs {
        if run.len() >= 3 {
            let trigrams = extract_unique_trigrams(run.as_bytes());
            for trigram in trigrams {
                let estimated_count = reader.estimate_posting_list_size(trigram);
                scored.push(ScoredTrigram {
                    trigram,
                    estimated_count,
                });
            }
        }
    }

    // Deduplicate trigrams (same trigram may appear in multiple literal runs)
    scored.sort_by_key(|s| s.trigram.to_u32());
    scored.dedup_by_key(|s| s.trigram);

    // Re-sort by estimated count ascending
    scored.sort_by_key(|s| s.estimated_count);

    let is_empty = scored.is_empty();

    QueryPlan {
        pre_filters: pre_filters.to_vec(),
        trigram_plan: scored,
        verify: VerifyStep::Regex(pattern.to_string()),
        is_empty,
    }
}

/// Extract runs of literal (non-metacharacter) characters from a regex pattern.
///
/// Metacharacters: `.`, `*`, `+`, `?`, `[`, `]`, `(`, `)`, `{`, `}`, `|`, `^`, `$`, `\`
/// Escaped characters (e.g., `\(`) are treated as literal.
/// Character classes (`[...]`) are skipped entirely -- their contents are not literal.
///
/// Returns a vector of literal substrings found in the pattern.
fn extract_literal_runs(pattern: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current_run = String::new();
    let bytes = pattern.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        // Skip character classes entirely
        if b == b'[' {
            if !current_run.is_empty() {
                runs.push(std::mem::take(&mut current_run));
            }
            i += 1;
            // Skip until closing ']', handling escaped brackets
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2; // skip escaped char inside class
                } else if bytes[i] == b']' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }

        if b == b'\\' && i + 1 < bytes.len() {
            // Escaped character -- treat next char as literal
            // But only if it's a metacharacter being escaped
            let next = bytes[i + 1];
            if is_regex_meta(next) {
                current_run.push(next as char);
                i += 2;
                continue;
            } else {
                // Escape sequence like \s, \w, \d -- not a literal
                if !current_run.is_empty() {
                    runs.push(std::mem::take(&mut current_run));
                }
                i += 2;
                continue;
            }
        }

        if is_regex_meta(b) {
            if !current_run.is_empty() {
                runs.push(std::mem::take(&mut current_run));
            }
            i += 1;
        } else {
            current_run.push(b as char);
            i += 1;
        }
    }

    if !current_run.is_empty() {
        runs.push(current_run);
    }

    runs
}

/// Check if a byte is a regex metacharacter.
fn is_regex_meta(b: u8) -> bool {
    matches!(
        b,
        b'.' | b'*'
            | b'+'
            | b'?'
            | b'['
            | b']'
            | b'('
            | b')'
            | b'{'
            | b'}'
            | b'|'
            | b'^'
            | b'$'
            | b'\\'
    )
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

    // ---- Task 3: plan_literal_query tests ----

    use crate::segment::{InputFile, SegmentWriter};
    use crate::types::SegmentId;
    use std::sync::Arc;

    /// Helper: build a test segment with multiple files of different languages.
    fn build_test_segment(base_dir: &std::path::Path) -> Arc<crate::segment::Segment> {
        std::fs::create_dir_all(base_dir).unwrap();
        let writer = SegmentWriter::new(base_dir, SegmentId(0));
        Arc::new(
            writer
                .build(vec![
                    InputFile {
                        path: "src/main.rs".to_string(),
                        content: b"fn main() { println!(\"hello\"); }".to_vec(),
                        mtime: 0,
                    },
                    InputFile {
                        path: "src/lib.rs".to_string(),
                        content: b"pub fn add(a: i32, b: i32) -> i32 { a + b }".to_vec(),
                        mtime: 0,
                    },
                    InputFile {
                        path: "app.py".to_string(),
                        content: b"def main():\n    print(\"hello\")\n".to_vec(),
                        mtime: 0,
                    },
                ])
                .unwrap(),
        )
    }

    #[test]
    fn test_plan_literal_query() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        let plan = plan_literal_query("println", &[], &segment);

        assert!(!plan.is_empty);
        assert!(plan.pre_filters.is_empty());
        assert!(!plan.trigram_plan.is_empty());
        assert!(matches!(plan.verify, VerifyStep::Literal(ref s) if s == "println"));

        // Verify trigrams are sorted by estimated count (ascending)
        for window in plan.trigram_plan.windows(2) {
            assert!(
                window[0].estimated_count <= window[1].estimated_count,
                "trigram plan not sorted: {} > {}",
                window[0].estimated_count,
                window[1].estimated_count
            );
        }
    }

    #[test]
    fn test_plan_literal_query_with_language_filter() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        let plan = plan_literal_query("main", &[PreFilter::Language(Language::Rust)], &segment);

        assert!(!plan.is_empty);
        assert_eq!(plan.pre_filters.len(), 1);
        assert!(matches!(
            plan.pre_filters[0],
            PreFilter::Language(Language::Rust)
        ));
        assert!(!plan.trigram_plan.is_empty());
    }

    #[test]
    fn test_plan_literal_query_short_query() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        // "fn" is only 2 chars -- no trigrams possible
        let plan = plan_literal_query("fn", &[], &segment);

        assert!(plan.is_empty);
        assert!(plan.trigram_plan.is_empty());
    }

    #[test]
    fn test_plan_literal_query_absent_trigram() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        // "xyzxyz" contains trigrams not in the index
        let plan = plan_literal_query("xyzxyz", &[], &segment);

        // Plan should still be valid but at least one trigram has count 0,
        // which means the plan could short-circuit during execution
        assert!(!plan.is_empty);
        assert!(plan.trigram_plan.iter().any(|t| t.estimated_count == 0));
    }

    #[test]
    fn test_plan_literal_query_with_path_filter() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        let plan = plan_literal_query(
            "main",
            &[PreFilter::PathGlob("src/**/*.rs".to_string())],
            &segment,
        );

        assert!(!plan.is_empty);
        assert_eq!(plan.pre_filters.len(), 1);
    }

    // ---- Task 4: plan_regex_query tests ----

    #[test]
    fn test_plan_regex_query() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        // Regex r"println!\(" has "println" as a literal run (>= 3 chars)
        // and "(" as an escaped literal (< 3 chars, ignored for trigrams)
        let plan = plan_regex_query(r"println!\(", &[], &segment);

        assert!(!plan.is_empty);
        assert!(matches!(plan.verify, VerifyStep::Regex(ref s) if s == r"println!\("));
        // Should have trigrams from the literal parts of the regex
        assert!(!plan.trigram_plan.is_empty());
    }

    #[test]
    fn test_plan_regex_query_no_extractable_trigrams() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        // Regex r"[a-z]+" has no required literal substrings >= 3 chars
        let plan = plan_regex_query(r"[a-z]+", &[], &segment);

        // Plan is empty because no trigrams can be extracted
        assert!(plan.is_empty);
        assert!(plan.trigram_plan.is_empty());
    }

    #[test]
    fn test_plan_regex_query_with_filters() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        let plan = plan_regex_query(
            r"fn\s+main",
            &[PreFilter::Language(Language::Rust)],
            &segment,
        );

        assert_eq!(plan.pre_filters.len(), 1);
        assert!(matches!(plan.verify, VerifyStep::Regex(_)));
    }
}
