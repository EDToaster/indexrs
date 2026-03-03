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

use std::fmt::Write;

use indexrs_core::search::{FileMatch, LineMatch, SearchResult};
use indexrs_core::types::Language;

// ---- Types for file list / file content formatting ----

/// Information about a single file for the file list formatter.
pub struct FileListEntry {
    pub path: String,
    pub language: Language,
    pub size_bytes: u32,
}

/// Options for controlling the search results format output.
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

// ---- Search results formatting ----

/// Format search results as plain text for MCP tool responses.
///
/// Produces output like:
/// ```text
/// Found 47 matches across 12 files (showing 1-12)
///
/// ## src/main.rs (Rust, 3 matches)
/// L42:  fn build_trigram_index(&mut self, content: &str) {
/// L43:* for trigram in content.trigrams() {
/// L44:      index.insert(trigram, self.current_doc_id);
/// ```
pub fn format_search_results(result: &SearchResult, offset: usize) -> String {
    let mut out = String::new();

    // Summary line
    let file_count = result.files.len();
    let showing_start = offset + 1;
    let showing_end = offset + file_count;

    if result.total_file_count > file_count || offset > 0 {
        writeln!(
            out,
            "Found {} matches across {} files (showing {}-{}, offset={})",
            result.total_match_count, result.total_file_count, showing_start, showing_end, offset,
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "Found {} matches across {} files (showing {}-{})",
            result.total_match_count, result.total_file_count, showing_start, showing_end,
        )
        .unwrap();
    }

    // Tip for very large result sets
    if result.total_file_count > 100 {
        writeln!(
            out,
            "Tip: Consider narrowing with path:, language:, or a more specific query."
        )
        .unwrap();
    }

    // File sections
    for file_match in &result.files {
        writeln!(out).unwrap();
        writeln!(
            out,
            "## {} ({}, {} matches)",
            file_match.path.display(),
            file_match.language,
            file_match.lines.len(),
        )
        .unwrap();

        for line_match in &file_match.lines {
            // Context before
            for ctx in &line_match.context_before {
                writeln!(out, "L{}:  {}", ctx.line_number, ctx.content).unwrap();
            }
            // Match line (marked with *)
            format_match_line(&mut out, line_match);
            // Context after
            for ctx in &line_match.context_after {
                writeln!(out, "L{}:  {}", ctx.line_number, ctx.content).unwrap();
            }
        }
    }

    out
}

/// Format search results using `FormatOptions` (foundation API, used by tests).
pub fn format_search_results_with_options(
    results: &SearchResult,
    options: &FormatOptions,
) -> String {
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

// ---- File list formatting (used by search_files tool) ----

/// Format a list of files as plain text.
///
/// Output format:
/// ```text
/// Found 23 files matching "config"
///
/// src/config.rs                    (Rust, 2.1 KB)
/// src/config/mod.rs                (Rust, 450 B)
/// ```
pub fn format_file_list(
    query: &str,
    total_count: usize,
    entries: &[FileListEntry],
    offset: usize,
) -> String {
    let mut out = String::new();

    if total_count == 0 {
        out.push_str(&format!("No files found matching \"{query}\"."));
        return out;
    }

    if offset == 0 && entries.len() == total_count {
        writeln!(out, "Found {total_count} files matching \"{query}\"").unwrap();
    } else {
        let start = offset + 1;
        let end = offset + entries.len();
        writeln!(
            out,
            "Found {total_count} files matching \"{query}\" (showing {start}-{end})"
        )
        .unwrap();
    }

    writeln!(out).unwrap();
    for entry in entries {
        let lang_str = if entry.language == Language::Unknown {
            String::new()
        } else {
            format!("{}", entry.language)
        };
        let size_str = format_size(entry.size_bytes as u64);
        if lang_str.is_empty() {
            writeln!(out, "{}    ({size_str})", entry.path).unwrap();
        } else {
            writeln!(out, "{}    ({lang_str}, {size_str})", entry.path).unwrap();
        }
    }

    out
}

/// Format a list of files using `FileInfo` (foundation API, used by tests).
pub fn format_file_info_list(files: &[FileInfo], query: &str) -> String {
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

// ---- File content formatting (used by get_file tool) ----

/// Format file content with line numbers.
///
/// Output format:
/// ```text
/// src/index/trigram.rs (lines 1-85 of 142, Rust)
///
///   1 | use std::collections::HashMap;
///   2 | use roaring::RoaringBitmap;
/// ```
pub fn format_file_content(
    path: &str,
    language: Language,
    total_lines: usize,
    start_line: usize,
    lines: &[&str],
    truncated: bool,
) -> String {
    let mut out = String::new();

    if lines.is_empty() {
        let lang_str = if language == Language::Unknown {
            String::new()
        } else {
            format!(", {language}")
        };
        writeln!(out, "{path} (empty file{lang_str})").unwrap();
        return out;
    }

    let end_line = start_line + lines.len() - 1;
    let lang_str = if language == Language::Unknown {
        String::new()
    } else {
        format!(", {language}")
    };
    writeln!(
        out,
        "{path} (lines {start_line}-{end_line} of {total_lines}{lang_str})"
    )
    .unwrap();
    writeln!(out).unwrap();

    let width = format!("{}", start_line + lines.len()).len();
    for (i, line) in lines.iter().enumerate() {
        let line_num = start_line + i;
        writeln!(out, "{line_num:>width$} | {line}").unwrap();
    }

    if truncated {
        writeln!(
            out,
            "\n(truncated at line {end_line} -- use start_line/end_line to read more)"
        )
        .unwrap();
    }

    out
}

/// Format file content using `FileFormatMetadata` (foundation API).
pub fn format_file_content_with_metadata(
    content: &str,
    path: &str,
    metadata: &FileFormatMetadata,
) -> String {
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

// ---- Index status formatting ----

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
pub fn format_size(bytes: u64) -> String {
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
            if hours == 1 {
                "1 hour".to_string()
            } else {
                format!("{hours} hours")
            }
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

    // ---- format_search_results (search-code agent style) ----

    fn make_result(files: Vec<FileMatch>, total_file_count: usize) -> SearchResult {
        let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();
        SearchResult {
            total_match_count,
            total_file_count,
            files,
            duration: Duration::from_millis(5),
        }
    }

    #[test]
    fn test_format_empty_results() {
        let result = make_result(vec![], 0);
        let text = format_search_results(&result, 0);
        assert!(text.contains("Found 0 matches across 0 files"));
    }

    #[test]
    fn test_format_single_file_single_match() {
        let result = make_result(
            vec![FileMatch {
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
            1,
        );
        let text = format_search_results(&result, 0);
        assert!(text.contains("Found 1 matches across 1 files"));
        assert!(text.contains("## src/main.rs (Rust, 1 matches)"));
        assert!(text.contains("L5:* fn main() {"));
    }

    #[test]
    fn test_format_with_context_lines() {
        let result = make_result(
            vec![FileMatch {
                file_id: FileId(0),
                path: PathBuf::from("src/main.rs"),
                language: Language::Rust,
                lines: vec![LineMatch {
                    line_number: 5,
                    content: "fn main() {".to_string(),
                    ranges: vec![(0, 7)],
                    context_before: vec![ContextLine {
                        line_number: 4,
                        content: "/// Entry point".to_string(),
                    }],
                    context_after: vec![ContextLine {
                        line_number: 6,
                        content: "    println!(\"hello\");".to_string(),
                    }],
                }],
                score: 0.9,
            }],
            1,
        );
        let text = format_search_results(&result, 0);
        assert!(text.contains("L4:  /// Entry point"));
        assert!(text.contains("L5:* fn main() {"));
        assert!(text.contains("L6:  "));
    }

    #[test]
    fn test_format_with_pagination_offset() {
        let result = make_result(
            vec![FileMatch {
                file_id: FileId(0),
                path: PathBuf::from("src/lib.rs"),
                language: Language::Rust,
                lines: vec![LineMatch {
                    line_number: 1,
                    content: "pub mod foo;".to_string(),
                    ranges: vec![],
                    context_before: vec![],
                    context_after: vec![],
                }],
                score: 0.5,
            }],
            10,
        );
        let text = format_search_results(&result, 5);
        assert!(text.contains("showing 6-6, offset=5"));
        assert!(text.contains("10 files"));
    }

    #[test]
    fn test_format_large_result_set_tip() {
        let result = SearchResult {
            total_match_count: 500,
            total_file_count: 200,
            files: vec![],
            duration: Duration::from_millis(50),
        };
        let text = format_search_results(&result, 0);
        assert!(text.contains("Tip: Consider narrowing"));
    }

    // ---- format_search_results_with_options (foundation API) ----

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

        let output = format_search_results_with_options(&result, &FormatOptions::default());
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

        let output = format_search_results_with_options(&result, &FormatOptions::default());
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
        let output = format_search_results_with_options(&result, &opts);
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

        let output = format_search_results_with_options(&result, &FormatOptions::default());
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

        let output = format_search_results_with_options(&result, &FormatOptions::default());
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

        let output = format_search_results_with_options(&result, &FormatOptions::default());
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

    // ---- format_file_list (file-tools agent API) ----

    #[test]
    fn test_format_file_list_no_results() {
        let result = format_file_list("missing", 0, &[], 0);
        assert_eq!(result, "No files found matching \"missing\".");
    }

    #[test]
    fn test_format_file_list_simple() {
        let entries = vec![
            FileListEntry {
                path: "src/main.rs".to_string(),
                language: Language::Rust,
                size_bytes: 2150,
            },
            FileListEntry {
                path: "src/lib.rs".to_string(),
                language: Language::Rust,
                size_bytes: 450,
            },
        ];
        let result = format_file_list("main", 2, &entries, 0);
        assert!(result.contains("Found 2 files matching \"main\""));
        assert!(result.contains("src/main.rs"));
        assert!(result.contains("(Rust, 2.1 KB)"));
        assert!(result.contains("src/lib.rs"));
        assert!(result.contains("(Rust, 450 B)"));
    }

    #[test]
    fn test_format_file_list_with_pagination() {
        let entries = vec![FileListEntry {
            path: "b.rs".to_string(),
            language: Language::Rust,
            size_bytes: 100,
        }];
        let result = format_file_list("config", 10, &entries, 5);
        assert!(result.contains("showing 6-6"));
    }

    #[test]
    fn test_format_file_list_unknown_language() {
        let entries = vec![FileListEntry {
            path: "Makefile".to_string(),
            language: Language::Unknown,
            size_bytes: 200,
        }];
        let result = format_file_list("make", 1, &entries, 0);
        assert!(result.contains("Makefile"));
        assert!(result.contains("(200 B)"));
        // Should NOT contain "Unknown"
        assert!(!result.contains("Unknown"));
    }

    // ---- format_file_info_list (foundation API) ----

    #[test]
    fn test_format_file_info_list() {
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

        let output = format_file_info_list(&files, "config");
        assert!(output.contains("Found 2 files matching \"config\""));
        assert!(output.contains("src/config.rs"));
        assert!(output.contains("Rust"));
        assert!(output.contains("2.1 KB"));
        assert!(output.contains("450 B"));
    }

    #[test]
    fn test_format_file_info_list_empty() {
        let output = format_file_info_list(&[], "nonexistent");
        assert!(output.contains("Found 0 files matching \"nonexistent\""));
    }

    // ---- format_file_content (file-tools agent API) ----

    #[test]
    fn test_format_file_content_basic() {
        let lines = vec!["fn main() {}", "    println!(\"hello\");", "}"];
        let result = format_file_content("src/main.rs", Language::Rust, 3, 1, &lines, false);
        assert!(result.contains("src/main.rs (lines 1-3 of 3, Rust)"));
        assert!(result.contains("1 | fn main() {}"));
        assert!(result.contains("2 |     println!(\"hello\");"));
        assert!(result.contains("3 | }"));
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn test_format_file_content_truncated() {
        let lines = vec!["line1", "line2"];
        let result = format_file_content("a.rs", Language::Rust, 1000, 499, &lines, true);
        assert!(result.contains("lines 499-500 of 1000"));
        assert!(result.contains("truncated at line 500"));
    }

    #[test]
    fn test_format_file_content_line_number_width() {
        let lines: Vec<&str> = vec!["x", "x", "x"];
        let result = format_file_content("a.rs", Language::Rust, 1000, 998, &lines, false);
        // Line numbers 998, 999, 1000 -- all should be 4 digits wide
        assert!(result.contains(" 998 | x"));
        assert!(result.contains(" 999 | x"));
        assert!(result.contains("1000 | x"));
    }

    #[test]
    fn test_format_file_content_empty() {
        let lines: Vec<&str> = vec![];
        let result = format_file_content("empty.rs", Language::Rust, 0, 1, &lines, false);
        assert!(result.contains("empty.rs (empty file, Rust)"));
    }

    // ---- format_file_content_with_metadata (foundation API) ----

    #[test]
    fn test_format_file_content_with_metadata_basic() {
        let content = "use std::io;\n\nfn main() {\n    println!(\"hello\");\n}\n";
        let metadata = FileFormatMetadata {
            total_lines: 5,
            language: Language::Rust,
            indexed_ago: Some("2m ago".into()),
        };

        let output = format_file_content_with_metadata(content, "src/main.rs", &metadata);
        assert!(output.contains("src/main.rs (lines 1-5 of 5, Rust, indexed 2m ago)"));
        assert!(output.contains("  1 | use std::io;"));
        assert!(output.contains("  3 | fn main() {"));
    }

    #[test]
    fn test_format_file_content_with_metadata_no_indexed_ago() {
        let content = "line1\nline2\n";
        let metadata = FileFormatMetadata {
            total_lines: 2,
            language: Language::Python,
            indexed_ago: None,
        };

        let output = format_file_content_with_metadata(content, "app.py", &metadata);
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
        assert_eq!(format_duration_approx(3600), "1 hour");
        assert_eq!(format_duration_approx(7200), "2 hours");
        assert_eq!(format_duration_approx(5400), "1h 30m");
    }
}
