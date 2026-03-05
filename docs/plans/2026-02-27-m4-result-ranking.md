# Basic Result Ranking Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement basic result ranking/scoring for search results, so files are ordered by relevance instead of just match density. The ranking system uses a weighted combination of signals: match type, path depth, filename match, match count, and recency, with path-alphabetical tiebreaking.

**Architecture:** Two new pieces: (1) `ranking.rs` module with a `RankingConfig` struct (tunable weights with sensible defaults) and a `score_file_match()` function that takes match metadata and produces a `[0.0, 1.0]` score. (2) Integration into `multi_search.rs` to replace the current simple `match_count / line_count` scoring with the new weighted ranking, and to use path-alphabetical ordering as tiebreaker. The scoring function is pure (no I/O) and operates on data already available during search: the query string, file path, line matches, mtime from metadata, and match type from the query engine. The `MatchType` enum (Exact, Prefix, Substring, Regex) is defined in `ranking.rs` and defaults to `Substring` for the current literal search pipeline.

**Tech Stack:** Rust 2024, existing `ferret-indexer-core` modules (search, multi_search, metadata, types), `tempfile` (dev)

**Prerequisite:** ASCII case-fold trigrams plan (`2026-02-27-ascii-casefold-trigrams.md`) must be implemented first. No direct impact on ranking, but the upstream search pipeline uses case-folded trigrams.

---

## Task 1: Create `ranking.rs` with `MatchType` enum and `RankingConfig` struct

**Files:**
- Create: `ferret-indexer-core/src/ranking.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Write the failing test

Create `ferret-indexer-core/src/ranking.rs` with a test for constructing a default `RankingConfig`:

```rust
//! Result ranking and scoring for search results.
//!
//! Provides a configurable scoring system that combines multiple relevance
//! signals into a single `[0.0, 1.0]` score per file match. Signals include
//! match type weight, path depth, filename match bonus, match count, and
//! file recency.
//!
//! The [`RankingConfig`] struct holds tunable weights with sensible defaults.
//! The [`score_file_match()`] function computes the composite score.

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_weights_sum_to_one() {
        let config = RankingConfig::default();
        let sum = config.match_type_weight
            + config.path_depth_weight
            + config.filename_match_weight
            + config.match_count_weight
            + config.recency_weight;
        assert!(
            (sum - 1.0).abs() < 1e-10,
            "weights should sum to 1.0, got {sum}"
        );
    }

    #[test]
    fn test_match_type_ordering() {
        // Exact > Prefix > Substring > Regex
        assert!(MatchType::Exact.base_score() > MatchType::Prefix.base_score());
        assert!(MatchType::Prefix.base_score() > MatchType::Substring.base_score());
        assert!(MatchType::Substring.base_score() > MatchType::Regex.base_score());
    }
}
```

### Step 2: Register the module in lib.rs

Add to `ferret-indexer-core/src/lib.rs`:

```rust
pub mod ranking;
```

And add re-exports:

```rust
pub use ranking::{MatchType, RankingConfig};
```

### Step 3: Run test to verify it fails

Run: `cargo test -p ferret-indexer-core -- test_default_config -v`

Expected: FAIL -- `RankingConfig` and `MatchType` do not exist yet.

### Step 4: Implement MatchType and RankingConfig

Add to `ranking.rs`, above the test module:

```rust
/// The type of match that produced a search result.
///
/// Used as a scoring signal: exact matches are more relevant than regex matches.
/// The query engine determines the match type; the current literal search
/// pipeline defaults to `Substring`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MatchType {
    /// Query matches the entire file content token (e.g., identifier).
    Exact,
    /// Query matches the beginning of a token.
    Prefix,
    /// Query matches as a substring within content.
    Substring,
    /// Query matches via regex pattern.
    Regex,
}

impl MatchType {
    /// Base score for this match type, in range [0.0, 1.0].
    /// Higher is better. Exact > Prefix > Substring > Regex.
    pub fn base_score(self) -> f64 {
        match self {
            MatchType::Exact => 1.0,
            MatchType::Prefix => 0.8,
            MatchType::Substring => 0.6,
            MatchType::Regex => 0.4,
        }
    }
}

/// Configuration for the result ranking system.
///
/// Each weight controls how much influence its corresponding signal has on
/// the final score. Weights should sum to 1.0 for normalized scoring, though
/// this is not enforced at runtime.
///
/// # Default Weights
///
/// | Signal | Weight | Description |
/// |---|---|---|
/// | Match type | 0.30 | Exact > prefix > substring > regex |
/// | Path depth | 0.15 | Shallower files ranked higher |
/// | Filename match | 0.15 | Bonus if query appears in filename |
/// | Match count | 0.25 | More matches in a file = higher rank |
/// | Recency | 0.15 | Recently modified files get a boost |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingConfig {
    /// Weight for the match type signal (default: 0.30).
    pub match_type_weight: f64,
    /// Weight for the path depth signal (default: 0.15).
    pub path_depth_weight: f64,
    /// Weight for the filename match signal (default: 0.15).
    pub filename_match_weight: f64,
    /// Weight for the match count signal (default: 0.25).
    pub match_count_weight: f64,
    /// Weight for the recency signal (default: 0.15).
    pub recency_weight: f64,
    /// Maximum path depth for normalization (default: 10).
    /// Paths deeper than this get a depth score of 0.
    pub max_path_depth: u32,
    /// Recency half-life in seconds (default: 30 days = 2_592_000).
    /// Files modified this many seconds ago get half the recency bonus.
    pub recency_half_life_secs: u64,
}

impl Default for RankingConfig {
    fn default() -> Self {
        RankingConfig {
            match_type_weight: 0.30,
            path_depth_weight: 0.15,
            filename_match_weight: 0.15,
            match_count_weight: 0.25,
            recency_weight: 0.15,
            max_path_depth: 10,
            recency_half_life_secs: 30 * 24 * 3600, // 30 days
        }
    }
}
```

### Step 5: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_default_config -v && cargo test -p ferret-indexer-core -- test_match_type -v`

Expected: PASS.

### Step 6: Run clippy

Run: `cargo clippy -p ferret-indexer-core -- -D warnings`

Expected: PASS with no warnings.

---

## Task 2: Implement individual scoring signal functions

**Files:**
- Modify: `ferret-indexer-core/src/ranking.rs`

### Step 1: Write the failing tests

Add these tests to the `tests` module in `ranking.rs`:

```rust
    #[test]
    fn test_path_depth_score_shallow() {
        let config = RankingConfig::default();
        // "main.rs" has depth 1 (1 component)
        let score = path_depth_score("main.rs", &config);
        assert!(score > 0.8, "shallow path should score high: {score}");
    }

    #[test]
    fn test_path_depth_score_deep() {
        let config = RankingConfig::default();
        // "a/b/c/d/e/f/g/h/i/j/k.rs" has depth 11
        let score = path_depth_score("a/b/c/d/e/f/g/h/i/j/k.rs", &config);
        assert!(
            score < 0.1,
            "deep path should score low: {score}"
        );
    }

    #[test]
    fn test_path_depth_score_moderate() {
        let config = RankingConfig::default();
        // "src/lib.rs" has depth 2
        let score_shallow = path_depth_score("src/lib.rs", &config);
        let score_deep = path_depth_score("src/a/b/c/lib.rs", &config);
        assert!(
            score_shallow > score_deep,
            "shallower should score higher: {score_shallow} vs {score_deep}"
        );
    }

    #[test]
    fn test_filename_match_score_match() {
        let score = filename_match_score("src/parser.rs", "parser");
        assert!(
            (score - 1.0).abs() < f64::EPSILON,
            "query in filename should score 1.0: {score}"
        );
    }

    #[test]
    fn test_filename_match_score_no_match() {
        let score = filename_match_score("src/parser.rs", "lexer");
        assert!(
            score.abs() < f64::EPSILON,
            "query not in filename should score 0.0: {score}"
        );
    }

    #[test]
    fn test_filename_match_score_case_insensitive() {
        let score = filename_match_score("src/Parser.rs", "parser");
        assert!(
            (score - 1.0).abs() < f64::EPSILON,
            "filename match should be case-insensitive: {score}"
        );
    }

    #[test]
    fn test_match_count_score_many_matches() {
        let score = match_count_score(10, 100);
        assert!(score > 0.0 && score <= 1.0, "score in range: {score}");
    }

    #[test]
    fn test_match_count_score_single_match() {
        let score_one = match_count_score(1, 100);
        let score_ten = match_count_score(10, 100);
        assert!(
            score_ten > score_one,
            "more matches should score higher: {score_ten} vs {score_one}"
        );
    }

    #[test]
    fn test_match_count_score_zero() {
        let score = match_count_score(0, 100);
        assert!(score.abs() < f64::EPSILON, "zero matches should be 0: {score}");
    }

    #[test]
    fn test_recency_score_recent() {
        let config = RankingConfig::default();
        let now = 1_700_000_000u64;
        // Modified 1 hour ago
        let score = recency_score(now - 3600, now, &config);
        assert!(score > 0.9, "recent file should score high: {score}");
    }

    #[test]
    fn test_recency_score_old() {
        let config = RankingConfig::default();
        let now = 1_700_000_000u64;
        // Modified 1 year ago
        let score = recency_score(now - 365 * 24 * 3600, now, &config);
        assert!(score < 0.1, "old file should score low: {score}");
    }

    #[test]
    fn test_recency_score_ordering() {
        let config = RankingConfig::default();
        let now = 1_700_000_000u64;
        let score_recent = recency_score(now - 3600, now, &config);
        let score_old = recency_score(now - 365 * 24 * 3600, now, &config);
        assert!(
            score_recent > score_old,
            "recent should score higher: {score_recent} vs {score_old}"
        );
    }
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_path_depth -v`

Expected: FAIL -- functions don't exist yet.

### Step 3: Implement the signal functions

Add to `ranking.rs`, below `RankingConfig`:

```rust
/// Compute a path depth score in [0.0, 1.0].
///
/// Shallower files score higher. Depth is the number of path components.
/// A file at the root ("main.rs") has depth 1. Score decreases linearly
/// with depth, reaching 0.0 at `config.max_path_depth`.
pub fn path_depth_score(path: &str, config: &RankingConfig) -> f64 {
    let depth = path.chars().filter(|&c| c == '/').count() as u32 + 1;
    if depth >= config.max_path_depth {
        return 0.0;
    }
    1.0 - (depth as f64 / config.max_path_depth as f64)
}

/// Compute a filename match score: 1.0 if query appears in the filename, 0.0 otherwise.
///
/// The comparison is case-insensitive. The "filename" is the last component of
/// the path without the extension. For example, for "src/Parser.rs", the
/// filename is "Parser".
pub fn filename_match_score(path: &str, query: &str) -> f64 {
    let filename = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if filename.to_lowercase().contains(&query.to_lowercase()) {
        1.0
    } else {
        0.0
    }
}

/// Compute a match count score in [0.0, 1.0].
///
/// Uses a logarithmic scale: `log2(1 + match_count) / log2(1 + line_count)`,
/// clamped to [0.0, 1.0]. This rewards files with many matches relative to
/// their size, with diminishing returns for very high match counts.
pub fn match_count_score(match_count: usize, line_count: u32) -> f64 {
    if match_count == 0 {
        return 0.0;
    }
    let line_count = line_count.max(1) as f64;
    let raw = (1.0 + match_count as f64).ln() / (1.0 + line_count).ln();
    raw.clamp(0.0, 1.0)
}

/// Compute a recency score in [0.0, 1.0] using exponential decay.
///
/// Files modified recently score close to 1.0. The score halves every
/// `config.recency_half_life_secs` seconds. Uses the formula:
/// `2^(-age_secs / half_life_secs)`.
///
/// If `mtime` is in the future relative to `now`, returns 1.0.
pub fn recency_score(mtime_epoch_secs: u64, now_epoch_secs: u64, config: &RankingConfig) -> f64 {
    if mtime_epoch_secs >= now_epoch_secs {
        return 1.0;
    }
    let age_secs = (now_epoch_secs - mtime_epoch_secs) as f64;
    let half_life = config.recency_half_life_secs as f64;
    if half_life <= 0.0 {
        return 0.0;
    }
    2.0_f64.powf(-age_secs / half_life)
}
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- ranking -v`

Expected: All ranking tests PASS.

### Step 5: Run clippy

Run: `cargo clippy -p ferret-indexer-core -- -D warnings`

Expected: PASS with no warnings.

---

## Task 3: Implement composite `score_file_match()` function

**Files:**
- Modify: `ferret-indexer-core/src/ranking.rs`

### Step 1: Write the failing tests

Add these tests to the `tests` module:

```rust
    #[test]
    fn test_score_file_match_range() {
        let config = RankingConfig::default();
        let input = ScoringInput {
            path: "src/main.rs",
            query: "main",
            match_type: MatchType::Substring,
            match_count: 3,
            line_count: 50,
            mtime_epoch_secs: 1_700_000_000,
            now_epoch_secs: 1_700_000_000 + 3600,
        };
        let score = score_file_match(&input, &config);
        assert!(
            (0.0..=1.0).contains(&score),
            "score should be in [0.0, 1.0]: {score}"
        );
    }

    #[test]
    fn test_score_exact_beats_substring() {
        let config = RankingConfig::default();
        let base = ScoringInput {
            path: "src/lib.rs",
            query: "search",
            match_type: MatchType::Substring,
            match_count: 1,
            line_count: 100,
            mtime_epoch_secs: 1_700_000_000,
            now_epoch_secs: 1_700_000_000,
        };
        let score_substring = score_file_match(&base, &config);
        let score_exact = score_file_match(
            &ScoringInput {
                match_type: MatchType::Exact,
                ..base
            },
            &config,
        );
        assert!(
            score_exact > score_substring,
            "exact should beat substring: {score_exact} vs {score_substring}"
        );
    }

    #[test]
    fn test_score_shallow_beats_deep() {
        let config = RankingConfig::default();
        let shallow = ScoringInput {
            path: "lib.rs",
            query: "search",
            match_type: MatchType::Substring,
            match_count: 1,
            line_count: 100,
            mtime_epoch_secs: 1_700_000_000,
            now_epoch_secs: 1_700_000_000,
        };
        let deep = ScoringInput {
            path: "a/b/c/d/e/f/g/lib.rs",
            ..shallow
        };
        let score_shallow = score_file_match(&shallow, &config);
        let score_deep = score_file_match(&deep, &config);
        assert!(
            score_shallow > score_deep,
            "shallow should beat deep: {score_shallow} vs {score_deep}"
        );
    }

    #[test]
    fn test_score_filename_match_bonus() {
        let config = RankingConfig::default();
        let with_name = ScoringInput {
            path: "src/search.rs",
            query: "search",
            match_type: MatchType::Substring,
            match_count: 1,
            line_count: 100,
            mtime_epoch_secs: 1_700_000_000,
            now_epoch_secs: 1_700_000_000,
        };
        let without_name = ScoringInput {
            path: "src/utils.rs",
            ..with_name
        };
        let score_with = score_file_match(&with_name, &config);
        let score_without = score_file_match(&without_name, &config);
        assert!(
            score_with > score_without,
            "filename match should boost: {score_with} vs {score_without}"
        );
    }

    #[test]
    fn test_score_more_matches_beats_fewer() {
        let config = RankingConfig::default();
        let many = ScoringInput {
            path: "src/lib.rs",
            query: "search",
            match_type: MatchType::Substring,
            match_count: 10,
            line_count: 100,
            mtime_epoch_secs: 1_700_000_000,
            now_epoch_secs: 1_700_000_000,
        };
        let few = ScoringInput {
            match_count: 1,
            ..many
        };
        let score_many = score_file_match(&many, &config);
        let score_few = score_file_match(&few, &config);
        assert!(
            score_many > score_few,
            "more matches should score higher: {score_many} vs {score_few}"
        );
    }

    #[test]
    fn test_score_recent_beats_old() {
        let config = RankingConfig::default();
        let now = 1_700_000_000u64;
        let recent = ScoringInput {
            path: "src/lib.rs",
            query: "search",
            match_type: MatchType::Substring,
            match_count: 1,
            line_count: 100,
            mtime_epoch_secs: now - 3600,
            now_epoch_secs: now,
        };
        let old = ScoringInput {
            mtime_epoch_secs: now - 365 * 24 * 3600,
            ..recent
        };
        let score_recent = score_file_match(&recent, &config);
        let score_old = score_file_match(&old, &config);
        assert!(
            score_recent > score_old,
            "recent should beat old: {score_recent} vs {score_old}"
        );
    }
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_score_file_match -v`

Expected: FAIL -- `ScoringInput` and `score_file_match` don't exist yet.

### Step 3: Implement ScoringInput and score_file_match

Add to `ranking.rs`:

```rust
/// Input data for scoring a single file match.
///
/// This struct gathers all the signals needed to compute a relevance score.
/// It is constructed from data already available during search (FileMatch +
/// FileMetadata fields), avoiding extra I/O.
#[derive(Debug, Clone)]
pub struct ScoringInput<'a> {
    /// File path relative to repository root.
    pub path: &'a str,
    /// The original query string.
    pub query: &'a str,
    /// How the query matched (exact, prefix, substring, regex).
    pub match_type: MatchType,
    /// Total number of match ranges across all matching lines.
    pub match_count: usize,
    /// Total number of lines in the file.
    pub line_count: u32,
    /// File modification time as seconds since Unix epoch.
    pub mtime_epoch_secs: u64,
    /// Current time as seconds since Unix epoch.
    pub now_epoch_secs: u64,
}

/// Compute a composite relevance score in [0.0, 1.0] for a file match.
///
/// Combines five signals with configurable weights:
/// - **Match type**: Exact > Prefix > Substring > Regex
/// - **Path depth**: Shallower files rank higher
/// - **Filename match**: Bonus if query appears in filename
/// - **Match count**: More matches (log-scaled) = higher rank
/// - **Recency**: Recently modified files get a boost (exponential decay)
///
/// Each signal produces a value in [0.0, 1.0], multiplied by its weight.
/// The weighted sum is the final score.
pub fn score_file_match(input: &ScoringInput<'_>, config: &RankingConfig) -> f64 {
    let mt_score = input.match_type.base_score();
    let pd_score = path_depth_score(input.path, config);
    let fn_score = filename_match_score(input.path, input.query);
    let mc_score = match_count_score(input.match_count, input.line_count);
    let rc_score = recency_score(input.mtime_epoch_secs, input.now_epoch_secs, config);

    let score = mt_score * config.match_type_weight
        + pd_score * config.path_depth_weight
        + fn_score * config.filename_match_weight
        + mc_score * config.match_count_weight
        + rc_score * config.recency_weight;

    score.clamp(0.0, 1.0)
}
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- ranking -v`

Expected: All ranking tests PASS.

### Step 5: Run clippy

Run: `cargo clippy -p ferret-indexer-core -- -D warnings`

Expected: PASS with no warnings.

---

## Task 4: Integrate ranking into `multi_search.rs`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs`

### Step 1: Write the failing test

Add this test to `multi_search.rs`:

```rust
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
```

### Step 2: Run test to verify it fails

Run: `cargo test -p ferret-indexer-core -- test_search_segments_uses_ranking -v`

Expected: FAIL -- the current scoring doesn't account for filename or path depth.

### Step 3: Integrate ranking into search_single_segment

Replace the scoring logic in `search_single_segment` in `multi_search.rs`. Change the function to accept the query string and use the new ranking system:

```rust
use crate::ranking::{score_file_match, MatchType, RankingConfig, ScoringInput};
use std::time::{SystemTime, UNIX_EPOCH};
```

In `search_single_segment`, replace the current score computation:

```rust
        // Compute a simple relevance score: match count / line count
        // (more matches relative to file size = more relevant)
        let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
        let line_count = meta.line_count.max(1) as f64;
        let score = (total_match_ranges as f64 / line_count).min(1.0);
```

With the new ranking:

```rust
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
```

### Step 4: Add path-alphabetical tiebreaker to search_segments

In `search_segments`, replace the sort:

```rust
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
```

With:

```rust
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
```

### Step 5: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- multi_search -v`

Expected: All multi_search tests PASS, including the new ranking test.

### Step 6: Run the full test suite

Run: `cargo test --workspace`

Expected: All tests PASS.

### Step 7: Run clippy

Run: `cargo clippy --workspace -- -D warnings`

Expected: PASS with no warnings.

---

## Task 5: Add tiebreaker test and edge case tests

**Files:**
- Modify: `ferret-indexer-core/src/ranking.rs`
- Modify: `ferret-indexer-core/src/multi_search.rs`

### Step 1: Add edge case tests to ranking.rs

Add to the `tests` module in `ranking.rs`:

```rust
    #[test]
    fn test_path_depth_score_root_file() {
        let config = RankingConfig::default();
        let score = path_depth_score("Cargo.toml", &config);
        // depth=1 -> score = 1.0 - 1/10 = 0.9
        assert!((score - 0.9).abs() < f64::EPSILON, "root file score: {score}");
    }

    #[test]
    fn test_filename_match_partial() {
        // "parse" should match in "parser.rs"
        let score = filename_match_score("src/parser.rs", "parse");
        assert!((score - 1.0).abs() < f64::EPSILON, "partial match: {score}");
    }

    #[test]
    fn test_filename_match_empty_query() {
        // Empty query matches any filename
        let score = filename_match_score("src/lib.rs", "");
        assert!((score - 1.0).abs() < f64::EPSILON, "empty query matches: {score}");
    }

    #[test]
    fn test_recency_score_same_time() {
        let config = RankingConfig::default();
        let score = recency_score(1_700_000_000, 1_700_000_000, &config);
        assert!((score - 1.0).abs() < f64::EPSILON, "same time = 1.0: {score}");
    }

    #[test]
    fn test_recency_score_at_half_life() {
        let config = RankingConfig::default();
        let now = 1_700_000_000u64;
        let mtime = now - config.recency_half_life_secs;
        let score = recency_score(mtime, now, &config);
        assert!(
            (score - 0.5).abs() < 0.01,
            "at half-life should be ~0.5: {score}"
        );
    }

    #[test]
    fn test_match_count_score_large_file() {
        // 1 match in 10000 lines should still produce a positive score
        let score = match_count_score(1, 10000);
        assert!(score > 0.0, "should be positive: {score}");
        assert!(score < 0.2, "should be small: {score}");
    }

    #[test]
    fn test_score_file_match_zero_line_count() {
        let config = RankingConfig::default();
        let input = ScoringInput {
            path: "empty.rs",
            query: "test",
            match_type: MatchType::Substring,
            match_count: 0,
            line_count: 0,
            mtime_epoch_secs: 1_700_000_000,
            now_epoch_secs: 1_700_000_000,
        };
        let score = score_file_match(&input, &config);
        assert!(
            (0.0..=1.0).contains(&score),
            "should handle zero line count: {score}"
        );
    }
```

### Step 2: Add tiebreaker test to multi_search.rs

Add to the `tests` module in `multi_search.rs`:

```rust
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
```

### Step 3: Run all tests

Run: `cargo test --workspace`

Expected: All tests PASS.

### Step 4: Run clippy and fmt

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: PASS.

---

## Task 6: Final integration verification

**Files:**
- No changes — verification only.

### Step 1: Run the full test suite

Run: `cargo test --workspace`

Expected: All tests PASS.

### Step 2: Run clippy

Run: `cargo clippy --workspace -- -D warnings`

Expected: PASS with no warnings.

### Step 3: Run format check

Run: `cargo fmt --all -- --check`

Expected: PASS.

### Step 4: Verify the demo still works

Run: `cargo run -p ferret-indexer-core --example demo -- . "fn main" 2>&1 | head -20`

Expected: Results are returned with the new ranking applied (files with "main" in the filename should appear first, shallow files preferred).

---

## Summary

| Task | Description | Files |
|------|-------------|-------|
| 1 | `MatchType` enum + `RankingConfig` struct with default weights | `ranking.rs`, `lib.rs` |
| 2 | Individual signal functions: path_depth, filename_match, match_count, recency | `ranking.rs` |
| 3 | Composite `score_file_match()` with `ScoringInput` | `ranking.rs` |
| 4 | Integrate into `multi_search.rs`, add path-alphabetical tiebreaker | `multi_search.rs` |
| 5 | Edge case tests + tiebreaker test | `ranking.rs`, `multi_search.rs` |
| 6 | Final verification (tests, clippy, fmt, demo) | None |

**Design decisions:**
- Weights sum to 1.0 by default (0.30 + 0.15 + 0.15 + 0.25 + 0.15) for normalized scoring
- Match count uses log-scale (`ln(1+count)/ln(1+lines)`) to prevent large files from dominating
- Recency uses exponential decay with 30-day half-life (tunable)
- Path depth is linear with max depth of 10 (tunable)
- Filename match is binary (0 or 1) with case-insensitive comparison
- `MatchType` defaults to `Substring` for current literal search pipeline; the query engine (HHC-46) will set the appropriate type once implemented
- `RankingConfig` is serializable for future config file support
- All scoring functions are pure (no I/O), testable in isolation
