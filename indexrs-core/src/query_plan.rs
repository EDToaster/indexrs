//! Query planner: converts a Query AST into an optimized execution plan.
//!
//! The planner inspects the parsed query to determine:
//! 1. Which pre-filters to apply (language, path)
//! 2. Which trigrams to intersect and in what order (smallest posting list first)
//! 3. What verification step to use (literal substring, regex, phrase)
//!
//! The plan is segment-specific because posting list sizes vary per segment.

use std::fmt;

use crate::index_state::SegmentList;
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

impl QueryPlan {
    /// Returns true if the plan is guaranteed to produce zero results.
    ///
    /// This is the case when:
    /// - The plan is marked as empty (query too short)
    /// - Any trigram in the plan has an estimated count of 0 (no files contain
    ///   that trigram, so the intersection must be empty)
    pub fn can_short_circuit(&self) -> bool {
        self.is_empty || self.trigram_plan.iter().any(|t| t.estimated_count == 0)
    }
}

impl fmt::Display for PreFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PreFilter::Language(lang) => write!(f, "lang={lang}"),
            PreFilter::PathGlob(glob) => write!(f, "path={glob}"),
        }
    }
}

impl fmt::Display for VerifyStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyStep::Literal(s) => write!(f, "verify=literal({s:?})"),
            VerifyStep::Regex(s) => write!(f, "verify=regex({s:?})"),
        }
    }
}

impl fmt::Display for QueryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty {
            return write!(f, "QueryPlan(empty)");
        }

        write!(f, "QueryPlan(")?;

        if !self.pre_filters.is_empty() {
            let filters: Vec<String> = self.pre_filters.iter().map(|pf| pf.to_string()).collect();
            write!(f, "filters=[{}], ", filters.join(", "))?;
        }

        write!(f, "{} trigrams, {})", self.trigram_plan.len(), self.verify)
    }
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
pub fn plan_regex_query(pattern: &str, pre_filters: &[PreFilter], segment: &Segment) -> QueryPlan {
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

/// Input to the query planner, representing the parsed query's essential fields.
///
/// This is a bridge type used until the full Query AST from HHC-46 is available.
/// Once the parser is implemented, `plan_query()` will accept `&Query` directly
/// and extract the relevant fields.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryInput {
    /// A literal substring search.
    Literal {
        /// The exact substring to search for.
        pattern: String,
        /// Pre-filters to apply (language, path).
        filters: Vec<PreFilter>,
    },
    /// A regex pattern search.
    Regex {
        /// The regex pattern string.
        pattern: String,
        /// Pre-filters to apply (language, path).
        filters: Vec<PreFilter>,
    },
}

/// Build a query plan for a parsed query against a single segment.
///
/// Dispatches to `plan_literal_query()` or `plan_regex_query()` based on
/// the query input type.
pub fn plan_query(input: &QueryInput, segment: &Segment) -> QueryPlan {
    match input {
        QueryInput::Literal { pattern, filters } => plan_literal_query(pattern, filters, segment),
        QueryInput::Regex { pattern, filters } => plan_regex_query(pattern, filters, segment),
    }
}

/// Build query plans for all segments in a snapshot.
///
/// Returns a vector of `(SegmentId, QueryPlan)` pairs, one per segment.
/// Each plan is optimized for its specific segment's posting list sizes.
pub fn plan_query_multi(
    input: &QueryInput,
    segments: &SegmentList,
) -> Vec<(crate::types::SegmentId, QueryPlan)> {
    segments
        .iter()
        .map(|segment| (segment.segment_id(), plan_query(input, segment)))
        .collect()
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
        assert!(matches!(
            plan.pre_filters[0],
            PreFilter::Language(Language::Rust)
        ));
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

    // ---- Task 5: plan_query and plan_query_multi tests ----

    #[test]
    fn test_plan_query_literal() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        let input = QueryInput::Literal {
            pattern: "println".to_string(),
            filters: vec![],
        };

        let plan = plan_query(&input, &segment);

        assert!(!plan.is_empty);
        assert!(matches!(plan.verify, VerifyStep::Literal(ref s) if s == "println"));
    }

    #[test]
    fn test_plan_query_regex() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        let input = QueryInput::Regex {
            pattern: r"println!\(".to_string(),
            filters: vec![PreFilter::Language(Language::Rust)],
        };

        let plan = plan_query(&input, &segment);

        assert!(!plan.is_empty);
        assert!(matches!(plan.verify, VerifyStep::Regex(_)));
        assert_eq!(plan.pre_filters.len(), 1);
    }

    #[test]
    fn test_plan_query_multi_segment() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = {
            let writer = SegmentWriter::new(&base_dir, SegmentId(0));
            Arc::new(
                writer
                    .build(vec![InputFile {
                        path: "a.rs".to_string(),
                        content: b"fn main() { println!(\"hello\"); }".to_vec(),
                        mtime: 0,
                    }])
                    .unwrap(),
            )
        };

        let seg1 = {
            let writer = SegmentWriter::new(&base_dir, SegmentId(1));
            Arc::new(
                writer
                    .build(vec![InputFile {
                        path: "b.rs".to_string(),
                        content: b"fn other() { println!(\"world\"); }".to_vec(),
                        mtime: 0,
                    }])
                    .unwrap(),
            )
        };

        let segments: crate::index_state::SegmentList = Arc::new(vec![seg0, seg1]);
        let input = QueryInput::Literal {
            pattern: "println".to_string(),
            filters: vec![],
        };

        let plans = plan_query_multi(&input, &segments);

        assert_eq!(plans.len(), 2);
        // Each plan should have trigrams
        for (_seg_id, plan) in &plans {
            assert!(!plan.is_empty);
            assert!(!plan.trigram_plan.is_empty());
        }
    }

    // ---- Task 6: early termination and deduplication tests ----

    #[test]
    fn test_plan_early_termination_zero_count_trigram() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        let plan = plan_literal_query("xyzxyz", &[], &segment);

        // When a trigram has estimated_count == 0, it should be first in the plan
        // (since 0 is the smallest), enabling early termination during execution
        assert!(!plan.is_empty);
        assert_eq!(plan.trigram_plan[0].estimated_count, 0);
    }

    #[test]
    fn test_plan_handles_duplicate_trigrams() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        // "aaaa" produces trigram "aaa" twice -- plan should deduplicate
        let plan = plan_literal_query("aaaa", &[], &segment);

        // extract_unique_trigrams already deduplicates, but verify plan has 1 trigram
        assert_eq!(plan.trigram_plan.len(), 1);
    }

    #[test]
    fn test_plan_can_short_circuit() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        let plan = plan_literal_query("xyzxyz", &[], &segment);

        // A plan where any trigram has count 0 can be detected as guaranteed-empty
        assert!(plan.can_short_circuit());
    }

    #[test]
    fn test_plan_cannot_short_circuit() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        let plan = plan_literal_query("println", &[], &segment);

        // All trigrams exist in the index, so no short-circuit
        assert!(!plan.can_short_circuit());
    }

    // ---- Task 7: Display implementations tests ----

    #[test]
    fn test_query_plan_display() {
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

        let display = format!("{plan}");
        assert!(display.contains("lang=Rust"));
        assert!(display.contains("2 trigrams"));
        assert!(display.contains("verify=literal"));
    }

    #[test]
    fn test_empty_plan_display() {
        let plan = QueryPlan {
            pre_filters: vec![],
            trigram_plan: vec![],
            verify: VerifyStep::Literal("fn".to_string()),
            is_empty: true,
        };

        let display = format!("{plan}");
        assert!(display.contains("empty"));
    }

    // ---- Task 8: Integration tests ----

    #[test]
    fn test_plan_trigram_ordering_matches_actual_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Build a segment where different trigrams have very different posting list sizes
        let mut files = Vec::new();
        // 50 files containing "println" (common)
        for i in 0..50 {
            files.push(InputFile {
                path: format!("common_{i:03}.rs"),
                content: format!("fn f{i}() {{ println!(\"hello {i}\"); }}").into_bytes(),
                mtime: 0,
            });
        }
        // 2 files containing "xyzzy" (rare)
        for i in 0..2 {
            files.push(InputFile {
                path: format!("rare_{i}.rs"),
                content: format!("fn xyzzy_{i}() {{ println!(\"xyzzy\"); }}").into_bytes(),
                mtime: 0,
            });
        }

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = Arc::new(writer.build(files).unwrap());

        // Query "xyzzy" -- trigrams from "xyzzy" should have small estimated counts
        let plan = plan_literal_query("xyzzy", &[], &segment);

        assert!(!plan.is_empty);
        assert!(!plan.trigram_plan.is_empty());

        // The "xyzzy" trigrams should all have estimated count <= 2
        for scored in &plan.trigram_plan {
            assert!(
                scored.estimated_count <= 2,
                "trigram {} has unexpected count {}: expected <= 2",
                scored.trigram,
                scored.estimated_count
            );
        }

        // Now plan "println" -- trigrams should have larger estimated counts
        let plan_common = plan_literal_query("println", &[], &segment);

        assert!(!plan_common.is_empty);
        // "println" trigrams should have estimated count >= 50
        // (they appear in all 52 files)
        for scored in &plan_common.trigram_plan {
            assert!(
                scored.estimated_count >= 50,
                "trigram {} has unexpected count {}: expected >= 50",
                scored.trigram,
                scored.estimated_count
            );
        }
    }

    #[test]
    fn test_plan_and_execute_literal_search() {
        use crate::index_state::SegmentList;
        use crate::multi_search::search_segments;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        // Plan the query
        let plan = plan_literal_query("println", &[], &segment);
        assert!(!plan.is_empty);
        assert!(!plan.can_short_circuit());

        // Execute the search through the existing pipeline
        let snapshot: SegmentList = Arc::new(vec![segment]);
        let result = search_segments(&snapshot, "println").unwrap();

        // The search should find matches
        assert!(!result.files.is_empty());

        // The plan's trigram count should reflect the query
        // "println" has 5 trigrams: "pri", "rin", "int", "ntl", "tln"
        assert_eq!(plan.trigram_plan.len(), 5);
    }

    #[test]
    fn test_plan_short_circuit_matches_empty_search() {
        use crate::index_state::SegmentList;
        use crate::multi_search::search_segments;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        let segment = build_test_segment(&base_dir);

        // Plan for a query that doesn't exist
        let plan = plan_literal_query("zzzzzzz", &[], &segment);
        assert!(plan.can_short_circuit());

        // Execute the search -- should also be empty
        let snapshot: SegmentList = Arc::new(vec![segment]);
        let result = search_segments(&snapshot, "zzzzzzz").unwrap();
        assert!(result.files.is_empty());
    }
}
