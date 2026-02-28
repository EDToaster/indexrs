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
