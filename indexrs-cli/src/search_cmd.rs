use std::sync::Arc;

use globset::{Glob, GlobMatcher};
use indexrs_core::error::IndexError;
use indexrs_core::index_state::SegmentList;
use indexrs_core::multi_search::{
    search_segments_streaming, search_segments_with_pattern_and_options,
};
use indexrs_core::search::{MatchPattern, SearchOptions};

use crate::color::ColorConfig;
use crate::output::{ExitCode, StreamingWriter};
use crate::paths::PathRewriter;

pub struct SearchCmdOptions {
    pub pattern: MatchPattern,
    pub context_lines: usize,
    pub limit: usize,
    pub language: Option<String>,
    pub path_glob: Option<String>,
    #[allow(dead_code)]
    pub stats: bool,
}

/// Resolve CLI flags into a MatchPattern.
///
/// Priority: regex > case_sensitive > ignore_case > smart_case > default (smart case).
/// Smart case: case-sensitive if the query contains any uppercase character,
/// otherwise case-insensitive.
pub fn resolve_match_pattern(
    query: &str,
    regex: bool,
    case_sensitive: bool,
    ignore_case: bool,
    smart_case: bool,
) -> MatchPattern {
    if regex {
        MatchPattern::Regex(query.to_string())
    } else if case_sensitive {
        MatchPattern::Literal(query.to_string())
    } else if ignore_case {
        MatchPattern::LiteralCaseInsensitive(query.to_string())
    } else if smart_case || (!case_sensitive && !ignore_case) {
        // Smart case: if query has uppercase, treat as case-sensitive
        if query.chars().any(|c| c.is_uppercase()) {
            MatchPattern::Literal(query.to_string())
        } else {
            MatchPattern::LiteralCaseInsensitive(query.to_string())
        }
    } else {
        MatchPattern::LiteralCaseInsensitive(query.to_string())
    }
}

/// Run the search command: search segments, format as vimgrep, stream to output.
///
/// This is the batch version that collects all results before outputting.
/// For incremental output, use [`run_search_streaming`] instead.
#[allow(dead_code)]
pub fn run_search<W: std::io::Write>(
    snapshot: &SegmentList,
    opts: &SearchCmdOptions,
    color: &ColorConfig,
    path_rewriter: &PathRewriter,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
    let search_opts = SearchOptions {
        context_lines: opts.context_lines,
        max_results: Some(opts.limit),
    };

    let result = search_segments_with_pattern_and_options(snapshot, &opts.pattern, &search_opts)?;

    if result.files.is_empty() {
        return Ok(ExitCode::NoResults);
    }

    let glob_matcher: Option<GlobMatcher> = opts
        .path_glob
        .as_ref()
        .map(|g| Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;

    for file_match in &result.files {
        let raw_path = file_match.path.to_string_lossy();

        // Path filter (use raw repo-relative path for glob matching)
        if let Some(ref matcher) = glob_matcher
            && !matcher.is_match(raw_path.as_ref())
        {
            continue;
        }

        // Language filter
        if let Some(ref lang) = opts.language
            && !file_match.language.to_string().eq_ignore_ascii_case(lang)
        {
            continue;
        }

        let path_str = path_rewriter.rewrite(&raw_path);

        for line_match in &file_match.lines {
            let col = line_match
                .ranges
                .first()
                .map(|(start, _)| start + 1)
                .unwrap_or(1);

            let line = color.format_search_line(
                &path_str,
                line_match.line_number,
                col,
                &line_match.content,
                &line_match.ranges,
            );

            if writer.write_line(&line).is_err() {
                break;
            }
        }
    }
    let _ = writer.finish();

    if opts.stats {
        eprintln!(
            "{} matches in {} files ({:.1?})",
            result.total_match_count, result.total_file_count, result.duration
        );
    }

    Ok(ExitCode::Success)
}

/// Run the search command in streaming mode: results are displayed as they're found.
///
/// Uses `search_segments_streaming` to send results through a channel,
/// formatting and writing each one as it arrives. This gives the user
/// immediate feedback, which is critical for fzf integration.
pub fn run_search_streaming<W: std::io::Write>(
    snapshot: &SegmentList,
    opts: &SearchCmdOptions,
    color: &ColorConfig,
    path_rewriter: &PathRewriter,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
    let search_opts = SearchOptions {
        context_lines: opts.context_lines,
        max_results: Some(opts.limit),
    };

    let glob_matcher: Option<GlobMatcher> = opts
        .path_glob
        .as_ref()
        .map(|g| Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;

    let (tx, rx) = std::sync::mpsc::channel();
    let pattern = opts.pattern.clone();

    // Run the search on a background thread so we can consume results on this thread
    let snapshot_clone = Arc::clone(snapshot);
    let search_handle = std::thread::spawn(move || {
        search_segments_streaming(&snapshot_clone, &pattern, &search_opts, tx)
    });

    let mut has_results = false;
    for file_match in rx {
        let raw_path = file_match.path.to_string_lossy();

        // Path filter (use raw repo-relative path for glob matching)
        if let Some(ref matcher) = glob_matcher
            && !matcher.is_match(raw_path.as_ref())
        {
            continue;
        }

        // Language filter
        if let Some(ref lang) = opts.language
            && !file_match.language.to_string().eq_ignore_ascii_case(lang)
        {
            continue;
        }

        has_results = true;
        let path_str = path_rewriter.rewrite(&raw_path);

        for line_match in &file_match.lines {
            let col = line_match
                .ranges
                .first()
                .map(|(start, _)| start + 1)
                .unwrap_or(1);

            let line = color.format_search_line(
                &path_str,
                line_match.line_number,
                col,
                &line_match.content,
                &line_match.ranges,
            );

            if writer.write_line(&line).is_err() {
                // SIGPIPE or broken pipe -- stop consuming to cancel search
                break;
            }
        }
    }
    let _ = writer.finish();

    // Check for search errors
    match search_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(IndexError::Io(std::io::Error::other(
                "search thread panicked",
            )));
        }
    }

    Ok(if has_results {
        ExitCode::Success
    } else {
        ExitCode::NoResults
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexrs_core::SegmentManager;
    use indexrs_core::segment::InputFile;
    use std::path::Path;

    fn build_test_index(dir: &Path) -> SegmentManager {
        let indexrs_dir = dir.join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        let manager = SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {\n    println!(\"hello world\");\n}\n".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "src/lib.rs".to_string(),
                    content: b"pub fn greeting() -> &'static str {\n    \"hello\"\n}\n".to_vec(),
                    mtime: 200,
                },
            ])
            .unwrap();
        manager
    }

    #[test]
    fn test_resolve_match_pattern_literal() {
        let pattern = resolve_match_pattern("hello", false, false, true, false);
        assert!(matches!(pattern, MatchPattern::LiteralCaseInsensitive(_)));
    }

    #[test]
    fn test_resolve_match_pattern_case_sensitive() {
        let pattern = resolve_match_pattern("hello", false, true, false, false);
        assert!(matches!(pattern, MatchPattern::Literal(_)));
    }

    #[test]
    fn test_resolve_match_pattern_regex() {
        let pattern = resolve_match_pattern("fn\\s+", true, false, false, false);
        assert!(matches!(pattern, MatchPattern::Regex(_)));
    }

    #[test]
    fn test_resolve_match_pattern_smart_case_lower() {
        let pattern = resolve_match_pattern("hello", false, false, false, true);
        assert!(matches!(pattern, MatchPattern::LiteralCaseInsensitive(_)));
    }

    #[test]
    fn test_resolve_match_pattern_smart_case_upper() {
        let pattern = resolve_match_pattern("Hello", false, false, false, true);
        assert!(matches!(pattern, MatchPattern::Literal(_)));
    }

    #[test]
    fn test_search_vimgrep_format() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let mut buf = Vec::new();
        let color = ColorConfig::new(false);

        let opts = SearchCmdOptions {
            pattern: MatchPattern::LiteralCaseInsensitive("println".to_string()),
            context_lines: 0,
            limit: 1000,
            language: None,
            path_glob: None,
            stats: false,
        };

        let exit = {
            let mut writer = StreamingWriter::new(&mut buf);
            run_search(
                &snapshot,
                &opts,
                &color,
                &PathRewriter::identity(),
                &mut writer,
            )
            .unwrap()
        };
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("src/main.rs:2:"));
        assert!(output.contains("println"));
        assert!(matches!(exit, ExitCode::Success));
    }

    #[test]
    fn test_search_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let mut buf = Vec::new();
        let color = ColorConfig::new(false);

        let opts = SearchCmdOptions {
            pattern: MatchPattern::LiteralCaseInsensitive("nonexistent_string_xyz".to_string()),
            context_lines: 0,
            limit: 1000,
            language: None,
            path_glob: None,
            stats: false,
        };

        let exit = {
            let mut writer = StreamingWriter::new(&mut buf);
            run_search(
                &snapshot,
                &opts,
                &color,
                &PathRewriter::identity(),
                &mut writer,
            )
            .unwrap()
        };
        assert!(matches!(exit, ExitCode::NoResults));
    }

    #[test]
    fn test_search_streaming_vimgrep_format() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let mut buf = Vec::new();
        let color = ColorConfig::new(false);

        let opts = SearchCmdOptions {
            pattern: MatchPattern::LiteralCaseInsensitive("println".to_string()),
            context_lines: 0,
            limit: 1000,
            language: None,
            path_glob: None,
            stats: false,
        };

        let exit = {
            let mut writer = StreamingWriter::new(&mut buf);
            run_search_streaming(
                &snapshot,
                &opts,
                &color,
                &PathRewriter::identity(),
                &mut writer,
            )
            .unwrap()
        };
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("src/main.rs:2:"));
        assert!(output.contains("println"));
        assert!(matches!(exit, ExitCode::Success));
    }

    #[test]
    fn test_search_streaming_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let mut buf = Vec::new();
        let color = ColorConfig::new(false);

        let opts = SearchCmdOptions {
            pattern: MatchPattern::LiteralCaseInsensitive("nonexistent_string_xyz".to_string()),
            context_lines: 0,
            limit: 1000,
            language: None,
            path_glob: None,
            stats: false,
        };

        let exit = {
            let mut writer = StreamingWriter::new(&mut buf);
            run_search_streaming(
                &snapshot,
                &opts,
                &color,
                &PathRewriter::identity(),
                &mut writer,
            )
            .unwrap()
        };
        assert!(matches!(exit, ExitCode::NoResults));
    }

    #[test]
    fn test_search_rewrites_paths_to_cwd_relative() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let mut buf = Vec::new();
        let color = ColorConfig::new(false);
        // Simulate CWD = repo/src (inside repo)
        let rewriter = PathRewriter::new(Path::new("/repo"), Path::new("/repo/src"));

        let opts = SearchCmdOptions {
            pattern: MatchPattern::LiteralCaseInsensitive("println".to_string()),
            context_lines: 0,
            limit: 1000,
            language: None,
            path_glob: None,
            stats: false,
        };

        let exit = {
            let mut writer = StreamingWriter::new(&mut buf);
            run_search(&snapshot, &opts, &color, &rewriter, &mut writer).unwrap()
        };
        let output = String::from_utf8(buf).unwrap();

        // "src/main.rs" should become "main.rs" with CWD = /repo/src
        assert!(
            output.contains("main.rs:2:"),
            "expected rewritten path, got: {output}"
        );
        assert!(
            !output.contains("src/main.rs:"),
            "path should not have src/ prefix"
        );
        assert!(matches!(exit, ExitCode::Success));
    }
}
