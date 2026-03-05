//! Multi-segment search with snapshot isolation.
//!
//! Provides [`search_segments()`] which executes a query across multiple segments,
//! filtering tombstoned entries, verifying matches in file content, deduplicating
//! across segments (preferring the newest), and returning a unified [`SearchResult`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;

use crate::error::IndexError;
use crate::index_state::SegmentList;
use crate::intersection::{find_candidates, intersect_two};
use crate::metadata::FileMetadata;
use crate::query::Query;
use crate::query_match::QueryMatcher;
use crate::query_trigrams::{TrigramQuery, extract_query_trigrams};
use crate::ranking::{MatchType, RankingConfig, ScoringInput, score_file_match};
use crate::search::{ContextLine, FileMatch, LineMatch, MatchPattern, SearchOptions, SearchResult};
use crate::segment::Segment;
use crate::tombstone::TombstoneSet;
use crate::types::{FileId, Language, SegmentId};
use crate::verify::ContentVerifier;

/// Minimum number of candidates before switching from sequential to parallel
/// verification. Below this threshold, the overhead of rayon's work-stealing
/// outweighs the benefit of parallelism.
const PAR_THRESHOLD: usize = 64;

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
    let finder = memchr::memmem::Finder::new(&folded_query);
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
            if let Some(pos) = finder.find(&folded_line[search_start..]) {
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
///
/// Segments are searched in parallel using rayon. Results are collected
/// per-segment, then merged in a single-threaded pass (newest segment wins
/// per path). A shared `AtomicUsize` budget provides approximate early
/// termination when `max_results` is set.
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

    // Shared budget for approximate early termination across segments
    let budget = AtomicUsize::new(options.max_results.unwrap_or(usize::MAX));

    // Search all segments in parallel, collecting tagged results
    let per_segment_results: Vec<Result<Vec<(SegmentId, FileMatch)>, IndexError>> = snapshot
        .par_iter()
        .map(|segment| {
            // Check budget before doing expensive work
            if budget.load(Ordering::Relaxed) == 0 {
                return Ok(Vec::new());
            }

            let tombstones = segment.load_tombstones()?;
            let remaining = budget.load(Ordering::Relaxed);
            let segment_budget = if options.max_results.is_some() {
                Some(remaining)
            } else {
                None
            };

            let file_matches = search_single_segment_with_context(
                segment,
                query,
                &tombstones,
                options.context_lines,
                segment_budget,
            )?;

            let seg_id = segment.segment_id();
            let tagged: Vec<(SegmentId, FileMatch)> = file_matches
                .into_iter()
                .filter_map(|fm| {
                    // Decrement global budget
                    if options.max_results.is_some() {
                        let prev = budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                            if b > 0 { Some(b - 1) } else { None }
                        });
                        if prev.is_err() {
                            return None;
                        }
                    }
                    Some((seg_id, fm))
                })
                .collect();

            Ok(tagged)
        })
        .collect();

    // Merge results: dedup by path, newest segment wins
    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();
    for result in per_segment_results {
        for (seg_id, fm) in result? {
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {}
                _ => {
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
        }
    }

    // Sort by score descending, then path for stability
    let mut files: Vec<FileMatch> = merged.into_values().map(|(_, fm)| fm).collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    // Trim to max_results (parallel search may slightly overshoot)
    if let Some(max) = options.max_results {
        files.truncate(max);
    }

    let total_file_count = files.len();
    let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_match_count,
        total_file_count,
        files,
        duration: start.elapsed(),
    })
}

/// Sort candidate file IDs by file size (ascending) for faster verification.
///
/// Smaller files verify faster (less data to decompress and scan) and are
/// more likely to be human-written source code. Combined with early
/// termination, this means the first N results come back much faster.
///
/// Uses a stable sort so equal-size files retain their original order.
/// Falls back to `u32::MAX` for any file ID whose size cannot be looked up
/// (pushes unknown entries to the end).
fn sort_candidates_by_size(segment: &Segment, candidates: Vec<FileId>) -> Vec<FileId> {
    if candidates.len() <= 1 {
        return candidates;
    }

    // Build a size lookup: for each candidate, read just size_bytes.
    // This is cheap -- O(1) per candidate via direct mmap indexing.
    let sizes: Vec<u32> = candidates
        .iter()
        .map(|fid| {
            segment
                .get_size_bytes(*fid)
                .ok()
                .flatten()
                .unwrap_or(u32::MAX)
        })
        .collect();

    // Sort candidates by their corresponding size (stable sort preserves
    // original order for equal sizes).
    let mut indices: Vec<usize> = (0..candidates.len()).collect();
    indices.sort_by_key(|&i| sizes[i]);

    indices.into_iter().map(|i| candidates[i]).collect()
}

/// Search a single segment with context line support.
fn search_single_segment_with_context(
    segment: &Segment,
    query: &str,
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
) -> Result<Vec<FileMatch>, IndexError> {
    let candidates = find_candidates(segment.trigram_reader(), query)?;
    let candidates = sort_candidates_by_size(segment, candidates);

    if candidates.len() < PAR_THRESHOLD {
        return search_single_segment_with_context_seq(
            segment,
            query,
            tombstones,
            context_lines,
            max_file_results,
            candidates,
        );
    }

    let now_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ranking_config = RankingConfig::default();
    let budget = AtomicUsize::new(max_file_results.unwrap_or(usize::MAX));

    let file_matches: Vec<FileMatch> = candidates
        .par_iter()
        .filter_map(|&file_id| {
            // Check budget before doing expensive work
            if budget.load(Ordering::Relaxed) == 0 {
                return None;
            }

            if tombstones.contains(file_id) {
                return None;
            }

            let meta = segment.get_metadata(file_id).ok()??;

            let content = segment
                .content_reader()
                .read_content_with_size_hint(
                    meta.content_offset,
                    meta.content_len,
                    meta.size_bytes as usize,
                )
                .ok()?;

            let line_matches = verify_content_matches(&content, query, context_lines);
            if line_matches.is_empty() {
                return None;
            }

            // Decrement budget atomically; if already 0, discard this result
            if budget
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                    if b > 0 { Some(b - 1) } else { None }
                })
                .is_err()
            {
                return None;
            }

            let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
            let input = ScoringInput {
                path: &meta.path,
                query,
                match_type: MatchType::Substring,
                match_count: total_match_ranges,
                line_count: meta.line_count,
                mtime_epoch_secs: meta.mtime_epoch_secs,
                now_epoch_secs,
            };
            let score = score_file_match(&input, &ranking_config);

            Some(FileMatch {
                file_id,
                path: PathBuf::from(&meta.path),
                language: meta.language,
                lines: line_matches,
                score,
            })
        })
        .collect();

    Ok(file_matches)
}

/// Sequential fallback for `search_single_segment_with_context` when the
/// candidate set is small enough that rayon overhead is not worthwhile.
fn search_single_segment_with_context_seq(
    segment: &Segment,
    query: &str,
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
    candidates: Vec<FileId>,
) -> Result<Vec<FileMatch>, IndexError> {
    let now_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ranking_config = RankingConfig::default();
    let mut file_matches = Vec::new();

    for file_id in candidates {
        if tombstones.contains(file_id) {
            continue;
        }

        let meta = match segment.get_metadata(file_id)? {
            Some(m) => m,
            None => continue,
        };

        let content = segment.content_reader().read_content_with_size_hint(
            meta.content_offset,
            meta.content_len,
            meta.size_bytes as usize,
        )?;

        let line_matches = verify_content_matches(&content, query, context_lines);
        if line_matches.is_empty() {
            continue;
        }

        let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
        let input = ScoringInput {
            path: &meta.path,
            query,
            match_type: MatchType::Substring,
            match_count: total_match_ranges,
            line_count: meta.line_count,
            mtime_epoch_secs: meta.mtime_epoch_secs,
            now_epoch_secs,
        };
        let score = score_file_match(&input, &ranking_config);

        file_matches.push(FileMatch {
            file_id,
            path: PathBuf::from(&meta.path),
            language: meta.language,
            lines: line_matches,
            score,
        });

        if let Some(max) = max_file_results
            && file_matches.len() >= max
        {
            break;
        }
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

/// Verify a single candidate file against a `MatchPattern`, returning a `FileMatch`
/// if the pattern matches. Shared by both sequential and parallel pattern search paths.
fn verify_candidate_with_pattern(
    segment: &Segment,
    file_id: FileId,
    pattern: &MatchPattern,
    verifier: &ContentVerifier,
    context_lines: usize,
    now_epoch_secs: u64,
    ranking_config: &RankingConfig,
) -> Option<FileMatch> {
    let meta = segment.get_metadata(file_id).ok()??;

    let content = segment
        .content_reader()
        .read_content_with_size_hint(
            meta.content_offset,
            meta.content_len,
            meta.size_bytes as usize,
        )
        .ok()?;

    let line_matches = if context_lines > 0 {
        let blocks = verifier.verify_with_context(&content);
        if blocks.is_empty() {
            return None;
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
            return None;
        }
        matches
    };

    let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
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
        now_epoch_secs,
    };
    let score = score_file_match(&input, ranking_config);

    Some(FileMatch {
        file_id,
        path: PathBuf::from(&meta.path),
        language: meta.language,
        lines: line_matches,
        score,
    })
}

/// Search a single segment using a `MatchPattern` for verification.
fn search_single_segment_with_pattern(
    segment: &Segment,
    pattern: &MatchPattern,
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
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
    let candidates = sort_candidates_by_size(segment, candidates);

    let verifier = ContentVerifier::new(pattern.clone(), context_lines as u32);

    if candidates.len() < PAR_THRESHOLD {
        return search_single_segment_with_pattern_seq(
            segment,
            pattern,
            &verifier,
            tombstones,
            context_lines,
            max_file_results,
            candidates,
        );
    }

    let now_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ranking_config = RankingConfig::default();
    let budget = AtomicUsize::new(max_file_results.unwrap_or(usize::MAX));

    let file_matches: Vec<FileMatch> = candidates
        .par_iter()
        .filter_map(|&file_id| {
            if budget.load(Ordering::Relaxed) == 0 {
                return None;
            }

            if tombstones.contains(file_id) {
                return None;
            }

            let file_match = verify_candidate_with_pattern(
                segment,
                file_id,
                pattern,
                &verifier,
                context_lines,
                now_epoch_secs,
                &ranking_config,
            )?;

            // Decrement budget atomically; if already 0, discard this result
            if budget
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                    if b > 0 { Some(b - 1) } else { None }
                })
                .is_err()
            {
                return None;
            }

            Some(file_match)
        })
        .collect();

    Ok(file_matches)
}

/// Sequential fallback for `search_single_segment_with_pattern` when the
/// candidate set is small enough that rayon overhead is not worthwhile.
fn search_single_segment_with_pattern_seq(
    segment: &Segment,
    pattern: &MatchPattern,
    verifier: &ContentVerifier,
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
    candidates: Vec<FileId>,
) -> Result<Vec<FileMatch>, IndexError> {
    let now_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ranking_config = RankingConfig::default();
    let mut file_matches = Vec::new();

    for file_id in candidates {
        if tombstones.contains(file_id) {
            continue;
        }

        if let Some(file_match) = verify_candidate_with_pattern(
            segment,
            file_id,
            pattern,
            verifier,
            context_lines,
            now_epoch_secs,
            &ranking_config,
        ) {
            file_matches.push(file_match);

            if let Some(max) = max_file_results
                && file_matches.len() >= max
            {
                break;
            }
        }
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
    search_segments_with_pattern_and_options(snapshot, pattern, &SearchOptions::default())
}

/// Search across multiple segments using a `MatchPattern` with options.
///
/// Combines pattern-aware matching (regex, case-insensitive) with
/// search options (context lines, max results).
///
/// Segments are searched in parallel using rayon. Results are collected
/// per-segment, then merged in a single-threaded pass (newest segment wins
/// per path). A shared `AtomicUsize` budget provides approximate early
/// termination when `max_results` is set.
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

    // Shared budget for approximate early termination across segments
    let budget = AtomicUsize::new(options.max_results.unwrap_or(usize::MAX));

    // Search all segments in parallel
    let per_segment_results: Vec<Result<Vec<(SegmentId, FileMatch)>, IndexError>> = snapshot
        .par_iter()
        .map(|segment| {
            if budget.load(Ordering::Relaxed) == 0 {
                return Ok(Vec::new());
            }

            let tombstones = segment.load_tombstones()?;
            let remaining = budget.load(Ordering::Relaxed);
            let segment_budget = if options.max_results.is_some() {
                Some(remaining)
            } else {
                None
            };

            let file_matches = search_single_segment_with_pattern(
                segment,
                pattern,
                &tombstones,
                options.context_lines,
                segment_budget,
            )?;

            let seg_id = segment.segment_id();
            let tagged: Vec<(SegmentId, FileMatch)> = file_matches
                .into_iter()
                .filter_map(|fm| {
                    if options.max_results.is_some() {
                        let prev = budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                            if b > 0 { Some(b - 1) } else { None }
                        });
                        if prev.is_err() {
                            return None;
                        }
                    }
                    Some((seg_id, fm))
                })
                .collect();

            Ok(tagged)
        })
        .collect();

    // Merge: dedup by path, newest segment wins
    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();
    for result in per_segment_results {
        for (seg_id, fm) in result? {
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

    // Trim to max_results (parallel search may slightly overshoot)
    if let Some(max) = options.max_results {
        files.truncate(max);
    }

    let total_file_count = files.len();
    let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_match_count,
        total_file_count,
        files,
        duration: start.elapsed(),
    })
}

/// Stream search results through a channel as they're found.
///
/// Unlike [`search_segments_with_pattern_and_options()`], this function sends
/// each `FileMatch` through the channel immediately after verification,
/// enabling consumers to display results incrementally.
///
/// Results arrive in segment-order (newest segment first), not sorted by
/// relevance score. The caller is responsible for any post-hoc ordering.
///
/// Cancellation: if the receiving end of the channel is dropped (e.g., fzf
/// exits), the search loop terminates early and returns `Ok(())`.
///
/// Deduplication: segments are processed newest-first. If a file path was
/// already sent from a newer segment, it is skipped in older segments.
pub fn search_segments_streaming(
    snapshot: &SegmentList,
    pattern: &MatchPattern,
    options: &SearchOptions,
    sender: mpsc::Sender<FileMatch>,
) -> Result<(), IndexError> {
    if snapshot.is_empty() {
        return Ok(());
    }

    let mut sent_paths: HashSet<PathBuf> = HashSet::new();
    let mut sent_count: usize = 0;

    // Process segments in reverse order (newest first) for dedup correctness
    for segment in snapshot.iter().rev() {
        let tombstones = segment.load_tombstones()?;
        let file_matches = search_single_segment_with_pattern(
            segment,
            pattern,
            &tombstones,
            options.context_lines,
            None, // no per-segment limit; we limit globally via sent_count
        )?;

        for fm in file_matches {
            // Dedup: skip if already sent from a newer segment
            if sent_paths.contains(&fm.path) {
                continue;
            }

            sent_paths.insert(fm.path.clone());

            // Send the match; if receiver dropped, stop searching
            if sender.send(fm).is_err() {
                return Ok(());
            }

            sent_count += 1;
            if let Some(max) = options.max_results
                && sent_count >= max
            {
                return Ok(());
            }
        }
    }

    Ok(())
}

// ── Query AST-based search pipeline ──

/// Extract metadata-level pre-filters from a Query AST.
///
/// Recursively walks the AST and collects all `LanguageFilter` and `PathFilter`
/// nodes. Returns `(languages, path_prefixes)` for use in pre-filtering
/// candidates before content verification.
fn extract_pre_filters(query: &Query) -> (Vec<Language>, Vec<String>) {
    let mut languages = Vec::new();
    let mut path_prefixes = Vec::new();
    collect_pre_filters(query, &mut languages, &mut path_prefixes);
    // Dedup languages (Language doesn't impl Ord, so use a set-style dedup)
    let mut seen_langs = Vec::new();
    for lang in languages {
        if !seen_langs.contains(&lang) {
            seen_langs.push(lang);
        }
    }
    path_prefixes.sort();
    path_prefixes.dedup();
    (seen_langs, path_prefixes)
}

/// Recursively collect language and path filters from the query AST.
fn collect_pre_filters(
    query: &Query,
    languages: &mut Vec<Language>,
    path_prefixes: &mut Vec<String>,
) {
    match query {
        Query::LanguageFilter(lang) => languages.push(*lang),
        Query::PathFilter(prefix) => path_prefixes.push(prefix.clone()),
        Query::And(children) => {
            for child in children {
                collect_pre_filters(child, languages, path_prefixes);
            }
        }
        Query::Or(left, right) => {
            collect_pre_filters(left, languages, path_prefixes);
            collect_pre_filters(right, languages, path_prefixes);
        }
        Query::Not(inner) => {
            collect_pre_filters(inner, languages, path_prefixes);
        }
        Query::Literal(_) | Query::Phrase(_) | Query::Regex(_) => {}
    }
}

/// Check whether a file's metadata passes the pre-filters.
///
/// If `languages` is non-empty, the file's language must be in the list.
/// If `path_prefixes` is non-empty, the file's path must start with at least
/// one prefix.
fn passes_pre_filters(
    meta: &FileMetadata,
    languages: &[Language],
    path_prefixes: &[String],
) -> bool {
    if !languages.is_empty() && !languages.contains(&meta.language) {
        return false;
    }
    if !path_prefixes.is_empty() && !path_prefixes.iter().any(|p| meta.path.starts_with(p)) {
        return false;
    }
    true
}

/// Look up trigram candidates from a segment using the pre-extracted TrigramQuery.
///
/// For `All` queries, trigrams are sorted by estimated posting list size
/// (smallest first) and intersected incrementally. If any trigram has zero
/// postings or an intermediate intersection becomes empty, remaining lists
/// are skipped entirely.
///
/// For `Any` queries, each branch is processed the same way and results
/// are unioned.
fn lookup_trigram_candidates(
    segment: &Segment,
    tq: &TrigramQuery,
) -> Result<Vec<FileId>, IndexError> {
    match tq {
        TrigramQuery::All(trigrams) => {
            if trigrams.is_empty() {
                return segment.all_file_ids();
            }
            intersect_trigrams_smallest_first(segment, trigrams)
        }
        TrigramQuery::Any(branches) => {
            let mut all_candidates: Vec<FileId> = Vec::new();
            for branch in branches {
                let candidates = intersect_trigrams_smallest_first(segment, branch)?;
                all_candidates.extend(candidates);
            }
            all_candidates.sort();
            all_candidates.dedup();
            Ok(all_candidates)
        }
        TrigramQuery::None => segment.all_file_ids(),
    }
}

/// Sort trigrams by estimated posting list size, then decode and intersect
/// incrementally with early termination.
fn intersect_trigrams_smallest_first(
    segment: &Segment,
    trigrams: &std::collections::HashSet<crate::types::Trigram>,
) -> Result<Vec<FileId>, IndexError> {
    if trigrams.is_empty() {
        return segment.all_file_ids();
    }

    let reader = segment.trigram_reader();

    // Score each trigram by estimated posting list byte size (cheap: O(log n) per trigram)
    let mut scored: Vec<_> = trigrams
        .iter()
        .map(|t| (*t, reader.estimate_posting_list_size(*t)))
        .collect();

    // Sort smallest first — smallest lists narrow the intersection fastest
    scored.sort_by_key(|&(_, est)| est);

    // Short-circuit: if the smallest trigram has zero postings, no files can match
    if scored[0].1 == 0 {
        return Ok(Vec::new());
    }

    // Decode the smallest list first
    let mut result = reader.lookup_file_ids(scored[0].0)?;
    if result.is_empty() {
        return Ok(Vec::new());
    }

    // Intersect incrementally, bailing as soon as the result is empty
    for &(trigram, _) in &scored[1..] {
        let list = reader.lookup_file_ids(trigram)?;
        result = intersect_two(&result, &list);
        if result.is_empty() {
            return Ok(Vec::new());
        }
    }

    Ok(result)
}

/// Search across multiple segments using a parsed `Query` AST.
///
/// This is the full search pipeline entry point for structured queries:
///
/// 1. Extract trigrams from the Query AST for candidate filtering
/// 2. Extract metadata pre-filters (language, path) from the AST
/// 3. For each segment (in parallel): look up candidates, apply pre-filters,
///    verify content with `QueryMatcher`, score results
/// 4. Deduplicate across segments (newest segment wins per path)
/// 5. Sort by relevance score and return `SearchResult`
///
/// Follows the same parallel search + merge + dedup pattern as
/// [`search_segments_with_pattern_and_options()`].
pub fn search_segments_with_query(
    snapshot: &SegmentList,
    query: &Query,
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

    let tq = extract_query_trigrams(query);
    let (languages, path_prefixes) = extract_pre_filters(query);

    // Shared budget for approximate early termination across segments
    let budget = AtomicUsize::new(options.max_results.unwrap_or(usize::MAX));

    // Search all segments in parallel
    let per_segment_results: Vec<Result<Vec<(SegmentId, FileMatch)>, IndexError>> = snapshot
        .par_iter()
        .map(|segment| {
            if budget.load(Ordering::Relaxed) == 0 {
                return Ok(Vec::new());
            }

            let tombstones = segment.load_tombstones()?;
            let remaining = budget.load(Ordering::Relaxed);
            let segment_budget = if options.max_results.is_some() {
                Some(remaining)
            } else {
                None
            };

            let file_matches = search_single_segment_with_query(
                segment,
                query,
                &tq,
                &languages,
                &path_prefixes,
                &tombstones,
                options.context_lines,
                segment_budget,
            )?;

            let seg_id = segment.segment_id();
            let tagged: Vec<(SegmentId, FileMatch)> = file_matches
                .into_iter()
                .filter_map(|fm| {
                    if options.max_results.is_some() {
                        let prev = budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                            if b > 0 { Some(b - 1) } else { None }
                        });
                        if prev.is_err() {
                            return None;
                        }
                    }
                    Some((seg_id, fm))
                })
                .collect();

            Ok(tagged)
        })
        .collect();

    // Merge: dedup by path, newest segment wins
    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();
    for result in per_segment_results {
        for (seg_id, fm) in result? {
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

    // Trim to max_results (parallel search may slightly overshoot)
    if let Some(max) = options.max_results {
        files.truncate(max);
    }

    let total_file_count = files.len();
    let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_match_count,
        total_file_count,
        files,
        duration: start.elapsed(),
    })
}

/// Streaming variant of [`search_segments_with_query()`].
///
/// Sends `FileMatch` results through `sender` as they are found, processing
/// segments newest-first for dedup correctness. This enables incremental output
/// (critical for fzf integration) while using the full Query AST pipeline.
///
/// Trade-offs vs the batch variant:
/// - Results arrive in segment order (newest first), not sorted by relevance score
/// - Dedup uses HashSet (like `search_segments_streaming`) instead of HashMap merge
/// - Within each segment, candidate verification is still parallelized via rayon
pub fn search_segments_with_query_streaming(
    snapshot: &SegmentList,
    query: &Query,
    options: &SearchOptions,
    sender: mpsc::Sender<FileMatch>,
) -> Result<(), IndexError> {
    if snapshot.is_empty() {
        return Ok(());
    }

    let tq = extract_query_trigrams(query);
    let (languages, path_prefixes) = extract_pre_filters(query);

    let mut sent_paths: HashSet<PathBuf> = HashSet::new();
    let mut sent_count: usize = 0;

    // Process segments in reverse order (newest first) for dedup correctness
    for segment in snapshot.iter().rev() {
        let tombstones = segment.load_tombstones()?;
        let file_matches = search_single_segment_with_query(
            segment,
            query,
            &tq,
            &languages,
            &path_prefixes,
            &tombstones,
            options.context_lines,
            None, // no per-segment limit; limit globally via sent_count
        )?;

        for fm in file_matches {
            if sent_paths.contains(&fm.path) {
                continue;
            }

            sent_paths.insert(fm.path.clone());

            if sender.send(fm).is_err() {
                return Ok(()); // receiver dropped, stop searching
            }

            sent_count += 1;
            if let Some(max) = options.max_results
                && sent_count >= max
            {
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Search a single segment using a Query AST.
///
/// Looks up trigram candidates, applies pre-filters, and verifies content
/// using `QueryMatcher`.
#[allow(clippy::too_many_arguments)]
fn search_single_segment_with_query(
    segment: &Segment,
    query: &Query,
    tq: &TrigramQuery,
    languages: &[Language],
    path_prefixes: &[String],
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
) -> Result<Vec<FileMatch>, IndexError> {
    let candidates = lookup_trigram_candidates(segment, tq)?;
    let candidates = sort_candidates_by_size(segment, candidates);

    let matcher = QueryMatcher::new(query, context_lines as u32);
    let now_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ranking_config = RankingConfig::default();
    let budget = AtomicUsize::new(max_file_results.unwrap_or(usize::MAX));

    let verify_candidate = |&file_id: &FileId| -> Option<FileMatch> {
        if budget.load(Ordering::Relaxed) == 0 {
            return None;
        }

        if tombstones.contains(file_id) {
            return None;
        }

        let meta = segment.get_metadata(file_id).ok()??;

        if !passes_pre_filters(&meta, languages, path_prefixes) {
            return None;
        }

        let content = segment
            .content_reader()
            .read_content_with_size_hint(
                meta.content_offset,
                meta.content_len,
                meta.size_bytes as usize,
            )
            .ok()?;

        let line_matches = matcher.matches(&content)?;

        // Decrement budget atomically; if already 0, discard this result
        if budget
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                if b > 0 { Some(b - 1) } else { None }
            })
            .is_err()
        {
            return None;
        }

        let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
        let input = ScoringInput {
            path: &meta.path,
            query: "",
            match_type: MatchType::Substring,
            match_count: total_match_ranges.max(1),
            line_count: meta.line_count,
            mtime_epoch_secs: meta.mtime_epoch_secs,
            now_epoch_secs,
        };
        let score = score_file_match(&input, &ranking_config);

        Some(FileMatch {
            file_id,
            path: PathBuf::from(&meta.path),
            language: meta.language,
            lines: line_matches,
            score,
        })
    };

    let file_matches: Vec<FileMatch> = if candidates.len() < PAR_THRESHOLD {
        candidates.iter().filter_map(verify_candidate).collect()
    } else {
        candidates.par_iter().filter_map(verify_candidate).collect()
    };

    Ok(file_matches)
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let results =
            search_single_segment_with_context(&seg, "println", &tombstones, 0, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, PathBuf::from("main.rs"));
        assert_eq!(results[0].lines.len(), 1);
        assert_eq!(results[0].lines[0].line_number, 2);
    }

    #[test]
    fn test_search_single_segment_with_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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

        let results =
            search_single_segment_with_context(&seg, "println", &tombstones, 0, None).unwrap();
        // Only lib.rs should appear (main.rs is tombstoned)
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, PathBuf::from("lib.rs"));
    }

    #[test]
    fn test_search_single_segment_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let results =
            search_single_segment_with_context(&seg, "foobar", &tombstones, 0, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_single_segment_short_query() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let results = search_single_segment_with_context(&seg, "fn", &tombstones, 0, None).unwrap();
        assert!(results.is_empty());
    }

    // ---- Multi-segment search tests ----

    #[test]
    fn test_search_segments_single_segment() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
        let base_dir = dir.path().join(".ferret_index/segments");
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
    fn test_search_single_segment_early_termination() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Build a segment with 5 files, all containing "println"
        let files: Vec<InputFile> = (0..5)
            .map(|i| InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 0,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let tombstones = TombstoneSet::new();

        // Without limit: should find all 5
        let all =
            search_single_segment_with_context(&seg, "println", &tombstones, 0, None).unwrap();
        assert_eq!(all.len(), 5);

        // With limit=2: should find exactly 2
        let limited =
            search_single_segment_with_context(&seg, "println", &tombstones, 0, Some(2)).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn test_search_segments_with_pattern_and_options() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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

    #[test]
    fn test_search_single_segment_pattern_early_termination() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files: Vec<InputFile> = (0..5)
            .map(|i| InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 0,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let tombstones = TombstoneSet::new();
        let pattern = MatchPattern::Literal("println".to_string());

        // Without limit
        let all = search_single_segment_with_pattern(&seg, &pattern, &tombstones, 0, None).unwrap();
        assert_eq!(all.len(), 5);

        // With limit=3
        let limited =
            search_single_segment_with_pattern(&seg, &pattern, &tombstones, 0, Some(3)).unwrap();
        assert_eq!(limited.len(), 3);
    }

    #[test]
    fn test_search_segments_with_pattern_and_options_early_termination() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            (0..5)
                .map(|i| InputFile {
                    path: format!("file_{i}.rs"),
                    content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                    mtime: 0,
                })
                .collect(),
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let pattern = MatchPattern::Literal("println".to_string());

        // Without limit
        let all = search_segments_with_pattern_and_options(
            &snapshot,
            &pattern,
            &SearchOptions {
                context_lines: 0,
                max_results: None,
            },
        )
        .unwrap();
        assert_eq!(all.total_file_count, 5);

        // With limit=2
        let limited = search_segments_with_pattern_and_options(
            &snapshot,
            &pattern,
            &SearchOptions {
                context_lines: 0,
                max_results: Some(2),
            },
        )
        .unwrap();
        assert_eq!(limited.files.len(), 2);
        assert_eq!(limited.total_file_count, 2);
    }

    #[test]
    fn test_search_segments_with_options_max_results_early_termination() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Segment 0: 3 files with "println"
        let seg0 = build_segment(
            &base_dir,
            SegmentId(0),
            (0..3)
                .map(|i| InputFile {
                    path: format!("a/file_{i}.rs"),
                    content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                    mtime: 0,
                })
                .collect(),
        );

        // Segment 1: 3 more files with "println" (different paths)
        let seg1 = build_segment(
            &base_dir,
            SegmentId(1),
            (0..3)
                .map(|i| InputFile {
                    path: format!("b/file_{i}.rs"),
                    content: format!("fn g{i}() {{ println!(\"world\"); }}\n").into_bytes(),
                    mtime: 0,
                })
                .collect(),
        );

        let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);

        // Without limit: all 6
        let all = search_segments_with_options(
            &snapshot,
            "println",
            &SearchOptions {
                context_lines: 0,
                max_results: None,
            },
        )
        .unwrap();
        assert_eq!(all.total_file_count, 6);

        // With limit=2: exactly 2 files returned
        let limited = search_segments_with_options(
            &snapshot,
            "println",
            &SearchOptions {
                context_lines: 0,
                max_results: Some(2),
            },
        )
        .unwrap();
        assert_eq!(limited.files.len(), 2);
        assert_eq!(limited.total_file_count, 2);
    }

    // ---- Candidate ordering tests ----

    #[test]
    fn test_sort_candidates_by_size() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Create files of varying sizes; IDs assigned in order: 0=medium, 1=small, 2=large
        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "medium.rs".to_string(),
                    content: vec![b'x'; 500],
                    mtime: 0,
                },
                InputFile {
                    path: "small.rs".to_string(),
                    content: vec![b'x'; 100],
                    mtime: 0,
                },
                InputFile {
                    path: "large.rs".to_string(),
                    content: vec![b'x'; 2000],
                    mtime: 0,
                },
            ],
        );

        let candidates = vec![FileId(0), FileId(1), FileId(2)];
        let sorted = sort_candidates_by_size(&seg, candidates);

        // Should be ordered: small(1, 100B), medium(0, 500B), large(2, 2000B)
        assert_eq!(sorted, vec![FileId(1), FileId(0), FileId(2)]);
    }

    #[test]
    fn test_sort_candidates_by_size_preserves_order_for_equal_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Two files with the same size
        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "a.rs".to_string(),
                    content: vec![b'x'; 200],
                    mtime: 0,
                },
                InputFile {
                    path: "b.rs".to_string(),
                    content: vec![b'x'; 200],
                    mtime: 0,
                },
            ],
        );

        let candidates = vec![FileId(0), FileId(1)];
        let sorted = sort_candidates_by_size(&seg, candidates);

        // Stable sort: original order preserved for equal sizes
        assert_eq!(sorted, vec![FileId(0), FileId(1)]);
    }

    #[test]
    fn test_sort_candidates_by_size_empty() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "a.rs".to_string(),
                content: b"fn a() {}".to_vec(),
                mtime: 0,
            }],
        );

        let sorted = sort_candidates_by_size(&seg, vec![]);
        assert!(sorted.is_empty());
    }

    #[test]
    fn test_search_single_segment_prefers_smaller_files() {
        // Verify that with early termination (max_file_results=1), the smaller file
        // is returned first (proving candidates were sorted by size).
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // File 0 is large (5000+ bytes), file 1 is small (~25 bytes).
        // Both contain the query "fn main".
        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "large.rs".to_string(),
                    content: {
                        let mut c = b"fn main() { /* large */ }".to_vec();
                        c.extend(vec![b' '; 5000]);
                        c
                    },
                    mtime: 0,
                },
                InputFile {
                    path: "small.rs".to_string(),
                    content: b"fn main() { /* small */ }".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let tombstones = TombstoneSet::new();
        // Request only 1 result -- with ordering, the small file should be returned
        let results =
            search_single_segment_with_context(&seg, "fn main", &tombstones, 0, Some(1)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].path,
            PathBuf::from("small.rs"),
            "smaller file should be verified first and returned under early termination"
        );
    }

    #[test]
    fn test_search_single_segment_with_pattern_prefers_smaller_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "large.rs".to_string(),
                    content: {
                        let mut c = b"fn main() { /* large */ }".to_vec();
                        c.extend(vec![b' '; 5000]);
                        c
                    },
                    mtime: 0,
                },
                InputFile {
                    path: "small.rs".to_string(),
                    content: b"fn main() { /* small */ }".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let tombstones = TombstoneSet::new();
        let pattern = MatchPattern::Literal("fn main".to_string());
        let results =
            search_single_segment_with_pattern(&seg, &pattern, &tombstones, 0, Some(1)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].path,
            PathBuf::from("small.rs"),
            "smaller file should be verified first with pattern search too"
        );
    }

    // ---- Parallel search tests (candidate count > PAR_THRESHOLD) ----

    #[test]
    fn test_parallel_search_many_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Build a segment with 200 files, all containing "println"
        // This exceeds PAR_THRESHOLD (64), so the parallel path is exercised.
        let files: Vec<InputFile> = (0..200)
            .map(|i| InputFile {
                path: format!("file_{i:03}.rs"),
                content: format!("fn func_{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 1700000000 + i as u64,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let snapshot: SegmentList = Arc::new(vec![seg]);
        let result = search_segments(&snapshot, "println").unwrap();
        assert_eq!(result.total_file_count, 200);
    }

    #[test]
    fn test_parallel_search_with_budget() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files: Vec<InputFile> = (0..200)
            .map(|i| InputFile {
                path: format!("file_{i:03}.rs"),
                content: format!("fn func_{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 1700000000 + i as u64,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let snapshot: SegmentList = Arc::new(vec![seg]);
        let options = SearchOptions {
            context_lines: 0,
            max_results: Some(10),
        };
        let result = search_segments_with_options(&snapshot, "println", &options).unwrap();
        assert!(result.total_file_count <= 10);
        assert!(result.total_file_count >= 1);
    }

    #[test]
    fn test_parallel_search_pattern_many_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files: Vec<InputFile> = (0..200)
            .map(|i| InputFile {
                path: format!("file_{i:03}.rs"),
                content: format!("fn func_{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 1700000000 + i as u64,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let snapshot: SegmentList = Arc::new(vec![seg]);

        let pattern = MatchPattern::Literal("println".to_string());
        let result = search_segments_with_pattern(&snapshot, &pattern).unwrap();
        assert_eq!(result.total_file_count, 200);
    }

    #[test]
    fn test_parallel_search_pattern_with_budget() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files: Vec<InputFile> = (0..200)
            .map(|i| InputFile {
                path: format!("file_{i:03}.rs"),
                content: format!("fn func_{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 1700000000 + i as u64,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let snapshot: SegmentList = Arc::new(vec![seg]);

        let pattern = MatchPattern::Literal("println".to_string());
        let options = SearchOptions {
            context_lines: 0,
            max_results: Some(10),
        };
        let result =
            search_segments_with_pattern_and_options(&snapshot, &pattern, &options).unwrap();
        assert!(result.total_file_count <= 10);
        assert!(result.total_file_count >= 1);
    }

    #[test]
    fn test_parallel_search_with_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files: Vec<InputFile> = (0..200)
            .map(|i| InputFile {
                path: format!("file_{i:03}.rs"),
                content: format!("fn func_{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 1700000000 + i as u64,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);

        // Tombstone even-numbered files
        let mut ts = TombstoneSet::new();
        for i in (0..200u32).step_by(2) {
            ts.insert(FileId(i));
        }
        ts.write_to(&seg.dir_path().join("tombstones.bin")).unwrap();

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let result = search_segments(&snapshot, "println").unwrap();
        // Only odd-numbered files should remain (100 files)
        assert_eq!(result.total_file_count, 100);
    }

    #[test]
    fn test_parallel_search_regex_many_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files: Vec<InputFile> = (0..200)
            .map(|i| InputFile {
                path: format!("file_{i:03}.rs"),
                content: format!("fn func_{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 1700000000 + i as u64,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let snapshot: SegmentList = Arc::new(vec![seg]);

        let pattern = MatchPattern::Regex(r"println!\(.*\)".to_string());
        let result = search_segments_with_pattern(&snapshot, &pattern).unwrap();
        assert_eq!(result.total_file_count, 200);
    }

    // ---- Streaming search tests ----

    #[test]
    fn test_search_segments_streaming_basic() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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
                    content: b"pub fn lib() { println!(\"world\"); }\n".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
        let options = SearchOptions::default();

        let (tx, rx) = mpsc::channel();
        let result = search_segments_streaming(&snapshot, &pattern, &options, tx);
        assert!(result.is_ok());

        let matches: Vec<FileMatch> = rx.into_iter().collect();
        assert_eq!(matches.len(), 2);
        let paths: Vec<String> = matches
            .iter()
            .map(|m| m.path.to_string_lossy().to_string())
            .collect();
        assert!(paths.contains(&"main.rs".to_string()));
        assert!(paths.contains(&"lib.rs".to_string()));
    }

    #[test]
    fn test_search_segments_streaming_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files: Vec<InputFile> = (0..10)
            .map(|i| InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 0,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let snapshot: SegmentList = Arc::new(vec![seg]);
        let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
        let options = SearchOptions::default();

        let (tx, rx) = mpsc::channel();

        let handle = std::thread::spawn(move || {
            let first = rx.recv().unwrap();
            drop(rx);
            first
        });

        let result = search_segments_streaming(&snapshot, &pattern, &options, tx);
        assert!(result.is_ok());

        let first_match = handle.join().unwrap();
        assert!(!first_match.lines.is_empty());
    }

    #[test]
    fn test_search_segments_streaming_dedup_newest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"old version\"); }\n".to_vec(),
                mtime: 100,
            }],
        );

        let seg1 = build_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"new version\"); }\n".to_vec(),
                mtime: 200,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
        let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
        let options = SearchOptions::default();

        let (tx, rx) = mpsc::channel();
        search_segments_streaming(&snapshot, &pattern, &options, tx).unwrap();

        let matches: Vec<FileMatch> = rx.into_iter().collect();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].lines[0].content.contains("new version"));
    }

    #[test]
    fn test_search_segments_streaming_max_results() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files: Vec<InputFile> = (0..10)
            .map(|i| InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 0,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let snapshot: SegmentList = Arc::new(vec![seg]);
        let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
        let options = SearchOptions {
            context_lines: 0,
            max_results: Some(3),
        };

        let (tx, rx) = mpsc::channel();
        search_segments_streaming(&snapshot, &pattern, &options, tx).unwrap();

        let matches: Vec<FileMatch> = rx.into_iter().collect();
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn test_search_segments_streaming_empty_snapshot() {
        let snapshot: SegmentList = Arc::new(vec![]);
        let pattern = MatchPattern::LiteralCaseInsensitive("println".to_string());
        let options = SearchOptions::default();

        let (tx, rx) = mpsc::channel();
        search_segments_streaming(&snapshot, &pattern, &options, tx).unwrap();

        let matches: Vec<FileMatch> = rx.into_iter().collect();
        assert!(matches.is_empty());
    }

    // ---- Query AST-based search tests ----

    #[test]
    fn test_search_with_query_literal() {
        use crate::query::parse_query;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let query = parse_query("println").unwrap();
        let options = SearchOptions::default();
        let result = search_segments_with_query(&snapshot, &query, &options).unwrap();

        assert_eq!(result.total_file_count, 1);
        assert_eq!(result.files[0].path, PathBuf::from("main.rs"));
        assert_eq!(result.files[0].lines.len(), 1);
        assert!(result.files[0].lines[0].content.contains("println"));
    }

    #[test]
    fn test_search_with_query_and() {
        use crate::query::parse_query;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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
                    content: b"fn helper() {\n    println!(\"world\");\n}\n".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        // Implicit AND: "println main" means files must contain both "println" AND "main"
        let query = parse_query("println main").unwrap();
        let options = SearchOptions::default();
        let result = search_segments_with_query(&snapshot, &query, &options).unwrap();

        // Only main.rs contains both "println" and "main"
        assert_eq!(result.total_file_count, 1);
        assert_eq!(result.files[0].path, PathBuf::from("main.rs"));
    }

    #[test]
    fn test_search_with_query_language_filter() {
        use crate::query::parse_query;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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
                    path: "script.py".to_string(),
                    content: b"def main():\n    println(\"hello\")\n".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        // "language:rust println" should only match .rs files
        let query = parse_query("language:rust println").unwrap();
        let options = SearchOptions::default();
        let result = search_segments_with_query(&snapshot, &query, &options).unwrap();

        assert_eq!(result.total_file_count, 1);
        assert_eq!(result.files[0].path, PathBuf::from("main.rs"));
    }

    // ---- Streaming query search tests ----

    #[test]
    fn test_search_with_query_streaming_literal() {
        use crate::query::parse_query;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let query = parse_query("println").unwrap();
        let options = SearchOptions::default();

        let (tx, rx) = mpsc::channel();
        search_segments_with_query_streaming(&snapshot, &query, &options, tx).unwrap();

        let matches: Vec<FileMatch> = rx.into_iter().collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, PathBuf::from("main.rs"));
        assert!(matches[0].lines[0].content.contains("println"));
    }

    #[test]
    fn test_search_with_query_streaming_and() {
        use crate::query::parse_query;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg = build_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "both.rs".to_string(),
                    content: b"fn process() -> Result<(), Box<dyn Error>> { Ok(()) }\n".to_vec(),
                    mtime: 0,
                },
                InputFile {
                    path: "result_only.rs".to_string(),
                    content: b"fn foo() -> Result<i32, String> { Ok(42) }\n".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let query = parse_query("Result Error").unwrap();
        let options = SearchOptions::default();

        let (tx, rx) = mpsc::channel();
        search_segments_with_query_streaming(&snapshot, &query, &options, tx).unwrap();

        let matches: Vec<FileMatch> = rx.into_iter().collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, PathBuf::from("both.rs"));
    }

    #[test]
    fn test_search_with_query_streaming_dedup() {
        use crate::query::parse_query;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"old\"); }\n".to_vec(),
                mtime: 100,
            }],
        );
        let seg1 = build_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"new\"); }\n".to_vec(),
                mtime: 200,
            }],
        );

        let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);
        let query = parse_query("println").unwrap();
        let options = SearchOptions::default();

        let (tx, rx) = mpsc::channel();
        search_segments_with_query_streaming(&snapshot, &query, &options, tx).unwrap();

        let matches: Vec<FileMatch> = rx.into_iter().collect();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].lines[0].content.contains("new"));
    }

    #[test]
    fn test_search_with_query_streaming_max_results() {
        use crate::query::parse_query;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files: Vec<InputFile> = (0..5)
            .map(|i| InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 0,
            })
            .collect();

        let seg = build_segment(&base_dir, SegmentId(0), files);
        let snapshot: SegmentList = Arc::new(vec![seg]);
        let query = parse_query("println").unwrap();
        let options = SearchOptions {
            context_lines: 0,
            max_results: Some(2),
        };

        let (tx, rx) = mpsc::channel();
        search_segments_with_query_streaming(&snapshot, &query, &options, tx).unwrap();

        let matches: Vec<FileMatch> = rx.into_iter().collect();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_search_with_query_streaming_language_filter() {
        use crate::query::parse_query;

        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
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
                    path: "script.py".to_string(),
                    content: b"def main():\n    println(\"hello\")\n".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let snapshot: SegmentList = Arc::new(vec![seg]);
        let query = parse_query("language:rust println").unwrap();
        let options = SearchOptions::default();

        let (tx, rx) = mpsc::channel();
        search_segments_with_query_streaming(&snapshot, &query, &options, tx).unwrap();

        let matches: Vec<FileMatch> = rx.into_iter().collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, PathBuf::from("main.rs"));
    }

    #[test]
    fn test_lookup_candidates_smallest_first_correctness() {
        // Build a segment where file 0 has many trigrams and file 1 has few.
        // A query that requires trigrams from both should return the correct intersection.
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let files = vec![
            InputFile {
                path: "a.rs".to_string(),
                // Contains many unique trigrams
                content: b"fn alpha_beta_gamma_delta() { return 42; }".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "b.rs".to_string(),
                // Shares "fn " trigram but NOT "alp", "lph", "pha" etc.
                content: b"fn other() { return 99; }".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "c.rs".to_string(),
                content: b"fn alpha_beta_gamma_delta() { return 0; }".to_vec(),
                mtime: 0,
            },
        ];

        let segment_id = SegmentId(0);
        let writer = crate::segment::SegmentWriter::new(&base_dir, segment_id);
        let segment = writer.build(files).unwrap();

        // Query "alpha_beta" should match files 0 and 2 (not file 1)
        let tq = crate::query_trigrams::extract_query_trigrams(&crate::query::Query::Literal(
            crate::query::LiteralQuery {
                text: "alpha_beta".to_string(),
                case_sensitive: false,
            },
        ));

        let candidates = lookup_trigram_candidates(&segment, &tq).unwrap();
        assert_eq!(candidates, vec![FileId(0), FileId(2)]);

        // Query with a trigram NOT in the index should return empty
        let tq_none = crate::query_trigrams::extract_query_trigrams(&crate::query::Query::Literal(
            crate::query::LiteralQuery {
                text: "zzzzzzz".to_string(),
                case_sensitive: false,
            },
        ));
        let candidates_none = lookup_trigram_candidates(&segment, &tq_none).unwrap();
        assert!(candidates_none.is_empty());
    }
}
