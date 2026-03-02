//! Plain-text response formatter for MCP.
//!
//! Formats search results as plain text optimized for LLM consumption
//! (~40% fewer tokens than JSON). This is a separate formatter from the
//! CLI output -- same underlying [`SearchResult`], different rendering.
//!
//! Design principles (from `docs/design/mcp-interface.md`):
//! - **Text-first**: no JSON blobs, LLMs parse plain text more naturally
//! - **File-grouped**: results grouped by file with `## path` headers
//! - **Match highlighting**: `*` in the gutter for matching lines
//! - **Line numbers always**: enables LLM to reference specific locations
//! - **Summary first**: one-line summary so LLM can assess pagination needs
//!
//! # Example output
//!
//! ```text
//! Found 47 matches across 12 files (showing 1-12)
//!
//! ## src/index/builder.rs
//! L42:   fn build_trigram_index(...)
//! L43:       let mut index = TrigramIndex::new();
//! L44:*      for trigram in content.trigrams() {
//! L45:       index.insert(trigram, self.current_doc_id);
//! ```

use std::fmt::Write;

use indexrs_core::search::{FileMatch, LineMatch, SearchResult};
use indexrs_core::types::Language;

/// Options for controlling the format output.
#[derive(Debug, Clone)]
pub struct FormatOptions {
    /// Offset of the first file being shown (0-based). Used in the summary line.
    pub offset: usize,
    /// Maximum number of files shown in this page. Used in the summary line.
    pub page_size: usize,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            offset: 0,
            page_size: 20,
        }
    }
}

/// Metadata about a file for the file list display.
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// Relative path from repository root.
    pub path: String,
    /// Detected programming language.
    pub language: Language,
    /// File size in bytes.
    pub size_bytes: u64,
}

/// Metadata for formatting a single file's content.
#[derive(Debug, Clone)]
pub struct FileFormatMetadata {
    /// Total number of lines in the file.
    pub total_lines: usize,
    /// Detected programming language.
    pub language: Language,
    /// How long ago the file was indexed (human-readable, e.g. "2m ago").
    pub indexed_ago: Option<String>,
}

/// Information about the index status for display.
#[derive(Debug, Clone)]
pub struct IndexStatusInfo {
    /// Total number of indexed files.
    pub total_files: usize,
    /// Number of active segments.
    pub segment_count: usize,
    /// Total index size on disk in bytes.
    pub disk_size_bytes: Option<u64>,
}

/// Format search results as plain text for MCP tool responses.
///
/// The output follows the design doc format:
/// - Summary line: "Found N matches across M files (showing X-Y)"
/// - File sections with `## path` headers
/// - `L{n}:*` gutter for matches, `L{n}: ` for context lines
/// - Large result hint when total_file_count > 100
pub fn format_search_results(results: &SearchResult, options: &FormatOptions) -> String {
    let mut out = String::new();

    // Summary line
    let showing_start = options.offset + 1;
    let showing_end = options.offset + results.files.len();
    if options.offset == 0 && results.files.len() == results.total_file_count {
        // All results fit on one page
        writeln!(
            out,
            "Found {} matches across {} files",
            results.total_match_count, results.total_file_count
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "Found {} matches across {} files (showing {}-{})",
            results.total_match_count, results.total_file_count, showing_start, showing_end
        )
        .unwrap();
    }

    // Large result hint
    if results.total_file_count > 100 {
        writeln!(
            out,
            "Tip: Consider narrowing with path:, language:, or a more specific query."
        )
        .unwrap();
    }

    // File sections
    for file_match in &results.files {
        writeln!(out).unwrap();
        format_file_match(&mut out, file_match);
    }

    out
}

/// Format a staleness warning to prepend to results.
///
/// Returns `Some(warning)` when `age_secs >= 600` (10 minutes), `None` otherwise.
/// The warning matches the design doc format:
/// ```text
/// Warning: Index is 15 minutes stale. 7 file changes pending. Consider running reindex.
/// ```
pub fn format_staleness_warning(age_secs: u64, pending_changes: usize) -> Option<String> {
    if age_secs < 600 {
        return None;
    }

    let age_str = format_duration_approx(age_secs);
    let changes_str = if pending_changes > 0 {
        format!(" {pending_changes} file changes pending.")
    } else {
        String::new()
    };

    Some(format!(
        "Warning: Index is {age_str} stale.{changes_str} Consider running reindex.\n"
    ))
}

/// Format a list of files with language and size metadata.
///
/// Output format:
/// ```text
/// Found 23 files matching "config"
///
/// src/config.rs                    (Rust, 2.1 KB)
/// src/config/mod.rs                (Rust, 450 B)
/// ```
pub fn format_file_list(files: &[FileInfo], query: &str) -> String {
    let mut out = String::new();

    writeln!(out, "Found {} files matching \"{query}\"", files.len()).unwrap();

    for file in files {
        writeln!(
            out,
            "{}  ({}, {})",
            file.path,
            file.language,
            format_size(file.size_bytes)
        )
        .unwrap();
    }

    out
}

/// Format file content with line numbers and metadata header.
///
/// Output format:
/// ```text
/// src/index/trigram.rs (lines 1-85 of 142, Rust, indexed 2m ago)
///
///   1 | use std::collections::HashMap;
///   2 | use roaring::RoaringBitmap;
/// ```
pub fn format_file_content(content: &str, path: &str, metadata: &FileFormatMetadata) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len();
    let total = metadata.total_lines;

    let mut out = String::new();

    // Header
    let indexed_info = metadata
        .indexed_ago
        .as_deref()
        .map(|a| format!(", indexed {a}"))
        .unwrap_or_default();
    writeln!(
        out,
        "{path} (lines 1-{line_count} of {total}, {lang}{indexed_info})",
        lang = metadata.language,
    )
    .unwrap();
    writeln!(out).unwrap();

    // Line-numbered content
    let width = total.to_string().len().max(3);
    for (i, line) in lines.iter().enumerate() {
        writeln!(out, "{:>width$} | {line}", i + 1).unwrap();
    }

    out
}

/// Format index status information.
///
/// Output format:
/// ```text
/// indexrs status: healthy
///
/// Files:    1,234 indexed
/// Segments: 3 active
/// Disk:     18.2 MB
/// ```
pub fn format_index_status(status: &IndexStatusInfo) -> String {
    let mut out = String::new();

    writeln!(out, "indexrs status: healthy").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "Files:    {} indexed", status.total_files).unwrap();
    writeln!(out, "Segments: {} active", status.segment_count).unwrap();
    if let Some(disk) = status.disk_size_bytes {
        writeln!(out, "Disk:     {}", format_size(disk)).unwrap();
    }

    out
}

// ---- Internal helpers ----

/// Format a single file match section with `## path` header and line output.
fn format_file_match(out: &mut String, file_match: &FileMatch) {
    writeln!(out, "## {}", file_match.path.display()).unwrap();

    for line_match in &file_match.lines {
        // Context before
        for ctx in &line_match.context_before {
            writeln!(out, "L{}:  {}", ctx.line_number, ctx.content).unwrap();
        }
        // Match line (marked with *)
        format_match_line(out, line_match);
        // Context after
        for ctx in &line_match.context_after {
            writeln!(out, "L{}:  {}", ctx.line_number, ctx.content).unwrap();
        }
    }
}

/// Format a single matching line with `*` gutter marker.
fn format_match_line(out: &mut String, line_match: &LineMatch) {
    writeln!(out, "L{}:* {}", line_match.line_number, line_match.content).unwrap();
}

/// Format a byte size as a human-readable string.
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Format seconds into an approximate human-readable duration.
fn format_duration_approx(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} seconds")
    } else if secs < 3600 {
        format!("{} minutes", secs / 60)
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins == 0 {
            format!("{hours} hours")
        } else {
            format!("{hours}h {mins}m")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use indexrs_core::search::{ContextLine, FileMatch, LineMatch, SearchResult};
    use indexrs_core::types::{FileId, Language};

    use super::*;

    // ---- format_search_results ----

    #[test]
    fn test_format_search_results_basic() {
        let result = SearchResult {
            total_match_count: 2,
            total_file_count: 1,
            files: vec![FileMatch {
                file_id: FileId(0),
                path: PathBuf::from("src/main.rs"),
                language: Language::Rust,
                lines: vec![LineMatch {
                    line_number: 5,
                    content: "fn main() {".to_string(),
                    ranges: vec![(0, 7)],
                    context_before: vec![],
                    context_after: vec![],
                }],
                score: 0.9,
            }],
            duration: Duration::from_millis(5),
        };

        let output = format_search_results(&result, &FormatOptions::default());
        assert!(output.contains("Found 2 matches across 1 files"));
        assert!(output.contains("## src/main.rs"));
        assert!(output.contains("L5:* fn main() {"));
    }

    #[test]
    fn test_format_search_results_all_on_one_page() {
        let result = SearchResult {
            total_match_count: 3,
            total_file_count: 2,
            files: vec![
                FileMatch {
                    file_id: FileId(0),
                    path: PathBuf::from("a.rs"),
                    language: Language::Rust,
                    lines: vec![],
                    score: 0.9,
                },
                FileMatch {
                    file_id: FileId(1),
                    path: PathBuf::from("b.rs"),
                    language: Language::Rust,
                    lines: vec![],
                    score: 0.8,
                },
            ],
            duration: Duration::from_millis(1),
        };

        let output = format_search_results(&result, &FormatOptions::default());
        // When all results fit, no "showing X-Y"
        assert!(output.contains("Found 3 matches across 2 files\n"));
        assert!(!output.contains("showing"));
    }

    #[test]
    fn test_format_search_results_paginated() {
        let result = SearchResult {
            total_match_count: 100,
            total_file_count: 50,
            files: vec![FileMatch {
                file_id: FileId(5),
                path: PathBuf::from("c.rs"),
                language: Language::Rust,
                lines: vec![],
                score: 0.5,
            }],
            duration: Duration::from_millis(10),
        };

        let opts = FormatOptions {
            offset: 10,
            page_size: 20,
        };
        let output = format_search_results(&result, &opts);
        assert!(output.contains("(showing 11-11)"));
    }

    #[test]
    fn test_format_search_results_large_hint() {
        let result = SearchResult {
            total_match_count: 2341,
            total_file_count: 487,
            files: vec![],
            duration: Duration::from_millis(50),
        };

        let output = format_search_results(&result, &FormatOptions::default());
        assert!(output.contains("Tip: Consider narrowing"));
    }

    #[test]
    fn test_format_search_results_no_large_hint_when_small() {
        let result = SearchResult {
            total_match_count: 10,
            total_file_count: 5,
            files: vec![],
            duration: Duration::from_millis(1),
        };

        let output = format_search_results(&result, &FormatOptions::default());
        assert!(!output.contains("Tip:"));
    }

    #[test]
    fn test_format_search_results_with_context() {
        let result = SearchResult {
            total_match_count: 1,
            total_file_count: 1,
            files: vec![FileMatch {
                file_id: FileId(0),
                path: PathBuf::from("lib.rs"),
                language: Language::Rust,
                lines: vec![LineMatch {
                    line_number: 10,
                    content: "let x = trigram();".to_string(),
                    ranges: vec![(8, 15)],
                    context_before: vec![ContextLine {
                        line_number: 9,
                        content: "// comment".to_string(),
                    }],
                    context_after: vec![ContextLine {
                        line_number: 11,
                        content: "println!(\"{x}\");".to_string(),
                    }],
                }],
                score: 0.95,
            }],
            duration: Duration::from_millis(2),
        };

        let output = format_search_results(&result, &FormatOptions::default());
        assert!(output.contains("L9:  // comment"));
        assert!(output.contains("L10:* let x = trigram();"));
        assert!(output.contains("L11:  println!"));
    }

    // ---- format_staleness_warning ----

    #[test]
    fn test_staleness_warning_under_threshold() {
        assert!(format_staleness_warning(599, 0).is_none());
        assert!(format_staleness_warning(0, 5).is_none());
    }

    #[test]
    fn test_staleness_warning_at_threshold() {
        let warning = format_staleness_warning(600, 0).unwrap();
        assert!(warning.contains("10 minutes stale"));
        assert!(warning.contains("Consider running reindex"));
    }

    #[test]
    fn test_staleness_warning_with_pending() {
        let warning = format_staleness_warning(900, 7).unwrap();
        assert!(warning.contains("15 minutes stale"));
        assert!(warning.contains("7 file changes pending"));
    }

    #[test]
    fn test_staleness_warning_hours() {
        let warning = format_staleness_warning(7200, 0).unwrap();
        assert!(warning.contains("2 hours stale"));
    }

    // ---- format_file_list ----

    #[test]
    fn test_format_file_list() {
        let files = vec![
            FileInfo {
                path: "src/config.rs".into(),
                language: Language::Rust,
                size_bytes: 2150,
            },
            FileInfo {
                path: "src/config/mod.rs".into(),
                language: Language::Rust,
                size_bytes: 450,
            },
        ];

        let output = format_file_list(&files, "config");
        assert!(output.contains("Found 2 files matching \"config\""));
        assert!(output.contains("src/config.rs"));
        assert!(output.contains("Rust"));
        assert!(output.contains("2.1 KB"));
        assert!(output.contains("450 B"));
    }

    #[test]
    fn test_format_file_list_empty() {
        let output = format_file_list(&[], "nonexistent");
        assert!(output.contains("Found 0 files matching \"nonexistent\""));
    }

    // ---- format_file_content ----

    #[test]
    fn test_format_file_content() {
        let content = "use std::io;\n\nfn main() {\n    println!(\"hello\");\n}\n";
        let metadata = FileFormatMetadata {
            total_lines: 5,
            language: Language::Rust,
            indexed_ago: Some("2m ago".into()),
        };

        let output = format_file_content(content, "src/main.rs", &metadata);
        assert!(output.contains("src/main.rs (lines 1-5 of 5, Rust, indexed 2m ago)"));
        assert!(output.contains("  1 | use std::io;"));
        assert!(output.contains("  3 | fn main() {"));
    }

    #[test]
    fn test_format_file_content_no_indexed_ago() {
        let content = "line1\nline2\n";
        let metadata = FileFormatMetadata {
            total_lines: 2,
            language: Language::Python,
            indexed_ago: None,
        };

        let output = format_file_content(content, "app.py", &metadata);
        assert!(output.contains("app.py (lines 1-2 of 2, Python)"));
        assert!(!output.contains("indexed"));
    }

    // ---- format_index_status ----

    #[test]
    fn test_format_index_status() {
        let status = IndexStatusInfo {
            total_files: 1234,
            segment_count: 3,
            disk_size_bytes: Some(18_200_000),
        };

        let output = format_index_status(&status);
        assert!(output.contains("indexrs status: healthy"));
        assert!(output.contains("1234 indexed"));
        assert!(output.contains("3 active"));
        assert!(output.contains("17.4 MB"));
    }

    #[test]
    fn test_format_index_status_no_disk() {
        let status = IndexStatusInfo {
            total_files: 0,
            segment_count: 0,
            disk_size_bytes: None,
        };

        let output = format_index_status(&status);
        assert!(output.contains("0 indexed"));
        assert!(!output.contains("Disk:"));
    }

    // ---- format_size helper ----

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn test_format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(2150), "2.1 KB");
    }

    #[test]
    fn test_format_size_megabytes() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(18_200_000), "17.4 MB");
    }

    #[test]
    fn test_format_size_gigabytes() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    // ---- format_duration_approx helper ----

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration_approx(30), "30 seconds");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration_approx(600), "10 minutes");
        assert_eq!(format_duration_approx(900), "15 minutes");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration_approx(3600), "1 hours");
        assert_eq!(format_duration_approx(7200), "2 hours");
        assert_eq!(format_duration_approx(5400), "1h 30m");
    }
}
