use std::sync::Arc;

use ferret_indexer_core::error::IndexError;
use ferret_indexer_core::index_state::SegmentList;
use ferret_indexer_core::query::{LiteralQuery, Query, RegexQuery, match_language, parse_query};
use ferret_indexer_core::search::{MatchPattern, SearchOptions};
use ferret_indexer_daemon::CaseMode;

use crate::color::ColorConfig;
use crate::output::{ExitCode, StreamingWriter};
use crate::paths::PathRewriter;

/// Resolve CLI flags into a MatchPattern.
///
/// If `regex` is true, returns a Regex pattern (always case-sensitive).
/// Otherwise, uses `case_mode` to select literal vs case-insensitive matching.
/// Smart mode: case-sensitive if the query contains any uppercase character.
pub fn resolve_match_pattern(query: &str, regex: bool, case_mode: CaseMode) -> MatchPattern {
    if regex {
        MatchPattern::Regex(query.to_string())
    } else {
        match case_mode {
            CaseMode::Sensitive => MatchPattern::Literal(query.to_string()),
            CaseMode::Insensitive => MatchPattern::LiteralCaseInsensitive(query.to_string()),
            CaseMode::Smart => {
                if query.chars().any(|c| c.is_uppercase()) {
                    MatchPattern::Literal(query.to_string())
                } else {
                    MatchPattern::LiteralCaseInsensitive(query.to_string())
                }
            }
        }
    }
}

/// Convert CLI flags (pattern + optional language) into a Query AST.
///
/// Maps MatchPattern variants to Query leaf nodes. If a language filter is
/// provided, wraps the content query in an AND with LanguageFilter.
///
/// Path glob is NOT included here — Query::PathFilter is prefix-based,
/// but --path supports globs. Path glob filtering stays post-hoc in CLI.
pub fn flags_to_query(pattern: &MatchPattern, language: Option<&str>) -> Result<Query, IndexError> {
    let content_query = match pattern {
        MatchPattern::Literal(s) => Query::Literal(LiteralQuery {
            text: s.clone(),
            case_sensitive: true,
        }),
        MatchPattern::LiteralCaseInsensitive(s) => Query::Literal(LiteralQuery {
            text: s.clone(),
            case_sensitive: false,
        }),
        MatchPattern::Regex(s) => Query::Regex(RegexQuery {
            pattern: s.clone(),
            case_sensitive: true,
        }),
    };

    let Some(lang_str) = language else {
        return Ok(content_query);
    };

    let lang = match_language(lang_str)?;
    Ok(Query::And(vec![Query::LanguageFilter(lang), content_query]))
}

/// Run a search using the advanced query language.
///
/// Parses the query string into a Query AST, executes it through the full
/// query engine pipeline (trigram extraction -> candidate filtering ->
/// boolean verification), and streams results in vimgrep format.
pub fn run_query_search<W: std::io::Write>(
    snapshot: &SegmentList,
    query_str: &str,
    context_lines: usize,
    limit: usize,
    color: &ColorConfig,
    path_rewriter: &PathRewriter,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
    let query = parse_query(query_str)?;
    let search_opts = SearchOptions {
        context_lines,
        max_results: Some(limit),
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let snapshot_clone = Arc::clone(snapshot);
    let search_handle = std::thread::spawn(move || {
        ferret_indexer_core::multi_search::search_segments_with_query_streaming(
            &snapshot_clone,
            &query,
            &search_opts,
            tx,
        )
    });

    let mut has_results = false;
    for file_match in rx {
        has_results = true;
        let raw_path = file_match.path.to_string_lossy();
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

    #[test]
    fn test_resolve_match_pattern_insensitive() {
        let pattern = resolve_match_pattern("hello", false, CaseMode::Insensitive);
        assert!(matches!(pattern, MatchPattern::LiteralCaseInsensitive(_)));
    }

    #[test]
    fn test_resolve_match_pattern_case_sensitive() {
        let pattern = resolve_match_pattern("hello", false, CaseMode::Sensitive);
        assert!(matches!(pattern, MatchPattern::Literal(_)));
    }

    #[test]
    fn test_resolve_match_pattern_regex() {
        let pattern = resolve_match_pattern("fn\\s+", true, CaseMode::Insensitive);
        assert!(matches!(pattern, MatchPattern::Regex(_)));
    }

    #[test]
    fn test_resolve_match_pattern_smart_case_lower() {
        let pattern = resolve_match_pattern("hello", false, CaseMode::Smart);
        assert!(matches!(pattern, MatchPattern::LiteralCaseInsensitive(_)));
    }

    #[test]
    fn test_resolve_match_pattern_smart_case_upper() {
        let pattern = resolve_match_pattern("Hello", false, CaseMode::Smart);
        assert!(matches!(pattern, MatchPattern::Literal(_)));
    }

    #[test]
    fn test_flags_to_query_case_insensitive() {
        let pattern = MatchPattern::LiteralCaseInsensitive("hello".to_string());
        let query = flags_to_query(&pattern, None).unwrap();
        assert_eq!(
            query,
            Query::Literal(LiteralQuery {
                text: "hello".to_string(),
                case_sensitive: false,
            })
        );
    }

    #[test]
    fn test_flags_to_query_case_sensitive() {
        let pattern = MatchPattern::Literal("Hello".to_string());
        let query = flags_to_query(&pattern, None).unwrap();
        assert_eq!(
            query,
            Query::Literal(LiteralQuery {
                text: "Hello".to_string(),
                case_sensitive: true,
            })
        );
    }

    #[test]
    fn test_flags_to_query_regex() {
        let pattern = MatchPattern::Regex("fn\\s+".to_string());
        let query = flags_to_query(&pattern, None).unwrap();
        assert_eq!(
            query,
            Query::Regex(RegexQuery {
                pattern: "fn\\s+".to_string(),
                case_sensitive: true,
            })
        );
    }

    #[test]
    fn test_flags_to_query_with_language() {
        let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
        let query = flags_to_query(&pattern, Some("rust")).unwrap();
        if let Query::And(children) = &query {
            assert_eq!(children.len(), 2);
            assert!(matches!(children[0], Query::LanguageFilter(_)));
        } else {
            panic!("expected And, got {query:?}");
        }
    }

    #[test]
    fn test_flags_to_query_unknown_language() {
        let pattern = MatchPattern::LiteralCaseInsensitive("hello".to_string());
        assert!(flags_to_query(&pattern, Some("brainfuck")).is_err());
    }
}
