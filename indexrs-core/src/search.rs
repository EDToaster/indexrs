//! Search result types for indexrs query results.
//!
//! These types represent the output of the query engine: matched files,
//! matched lines within those files, and aggregate search result metadata.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::{FileId, Language};

/// The pattern type used for content verification.
///
/// Produced by the query parser and consumed by the content verifier.
/// Determines which matching strategy is used during candidate verification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MatchPattern {
    /// Exact byte-level substring match.
    Literal(String),
    /// Regex pattern (compiled with the `regex` crate).
    Regex(String),
    /// Case-insensitive literal match (lowercased comparison).
    LiteralCaseInsensitive(String),
}

/// A single matching line within a file.
///
/// Contains the line content and byte-offset ranges indicating which portions
/// of the line matched the query, used for rendering match highlights.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineMatch {
    /// 1-based line number within the file.
    pub line_number: u32,
    /// The full text content of the matching line.
    pub content: String,
    /// Byte-offset ranges `(start, end)` within `content` that matched the query.
    /// Used for highlighting matched portions in the output.
    pub ranges: Vec<(usize, usize)>,
    /// Context lines before this match (nearest lines first, i.e., index 0 is furthest).
    /// Empty when context_lines=0.
    pub context_before: Vec<ContextLine>,
    /// Context lines after this match. Empty when context_lines=0.
    pub context_after: Vec<ContextLine>,
}

/// A file that matched a search query, with its matching lines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMatch {
    /// The indexed file ID.
    pub file_id: FileId,
    /// Path to the file relative to the repository root.
    pub path: PathBuf,
    /// Detected programming language of the file.
    pub language: Language,
    /// Lines within the file that matched the query.
    pub lines: Vec<LineMatch>,
    /// Relevance score in the range [0.0, 1.0], higher is more relevant.
    pub score: f64,
}

/// A non-matching line shown as context around a match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextLine {
    /// 1-based line number within the file.
    pub line_number: u32,
    /// The full text content of the line.
    pub content: String,
}

/// A group of adjacent matches with their surrounding context lines.
///
/// When multiple matches are close together (within `2 * context_lines`),
/// they are merged into a single `ContextBlock` to avoid duplicate context
/// lines and provide a contiguous reading experience.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextBlock {
    /// Context lines before the first match in this block.
    pub before: Vec<ContextLine>,
    /// The matching lines within this block.
    pub matches: Vec<LineMatch>,
    /// Context lines after the last match in this block.
    pub after: Vec<ContextLine>,
}

/// Options that control search behavior.
///
/// Passed to [`search_segments_with_options()`](crate::multi_search::search_segments_with_options)
/// to configure context lines and other search parameters.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// Number of context lines to include before and after each match.
    /// Default: 0 (no context).
    pub context_lines: usize,
}

/// Aggregate result of a search query.
///
/// Contains the matched files, total match/file counts, and query duration.
/// Implements `Display` for plain-text summary output.
///
/// Files are ordered by relevance score (descending). Use [`paginate()`](Self::paginate)
/// for offset-based pagination over the file list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Total number of matching lines across all files (before pagination).
    pub total_match_count: usize,
    /// Total number of files with matches (before pagination).
    pub total_file_count: usize,
    /// Files that matched the query, ordered by relevance score (descending).
    pub files: Vec<FileMatch>,
    /// Wall-clock time taken to execute the query.
    pub duration: Duration,
}

impl SearchResult {
    /// Return a paginated view of this result set.
    ///
    /// `offset` is the number of files to skip (0-based).
    /// `limit` is the maximum number of files to return.
    ///
    /// The returned `SearchResult` preserves the original `total_match_count`,
    /// `total_file_count`, and `duration` so consumers know the full result
    /// size for pagination UI.
    pub fn paginate(&self, offset: usize, limit: usize) -> SearchResult {
        let files: Vec<FileMatch> = self
            .files
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();
        SearchResult {
            total_match_count: self.total_match_count,
            total_file_count: self.total_file_count,
            files,
            duration: self.duration,
        }
    }
}

impl fmt::Display for FileMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // File header: ## path/to/file  (Language, N matches)
        writeln!(
            f,
            "## {} ({}, {} matches)",
            self.path.display(),
            self.language,
            self.lines.len()
        )?;

        for line_match in &self.lines {
            // Context before
            for ctx in &line_match.context_before {
                writeln!(f, "L{}:  {}", ctx.line_number, ctx.content)?;
            }
            // Match line (marked with *)
            writeln!(f, "L{}:* {}", line_match.line_number, line_match.content)?;
            // Context after
            for ctx in &line_match.context_after {
                writeln!(f, "L{}:  {}", ctx.line_number, ctx.content)?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for SearchResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} results in {} files ({:.1?})",
            self.total_match_count, self.total_file_count, self.duration,
        )?;
        for file_match in &self.files {
            writeln!(f)?;
            write!(f, "{file_match}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_result_display() {
        let result = SearchResult {
            total_match_count: 42,
            total_file_count: 2,
            files: vec![
                FileMatch {
                    file_id: FileId(1),
                    path: PathBuf::from("src/main.rs"),
                    language: Language::Rust,
                    lines: vec![],
                    score: 0.95,
                },
                FileMatch {
                    file_id: FileId(2),
                    path: PathBuf::from("src/lib.rs"),
                    language: Language::Rust,
                    lines: vec![],
                    score: 0.85,
                },
            ],
            duration: Duration::from_millis(5),
        };
        let display = result.to_string();
        assert!(display.contains("42 results"));
        assert!(display.contains("2 files"));
        assert!(display.contains("5"));
    }

    #[test]
    fn test_search_result_display_empty() {
        let result = SearchResult {
            total_match_count: 0,
            total_file_count: 0,
            files: vec![],
            duration: Duration::from_micros(100),
        };
        let display = result.to_string();
        assert!(display.contains("0 results"));
        assert!(display.contains("0 files"));
    }

    #[test]
    fn test_file_match_display_with_context() {
        let file_match = FileMatch {
            file_id: FileId(1),
            path: PathBuf::from("src/main.rs"),
            language: Language::Rust,
            lines: vec![LineMatch {
                line_number: 5,
                content: "fn main() {".to_string(),
                ranges: vec![(0, 7)],
                context_before: vec![ContextLine {
                    line_number: 4,
                    content: "".to_string(),
                }],
                context_after: vec![ContextLine {
                    line_number: 6,
                    content: "    println!(\"hello\");".to_string(),
                }],
            }],
            score: 0.9,
        };
        let display = file_match.to_string();
        assert!(display.contains("src/main.rs"));
        assert!(display.contains("Rust"));
        assert!(display.contains("L5:"));
        assert!(display.contains("fn main() {"));
    }

    #[test]
    fn test_search_result_display_with_pagination() {
        let result = SearchResult {
            total_match_count: 42,
            total_file_count: 10,
            files: vec![FileMatch {
                file_id: FileId(1),
                path: PathBuf::from("src/main.rs"),
                language: Language::Rust,
                lines: vec![],
                score: 0.9,
            }],
            duration: Duration::from_millis(5),
        };
        let display = result.to_string();
        assert!(display.contains("42 results"));
        assert!(display.contains("10 files"));
    }

    #[test]
    fn test_line_match_ranges() {
        let line = LineMatch {
            line_number: 10,
            content: "fn parse_query(input: &str) -> Query".to_string(),
            ranges: vec![(3, 14), (31, 36)],
            context_before: vec![],
            context_after: vec![],
        };
        assert_eq!(line.line_number, 10);
        assert_eq!(line.ranges.len(), 2);
        assert_eq!(
            &line.content[line.ranges[0].0..line.ranges[0].1],
            "parse_query"
        );
        assert_eq!(&line.content[line.ranges[1].0..line.ranges[1].1], "Query");
    }

    #[test]
    fn test_line_match_empty_ranges() {
        let line = LineMatch {
            line_number: 1,
            content: "use std::io;".to_string(),
            ranges: vec![],
            context_before: vec![],
            context_after: vec![],
        };
        assert!(line.ranges.is_empty());
    }

    #[test]
    fn test_file_match_construction() {
        let file_match = FileMatch {
            file_id: FileId(42),
            path: PathBuf::from("src/types.rs"),
            language: Language::Rust,
            lines: vec![LineMatch {
                line_number: 5,
                content: "pub struct FileId(u32);".to_string(),
                ranges: vec![(11, 17)],
                context_before: vec![],
                context_after: vec![],
            }],
            score: 0.92,
        };
        assert_eq!(file_match.file_id, FileId(42));
        assert_eq!(file_match.language, Language::Rust);
        assert_eq!(file_match.lines.len(), 1);
        assert!((file_match.score - 0.92).abs() < f64::EPSILON);
    }

    #[test]
    fn test_match_pattern_literal() {
        let pat = MatchPattern::Literal("println".to_string());
        assert!(matches!(pat, MatchPattern::Literal(_)));
    }

    #[test]
    fn test_match_pattern_regex() {
        let pat = MatchPattern::Regex("fn\\s+\\w+".to_string());
        assert!(matches!(pat, MatchPattern::Regex(_)));
    }

    #[test]
    fn test_match_pattern_case_insensitive() {
        let pat = MatchPattern::LiteralCaseInsensitive("Println".to_string());
        assert!(matches!(pat, MatchPattern::LiteralCaseInsensitive(_)));
    }

    #[test]
    fn test_context_line_construction() {
        let cl = ContextLine {
            line_number: 5,
            content: "use std::io;".to_string(),
        };
        assert_eq!(cl.line_number, 5);
        assert_eq!(cl.content, "use std::io;");
    }

    #[test]
    fn test_context_block_construction() {
        let block = ContextBlock {
            before: vec![ContextLine {
                line_number: 1,
                content: "// before".to_string(),
            }],
            matches: vec![LineMatch {
                line_number: 2,
                content: "fn main() {}".to_string(),
                ranges: vec![(0, 2)],
                context_before: vec![],
                context_after: vec![],
            }],
            after: vec![ContextLine {
                line_number: 3,
                content: "// after".to_string(),
            }],
        };
        assert_eq!(block.before.len(), 1);
        assert_eq!(block.matches.len(), 1);
        assert_eq!(block.after.len(), 1);
    }

    #[test]
    fn test_line_match_with_context() {
        let line = LineMatch {
            line_number: 10,
            content: "fn parse_query(input: &str) -> Query".to_string(),
            ranges: vec![(3, 14)],
            context_before: vec![
                ContextLine {
                    line_number: 8,
                    content: "".to_string(),
                },
                ContextLine {
                    line_number: 9,
                    content: "/// Parse a query string.".to_string(),
                },
            ],
            context_after: vec![ContextLine {
                line_number: 11,
                content: "    let tokens = tokenize(input);".to_string(),
            }],
        };
        assert_eq!(line.context_before.len(), 2);
        assert_eq!(line.context_after.len(), 1);
        assert_eq!(line.context_before[1].line_number, 9);
        assert_eq!(line.context_after[0].line_number, 11);
    }

    #[test]
    fn test_line_match_default_empty_context() {
        let line = LineMatch {
            line_number: 1,
            content: "use std::io;".to_string(),
            ranges: vec![],
            context_before: vec![],
            context_after: vec![],
        };
        assert!(line.context_before.is_empty());
        assert!(line.context_after.is_empty());
    }

    #[test]
    fn test_search_result_total_file_count() {
        let result = SearchResult {
            total_match_count: 42,
            total_file_count: 10,
            files: vec![],
            duration: Duration::from_millis(5),
        };
        assert_eq!(result.total_file_count, 10);
    }

    #[test]
    fn test_search_result_paginate_basic() {
        let files: Vec<FileMatch> = (0..10)
            .map(|i| FileMatch {
                file_id: FileId(i),
                path: PathBuf::from(format!("file_{i}.rs")),
                language: Language::Rust,
                lines: vec![],
                score: 1.0 - (i as f64 / 10.0),
            })
            .collect();

        let result = SearchResult {
            total_match_count: 50,
            total_file_count: 10,
            files,
            duration: Duration::from_millis(5),
        };

        let page = result.paginate(0, 3);
        assert_eq!(page.files.len(), 3);
        assert_eq!(page.total_file_count, 10);
        assert_eq!(page.total_match_count, 50);
        assert_eq!(page.files[0].path, PathBuf::from("file_0.rs"));
        assert_eq!(page.files[2].path, PathBuf::from("file_2.rs"));
    }

    #[test]
    fn test_search_result_paginate_offset() {
        let files: Vec<FileMatch> = (0..10)
            .map(|i| FileMatch {
                file_id: FileId(i),
                path: PathBuf::from(format!("file_{i}.rs")),
                language: Language::Rust,
                lines: vec![],
                score: 0.5,
            })
            .collect();

        let result = SearchResult {
            total_match_count: 50,
            total_file_count: 10,
            files,
            duration: Duration::from_millis(5),
        };

        let page = result.paginate(3, 4);
        assert_eq!(page.files.len(), 4);
        assert_eq!(page.files[0].path, PathBuf::from("file_3.rs"));
        assert_eq!(page.files[3].path, PathBuf::from("file_6.rs"));
    }

    #[test]
    fn test_search_result_paginate_past_end() {
        let files: Vec<FileMatch> = (0..3)
            .map(|i| FileMatch {
                file_id: FileId(i),
                path: PathBuf::from(format!("file_{i}.rs")),
                language: Language::Rust,
                lines: vec![],
                score: 0.5,
            })
            .collect();

        let result = SearchResult {
            total_match_count: 10,
            total_file_count: 3,
            files,
            duration: Duration::from_millis(5),
        };

        // Offset beyond available files
        let page = result.paginate(5, 10);
        assert_eq!(page.files.len(), 0);
        assert_eq!(page.total_file_count, 3);
    }

    #[test]
    fn test_search_result_paginate_partial_last_page() {
        let files: Vec<FileMatch> = (0..7)
            .map(|i| FileMatch {
                file_id: FileId(i),
                path: PathBuf::from(format!("file_{i}.rs")),
                language: Language::Rust,
                lines: vec![],
                score: 0.5,
            })
            .collect();

        let result = SearchResult {
            total_match_count: 35,
            total_file_count: 7,
            files,
            duration: Duration::from_millis(5),
        };

        // Last page has only 2 items instead of 5
        let page = result.paginate(5, 5);
        assert_eq!(page.files.len(), 2);
        assert_eq!(page.files[0].path, PathBuf::from("file_5.rs"));
        assert_eq!(page.files[1].path, PathBuf::from("file_6.rs"));
    }
}
