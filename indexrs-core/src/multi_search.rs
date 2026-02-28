//! Multi-segment search with snapshot isolation.
//!
//! Provides [`search_segments()`] which executes a query across multiple segments,
//! filtering tombstoned entries, verifying matches in file content, deduplicating
//! across segments (preferring the newest), and returning a unified [`SearchResult`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::error::IndexError;
use crate::index_state::SegmentList;
use crate::intersection::find_candidates;
use crate::ranking::{MatchType, RankingConfig, ScoringInput, score_file_match};
use crate::search::{ContextLine, FileMatch, LineMatch, MatchPattern, SearchOptions, SearchResult};
use crate::segment::Segment;
use crate::tombstone::TombstoneSet;
use crate::types::SegmentId;
use crate::verify::ContentVerifier;

/// Verify that a query string actually appears in file content, and return
/// the matching lines with highlight ranges and optional context lines.
///
/// This is the content verification step after trigram candidate filtering.
/// For each line in `content`, finds all occurrences of `query` using
/// case-insensitive (ASCII-folded) matching and builds a `LineMatch` with
/// 1-based line numbers and byte-offset highlight ranges.
///
/// When `context_lines > 0`, each match also includes surrounding context
/// lines (up to `context_lines` before and after).
///
/// Returns an empty vector if the query is empty or not found.
fn verify_content_matches(content: &[u8], query: &str, context_lines: usize) -> Vec<LineMatch> {
    if query.is_empty() || content.is_empty() {
        return Vec::new();
    }

    // Fold query to lowercase for case-insensitive matching.
    let folded_query: Vec<u8> = query.bytes().map(crate::trigram::ascii_fold_byte).collect();
    let text = String::from_utf8_lossy(content);
    let all_lines: Vec<&str> = text.split('\n').collect();

    // First pass: find matching line indices and their ranges
    let mut match_indices: Vec<(usize, Vec<(usize, usize)>)> = Vec::new();

    for (line_idx, line) in all_lines.iter().enumerate() {
        // Skip empty trailing line from trailing newline
        if line.is_empty() && line_idx > 0 && line_idx == all_lines.len() - 1 {
            continue;
        }

        let line_bytes = line.as_bytes();
        // Fold the line bytes for searching.
        let folded_line: Vec<u8> = line_bytes
            .iter()
            .map(|&b| crate::trigram::ascii_fold_byte(b))
            .collect();
        let mut ranges = Vec::new();
        let mut search_start = 0;

        while search_start + folded_query.len() <= folded_line.len() {
            if let Some(pos) = find_substring(&folded_line[search_start..], &folded_query) {
                let abs_start = search_start + pos;
                let abs_end = abs_start + folded_query.len();
                ranges.push((abs_start, abs_end));
                search_start = abs_start + 1; // advance past start to find overlapping matches
            } else {
                break;
            }
        }

        if !ranges.is_empty() {
            match_indices.push((line_idx, ranges));
        }
    }

    // Second pass: build LineMatch with context
    match_indices
        .iter()
        .map(|(line_idx, ranges)| {
            let line_idx = *line_idx;

            let context_before = if context_lines > 0 {
                let start = line_idx.saturating_sub(context_lines);
                (start..line_idx)
                    .map(|i| ContextLine {
                        line_number: (i + 1) as u32,
                        content: all_lines[i].to_string(),
                    })
                    .collect()
            } else {
                vec![]
            };

            let context_after = if context_lines > 0 {
                let end = (line_idx + 1 + context_lines).min(all_lines.len());
                // Skip trailing empty line
                let effective_end = if end > 0
                    && all_lines.last().is_some_and(|l| l.is_empty())
                    && end == all_lines.len()
                {
                    end - 1
                } else {
                    end
                };
                ((line_idx + 1)..effective_end)
                    .map(|i| ContextLine {
                        line_number: (i + 1) as u32,
                        content: all_lines[i].to_string(),
                    })
                    .collect()
            } else {
                vec![]
            };

            LineMatch {
                line_number: (line_idx + 1) as u32,
                content: all_lines[line_idx].to_string(),
                ranges: ranges.clone(),
                context_before,
                context_after,
            }
        })
        .collect()
}

/// Find the first occurrence of `needle` in `haystack`, returning the byte offset.
fn find_substring(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Search across multiple segments with snapshot isolation.
///
/// This is the main entry point for multi-segment queries. It:
/// 1. Takes a snapshot (`SegmentList`) and a query string
/// 2. For each segment: loads tombstones, runs `search_single_segment_with_context`
/// 3. Merges results: deduplicates by file path, preferring the newest segment
///    (highest `SegmentId`)
/// 4. Sorts by relevance score (descending)
/// 5. Returns a `SearchResult` with timing information
///
/// # Deduplication Strategy
///
/// When the same file path appears in multiple segments (e.g., a file was
/// modified and re-indexed), only the result from the segment with the highest
/// `SegmentId` is kept. This ensures callers see the most recent version.
///
/// # Edge Cases
///
/// - Empty snapshot: returns an empty `SearchResult`
/// - Query shorter than 3 chars: no trigrams can be extracted, returns empty
/// - All matches tombstoned: returns empty
pub fn search_segments(snapshot: &SegmentList, query: &str) -> Result<SearchResult, IndexError> {
    search_segments_with_options(snapshot, query, &SearchOptions::default())
}

/// Search across multiple segments with options.
///
/// Like [`search_segments()`] but accepts [`SearchOptions`] to configure
/// context lines and other search parameters.
pub fn search_segments_with_options(
    snapshot: &SegmentList,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResult, IndexError> {
    let start = Instant::now();

    if snapshot.is_empty() || query.len() < 3 {
        return Ok(SearchResult {
            total_match_count: 0,
            total_file_count: 0,
            files: Vec::new(),
            duration: start.elapsed(),
        });
    }

    // Collect results from all segments, tagged with segment ID for dedup
    // Key: file path -> (segment_id, FileMatch)
    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();

    for segment in snapshot.iter() {
        let tombstones = segment.load_tombstones()?;
        let file_matches =
            search_single_segment_with_context(segment, query, &tombstones, options.context_lines)?;

        for fm in file_matches {
            let seg_id = segment.segment_id();
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {
                    // Existing result is from a newer or same segment, keep it
                }
                _ => {
                    // This segment is newer, or path not seen yet
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
            if let Some(max) = options.max_results
                && merged.len() >= max
            {
                break;
            }
        }
    }

    // Extract FileMatch values and sort by score descending
    let mut files: Vec<FileMatch> = merged.into_values().map(|(_, fm)| fm).collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    let total_file_count = files.len();
    let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_match_count,
        total_file_count,
        files,
        duration: start.elapsed(),
    })
}

/// Search a single segment with context line support.
fn search_single_segment_with_context(
    segment: &Segment,
    query: &str,
    tombstones: &TombstoneSet,
    context_lines: usize,
) -> Result<Vec<FileMatch>, IndexError> {
    let candidates = find_candidates(segment.trigram_reader(), query)?;

    let mut file_matches = Vec::new();

    for file_id in candidates {
        // Skip tombstoned entries
        if tombstones.contains(file_id) {
            continue;
        }

        // Read metadata
        let meta = match segment.get_metadata(file_id)? {
            Some(m) => m,
            None => continue,
        };

        // Read and decompress content
        let content = segment
            .content_reader()
            .read_content(meta.content_offset, meta.content_len)?;

        // Verify the query actually appears in the content
        let line_matches = verify_content_matches(&content, query, context_lines);
        if line_matches.is_empty() {
            continue;
        }

        // Compute relevance score using the weighted ranking system
        let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let input = ScoringInput {
            path: &meta.path,
            query,
            match_type: MatchType::Substring,
            match_count: total_match_ranges,
            line_count: meta.line_count,
            mtime_epoch_secs: meta.mtime_epoch_secs,
            now_epoch_secs: now,
        };
        let config = RankingConfig::default();
        let score = score_file_match(&input, &config);

        file_matches.push(FileMatch {
            file_id,
            path: PathBuf::from(&meta.path),
            language: meta.language,
            lines: line_matches,
            score,
        });
    }

    Ok(file_matches)
}

/// Extract the literal prefix from a regex pattern for trigram candidate filtering.
///
/// Returns the longest prefix of the pattern that contains only literal characters
/// (no regex metacharacters). This is used for trigram-based candidate filtering
/// before full regex verification.
fn regex_literal_prefix(pattern: &str) -> String {
    let mut prefix = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(&ch) = chars.peek() {
        match ch {
            // Regex metacharacters that signal end of literal prefix
            '.' | '*' | '+' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '^' | '$' => {
                break;
            }
            '\\' => {
                // Escaped character: consume backslash and the next char
                chars.next();
                if let Some(&next) = chars.peek() {
                    match next {
                        // These are regex character classes, not literals
                        'd' | 'D' | 'w' | 'W' | 's' | 'S' | 'b' | 'B' => break,
                        // Literal escaped chars
                        _ => {
                            prefix.push(next);
                            chars.next();
                        }
                    }
                } else {
                    break;
                }
            }
            _ => {
                prefix.push(ch);
                chars.next();
            }
        }
    }
    prefix
}

/// Search a single segment using a `MatchPattern` for verification.
fn search_single_segment_with_pattern(
    segment: &Segment,
    pattern: &MatchPattern,
    tombstones: &TombstoneSet,
    context_lines: usize,
) -> Result<Vec<FileMatch>, IndexError> {
    // Extract the literal text for trigram candidate filtering.
    // For Regex patterns, we extract the literal prefix before metacharacters.
    let trigram_query: String = match pattern {
        MatchPattern::Literal(s) | MatchPattern::LiteralCaseInsensitive(s) => s.clone(),
        MatchPattern::Regex(s) => regex_literal_prefix(s),
    };

    // Get candidate file IDs via trigram lookup.
    // If the trigram query is < 3 chars (too short for trigrams), we need to
    // scan all files in the segment for regex patterns (they may still match),
    // but for literal patterns, no trigram lookup is possible.
    let candidates = if trigram_query.len() >= 3 {
        find_candidates(segment.trigram_reader(), &trigram_query)?
    } else if matches!(pattern, MatchPattern::Regex(_)) {
        // Regex with short/no literal prefix: scan all files
        segment.all_file_ids()?
    } else {
        return Ok(Vec::new());
    };

    let verifier = ContentVerifier::new(pattern.clone(), context_lines as u32);
    let mut file_matches = Vec::new();

    for file_id in candidates {
        if tombstones.contains(file_id) {
            continue;
        }

        let meta = match segment.get_metadata(file_id)? {
            Some(m) => m,
            None => continue,
        };

        let content = segment
            .content_reader()
            .read_content(meta.content_offset, meta.content_len)?;

        let line_matches = if context_lines > 0 {
            let blocks = verifier.verify_with_context(&content);
            if blocks.is_empty() {
                continue;
            }
            // Flatten ContextBlocks into LineMatches with context populated
            blocks
                .into_iter()
                .flat_map(|block| {
                    let before = block.before;
                    let after = block.after;
                    let match_count = block.matches.len();
                    block
                        .matches
                        .into_iter()
                        .enumerate()
                        .map(move |(i, m)| LineMatch {
                            context_before: if i == 0 { before.clone() } else { vec![] },
                            context_after: if i == match_count - 1 {
                                after.clone()
                            } else {
                                vec![]
                            },
                            ..m
                        })
                })
                .collect()
        } else {
            let matches = verifier.verify(&content);
            if matches.is_empty() {
                continue;
            }
            matches
        };

        // Compute relevance score using the weighted ranking system
        let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let match_type = match pattern {
            MatchPattern::Regex(_) => MatchType::Regex,
            _ => MatchType::Substring,
        };
        let pattern_query = match pattern {
            MatchPattern::Literal(s)
            | MatchPattern::LiteralCaseInsensitive(s)
            | MatchPattern::Regex(s) => s.as_str(),
        };
        let input = ScoringInput {
            path: &meta.path,
            query: pattern_query,
            match_type,
            match_count: total_match_ranges,
            line_count: meta.line_count,
            mtime_epoch_secs: meta.mtime_epoch_secs,
            now_epoch_secs: now,
        };
        let config = RankingConfig::default();
        let score = score_file_match(&input, &config);

        file_matches.push(FileMatch {
            file_id,
            path: PathBuf::from(&meta.path),
            language: meta.language,
            lines: line_matches,
            score,
        });
    }

    Ok(file_matches)
}

/// Search across multiple segments using a `MatchPattern`.
///
/// This is the pattern-aware version of `search_segments()`. It supports
/// literal, regex, and case-insensitive matching via `ContentVerifier`.
///
/// Behavior is identical to `search_segments()`: searches all segments,
/// filters tombstones, deduplicates by path (newest segment wins), and
/// sorts by relevance score.
pub fn search_segments_with_pattern(
    snapshot: &SegmentList,
    pattern: &MatchPattern,
) -> Result<SearchResult, IndexError> {
    let start = Instant::now();

    if snapshot.is_empty() {
        return Ok(SearchResult {
            total_match_count: 0,
            total_file_count: 0,
            files: Vec::new(),
            duration: start.elapsed(),
        });
    }

    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();

    for segment in snapshot.iter() {
        let tombstones = segment.load_tombstones()?;
        let file_matches = search_single_segment_with_pattern(segment, pattern, &tombstones, 0)?;

        for fm in file_matches {
            let seg_id = segment.segment_id();
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {}
                _ => {
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
        }
    }

    let mut files: Vec<FileMatch> = merged.into_values().map(|(_, fm)| fm).collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    let total_file_count = files.len();
    let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_match_count,
        total_file_count,
        files,
        duration: start.elapsed(),
    })
}

/// Search across multiple segments using a `MatchPattern` with options.
///
/// Combines pattern-aware matching (regex, case-insensitive) with
/// search options (context lines, max results).
pub fn search_segments_with_pattern_and_options(
    snapshot: &SegmentList,
    pattern: &MatchPattern,
    options: &SearchOptions,
) -> Result<SearchResult, IndexError> {
    let start = Instant::now();

    if snapshot.is_empty() {
        return Ok(SearchResult {
            total_match_count: 0,
            total_file_count: 0,
            files: Vec::new(),
            duration: start.elapsed(),
        });
    }

    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();

    for segment in snapshot.iter() {
        let tombstones = segment.load_tombstones()?;
        let file_matches = search_single_segment_with_pattern(
            segment,
            pattern,
            &tombstones,
            options.context_lines,
        )?;

        for fm in file_matches {
            let seg_id = segment.segment_id();
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {}
                _ => {
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
        }
    }

    let mut files: Vec<FileMatch> = merged.into_values().map(|(_, fm)| fm).collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    let total_file_count = files.len();
    let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();

    // Apply max_results limit
    if let Some(max) = options.max_results {
        files.truncate(max);
    }

    Ok(SearchResult {
        total_match_count,
        total_file_count,
        files,
        duration: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_state::SegmentList;
    use crate::segment::{InputFile, SegmentWriter};
    use crate::types::{FileId, SegmentId};
    use std::sync::Arc;

    // ---- Content verification tests ----

    #[test]
    fn test_verify_single_match() {
        let content = b"fn main() {\n    println!(\"hello\");\n}\n";
        let matches = verify_content_matches(content, "println", 0);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);
        assert!(matches[0].content.contains("println"));
        assert_eq!(matches[0].ranges.len(), 1);
    }

    #[test]
    fn test_verify_no_match() {
        let content = b"fn main() {}\n";
        let matches = verify_content_matches(content, "foobar", 0);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_verify_multiple_matches_same_line() {
        let content = b"let aa = aa + aa;\n";
        let matches = verify_content_matches(content, "aa", 0);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 1);
        // Should have 3 ranges: positions 4, 9, 14
        assert_eq!(matches[0].ranges.len(), 3);
    }

    #[test]
    fn test_verify_multiple_lines() {
        let content = b"fn foo() {}\nfn bar() {}\nfn baz() {}\n";
        let matches = verify_content_matches(content, "fn ", 0);
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].line_number, 1);
        assert_eq!(matches[1].line_number, 2);
        assert_eq!(matches[2].line_number, 3);
    }

    #[test]
    fn test_verify_empty_query() {
        let content = b"fn main() {}\n";
        let matches = verify_content_matches(content, "", 0);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_verify_empty_content() {
        let content = b"";
        let matches = verify_content_matches(content, "foo", 0);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_verify_no_trailing_newline() {
        let content = b"line one\nline two";
        let matches = verify_content_matches(content, "two", 0);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);
    }

    // ---- Context line tests ----

    #[test]
    fn test_verify_with_context_lines() {
        let content = b"line one\nline two\nline three\nline four\nline five\n";
        let matches = verify_content_matches(content, "three", 1);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 3);
        assert_eq!(matches[0].context_before.len(), 1);
        assert_eq!(matches[0].context_before[0].line_number, 2);
        assert_eq!(matches[0].context_before[0].content, "line two");
        assert_eq!(matches[0].context_after.len(), 1);
        assert_eq!(matches[0].context_after[0].line_number, 4);
        assert_eq!(matches[0].context_after[0].content, "line four");
    }

    #[test]
    fn test_verify_with_context_at_start() {
        let content = b"line one\nline two\nline three\n";
        let matches = verify_content_matches(content, "one", 2);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 1);
        // No context before first line
        assert_eq!(matches[0].context_before.len(), 0);
        assert_eq!(matches[0].context_after.len(), 2);
        assert_eq!(matches[0].context_after[0].line_number, 2);
        assert_eq!(matches[0].context_after[1].line_number, 3);
    }

    #[test]
    fn test_verify_with_context_at_end() {
        let content = b"line one\nline two\nline three\n";
        let matches = verify_content_matches(content, "three", 2);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 3);
        assert_eq!(matches[0].context_before.len(), 2);
        assert_eq!(matches[0].context_before[0].line_number, 1);
        assert_eq!(matches[0].context_before[1].line_number, 2);
        // No context after last line
        assert_eq!(matches[0].context_after.len(), 0);
    }

    #[test]
    fn test_verify_with_zero_context() {
        let content = b"line one\nline two\nline three\n";
        let matches = verify_content_matches(content, "two", 0);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].context_before.is_empty());
        assert!(matches[0].context_after.is_empty());
    }

    #[test]
    fn test_verify_context_adjacent_matches_no_overlap() {
        // When two matches are adjacent, context should not overlap
        let content = b"line 1\nmatch A\nmatch B\nline 4\n";
        let matches = verify_content_matches(content, "match", 1);
        assert_eq!(matches.len(), 2);
        // First match context_after should include "match B" (it's context even though it's also a match)
        assert_eq!(matches[0].context_after.len(), 1);
        assert_eq!(matches[0].context_after[0].line_number, 3);
        // Second match context_before should include "match A"
        assert_eq!(matches[1].context_before.len(), 1);
        assert_eq!(matches[1].context_before[0].line_number, 2);
    }

    // ---- Single-segment search tests ----

    /// Helper: build a segment with the given ID and files.
    fn build_segment(
        base_dir: &std::path::Path,
        segment_id: SegmentId,
        files: Vec<InputFile>,
    ) -> Arc<Segment> {
        let writer = SegmentWriter::new(base_dir, segment_id);
        Arc::new(writer.build(files).unwrap())
    }

    #[test]
    fn test_search_single_segment_basic() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "main.rs".to_string(),
                    content: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
                    mtime: 0,
                },
                InputFile {
                    path: "lib.rs".to_string(),
                    content: b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let tombstones = TombstoneSet::new();
        let results = search_single_segment_with_context(&seg, "println", &tombstones, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, PathBuf::from("main.rs"));
        assert_eq!(results[0].lines.len(), 1);
        assert_eq!(results[0].lines[0].line_number, 2);
    }

    #[test]
    fn test_search_single_segment_with_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "main.rs".to_string(),
                    content: b"fn main() { println!(\"hello\"); }\n".to_vec(),
                    mtime: 0,
                },
                InputFile {
                    path: "lib.rs".to_string(),
                    content: b"fn lib() { println!(\"world\"); }\n".to_vec(),
                    mtime: 0,
                },
            ],
        );

        // Tombstone file 0 (main.rs)
        let mut tombstones = TombstoneSet::new();
        tombstones.insert(FileId(0));

        let results = search_single_segment_with_context(&seg, "println", &tombstones, 0).unwrap();
        // Only lib.rs should appear (main.rs is tombstoned)
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, PathBuf::from("lib.rs"));
    }

    #[test]
    fn test_search_single_segment_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() {}\n".to_vec(),
                mtime: 0,
            }],
        );

        let tombstones = TombstoneSet::new();
        let results = search_single_segment_with_context(&seg, "foobar", &tombstones, 0).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_single_segment_short_query() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() {}\n".to_vec(),
                mtime: 0,
            }],
        );

        let tombstones = TombstoneSet::new();
        // Queries < 3 chars produce no trigrams, so no candidates
        let results = search_single_segment_with_context(&seg, "fn", &tombstones, 0).unwrap();
        assert!(results.is_empty());
    }

    // ---- Multi-segment search tests ----

    #[test]
    fn test_search_segments_single_segment() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
                mtime: 0,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let result = search_segments(&snapshot, "println").unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, PathBuf::from("main.rs"));
        assert_eq!(result.total_match_count, 1);
    }

    #[test]
    fn test_search_segments_multiple_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"hello\"); }\n".to_vec(),
                mtime: 0,
            }],
        );

        let seg1 = build_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "lib.rs".to_string(),
                content: b"pub fn lib() { println!(\"world\"); }\n".to_vec(),
                mtime: 0,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
        let result = search_segments(&snapshot, "println").unwrap();
        assert_eq!(result.files.len(), 2);
        // Both files should appear
        let paths: Vec<&str> = result
            .files
            .iter()
            .map(|f| f.path.to_str().unwrap())
            .collect();
        assert!(paths.contains(&"main.rs"));
        assert!(paths.contains(&"lib.rs"));
    }

    #[test]
    fn test_search_segments_dedup_prefers_newest() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Segment 0 has main.rs with "hello"
        let seg0 = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"hello\"); }\n".to_vec(),
                mtime: 100,
            }],
        );

        // Segment 1 has main.rs with updated content (same path, different content)
        let seg1 = build_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"goodbye world\"); }\n".to_vec(),
                mtime: 200,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
        let result = search_segments(&snapshot, "println").unwrap();

        // Should only have one result for main.rs (from newest segment)
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, PathBuf::from("main.rs"));
        // The content should be from seg1 (the newer one)
        assert!(result.files[0].lines[0].content.contains("goodbye"));
    }

    #[test]
    fn test_search_segments_dedup_with_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Segment 0 has main.rs
        let seg0 = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"hello\"); }\n".to_vec(),
                mtime: 100,
            }],
        );

        // Write tombstone for file 0 in segment 0
        let mut ts = TombstoneSet::new();
        ts.insert(FileId(0));
        ts.write_to(&seg0.dir_path().join("tombstones.bin"))
            .unwrap();

        // Segment 1 has the updated main.rs
        let seg1 = build_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"updated\"); }\n".to_vec(),
                mtime: 200,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
        let result = search_segments(&snapshot, "println").unwrap();

        // Only one result, from seg1
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].lines[0].content.contains("updated"));
    }

    #[test]
    fn test_search_segments_empty_snapshot() {
        let snapshot: SegmentList = Arc::new(vec![]);
        let result = search_segments(&snapshot, "println").unwrap();
        assert_eq!(result.files.len(), 0);
        assert_eq!(result.total_match_count, 0);
    }

    #[test]
    fn test_search_segments_case_insensitive_via_folded_index() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "main.rs".to_string(),
                    content: b"fn HttpRequest() {}".to_vec(),
                    mtime: 0,
                },
                InputFile {
                    path: "lib.rs".to_string(),
                    content: b"fn httprequest() {}".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);

        // Searching "httprequest" (lowercase) should find BOTH files
        let result = search_segments(&snapshot, "httprequest").unwrap();
        assert_eq!(
            result.files.len(),
            2,
            "both files should match via case-folded trigrams"
        );
    }

    #[test]
    fn test_search_segments_sorted_by_score() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // File with many matches (high score)
        let seg0 = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "many.rs".to_string(),
                content: b"fn foo() {}\nfn foo() {}\nfn foo() {}\n".to_vec(),
                mtime: 0,
            }],
        );

        // File with one match in many lines (low score)
        let seg1 = build_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "few.rs".to_string(),
                content: b"line 1\nline 2\nline 3\nline 4\nline 5\nfn foo() {}\nline 7\nline 8\nline 9\nline 10\n".to_vec(),
                mtime: 0,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
        let result = search_segments(&snapshot, "foo").unwrap();
        assert_eq!(result.files.len(), 2);
        // many.rs should come first (higher score)
        assert_eq!(result.files[0].path, PathBuf::from("many.rs"));
        assert!(result.files[0].score >= result.files[1].score);
    }

    // ---- Pattern-aware search tests ----

    #[test]
    fn test_search_segments_with_pattern_literal() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
                mtime: 0,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let pattern = MatchPattern::Literal("println".to_string());
        let result = search_segments_with_pattern(&snapshot, &pattern).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].lines[0].line_number, 2);
    }

    #[test]
    fn test_search_segments_with_pattern_regex() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() {}\nfn helper() {}\nlet x = 1;\n".to_vec(),
                mtime: 0,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let pattern = MatchPattern::Regex(r"fn\s+\w+".to_string());
        let result = search_segments_with_pattern(&snapshot, &pattern).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].lines.len(), 2); // main and helper
    }

    #[test]
    fn test_search_segments_with_pattern_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"Hello World\nhello world\nHELLO WORLD\n".to_vec(),
                mtime: 0,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let pattern = MatchPattern::LiteralCaseInsensitive("hello".to_string());
        let result = search_segments_with_pattern(&snapshot, &pattern).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].lines.len(), 3);
    }

    // ---- search_segments_with_options tests ----

    #[test]
    fn test_search_segments_with_context() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"use std::io;\n\nfn main() {\n    println!(\"hello\");\n}\n".to_vec(),
                mtime: 0,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let opts = SearchOptions {
            context_lines: 1,
            max_results: None,
        };
        let result = search_segments_with_options(&snapshot, "println", &opts).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].lines.len(), 1);

        let line = &result.files[0].lines[0];
        assert_eq!(line.line_number, 4);
        // Context before: line 3 "fn main() {"
        assert_eq!(line.context_before.len(), 1);
        assert_eq!(line.context_before[0].line_number, 3);
        assert!(line.context_before[0].content.contains("fn main()"));
        // Context after: line 5 "}"
        assert_eq!(line.context_after.len(), 1);
        assert_eq!(line.context_after[0].line_number, 5);
    }

    #[test]
    fn test_search_segments_uses_ranking() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // File where query matches the filename (should rank higher)
        let seg0 = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "src/search.rs".to_string(),
                content: b"fn search() {}\n".to_vec(),
                mtime: 1_700_000_000,
            }],
        );

        // File where query does NOT match filename (should rank lower)
        let seg1 = build_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "src/a/b/c/d/utils.rs".to_string(),
                content: b"fn search() {}\n".to_vec(),
                mtime: 1_600_000_000,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
        let result = search_segments(&snapshot, "search").unwrap();
        assert_eq!(result.files.len(), 2);
        // search.rs should rank first: filename match + shallower + more recent
        assert_eq!(
            result.files[0].path,
            PathBuf::from("src/search.rs"),
            "search.rs should rank first due to filename match + depth + recency"
        );
    }

    #[test]
    fn test_search_segments_default_no_context() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"line 1\nline 2\nfn main() {\n    println!(\"hello\");\n}\n".to_vec(),
                mtime: 0,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        // Original search_segments should produce no context
        let result = search_segments(&snapshot, "println").unwrap();
        assert_eq!(result.files[0].lines[0].context_before.len(), 0);
        assert_eq!(result.files[0].lines[0].context_after.len(), 0);
    }

    #[test]
    fn test_search_segments_tiebreaker_alphabetical() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Two files at same depth, same mtime, same match count
        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "src/beta.rs".to_string(),
                    content: b"fn search() {}\n".to_vec(),
                    mtime: 1_700_000_000,
                },
                InputFile {
                    path: "src/alpha.rs".to_string(),
                    content: b"fn search() {}\n".to_vec(),
                    mtime: 1_700_000_000,
                },
            ],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let result = search_segments(&snapshot, "search").unwrap();
        assert_eq!(result.files.len(), 2);
        // When scores are equal, should be sorted alphabetically
        assert_eq!(
            result.files[0].path,
            PathBuf::from("src/alpha.rs"),
            "alphabetical tiebreaker: alpha before beta"
        );
        assert_eq!(result.files[1].path, PathBuf::from("src/beta.rs"));
    }

    #[test]
    fn test_search_segments_with_pattern_and_options() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"line one\nline two\nfn hello() {}\nline four\nline five\n".to_vec(),
                mtime: 0,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let pattern = MatchPattern::LiteralCaseInsensitive("hello".to_string());
        let options = SearchOptions {
            context_lines: 1,
            max_results: None,
        };
        let result =
            search_segments_with_pattern_and_options(&snapshot, &pattern, &options).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].lines[0].line_number, 3);
        // Should have context
        assert!(!result.files[0].lines[0].context_before.is_empty());
        assert!(!result.files[0].lines[0].context_after.is_empty());
    }
}
