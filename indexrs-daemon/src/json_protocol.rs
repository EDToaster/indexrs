//! Structured response types for the JSON daemon protocol.

use serde::{Deserialize, Serialize};

use indexrs_core::search::FileMatch;

/// Wrapper for JSON search response frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JsonSearchFrame {
    #[serde(rename = "result")]
    Result { file: FileMatch },
    #[serde(rename = "stats")]
    Stats { stats: SearchStats },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchStats {
    pub total_matches: usize,
    pub files_matched: usize,
    pub duration_ms: u64,
    pub page: usize,
    pub per_page: usize,
    pub total_pages: usize,
    pub has_next: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileResponse {
    pub path: String,
    pub language: String,
    pub total_lines: usize,
    pub lines: Vec<String>,
}

/// Per-segment size breakdown.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SegmentInfo {
    pub id: u32,
    pub entry_count: u32,
    pub tombstoned_count: u32,
    pub trigrams_bytes: u64,
    pub meta_paths_bytes: u64,
    pub content_bytes: u64,
    pub tombstones_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub files_indexed: usize,
    pub segments: usize,
    /// Total bytes on disk for the index directory (.indexrs/segments/).
    #[serde(default)]
    pub index_bytes: u64,
    /// Unix epoch seconds of the most recently modified file in the index.
    #[serde(default)]
    pub last_indexed_ts: u64,
    /// Top languages by file count: vec of (language_name, file_count).
    #[serde(default)]
    pub languages: Vec<(String, usize)>,
    /// Fraction of entries that are tombstoned (0.0 to 1.0).
    #[serde(default)]
    pub tombstone_ratio: f32,
    /// Whether the registered repo path exists on disk.
    #[serde(default = "default_true")]
    pub path_valid: bool,
    /// Absolute count of tombstoned entries across all segments.
    #[serde(default)]
    pub tombstoned_count: u32,
    /// Aggregate content.zst size in bytes.
    #[serde(default)]
    pub content_bytes: u64,
    /// Aggregate trigrams.bin size in bytes.
    #[serde(default)]
    pub trigrams_bytes: u64,
    /// Aggregate meta.bin + paths.bin size in bytes.
    #[serde(default)]
    pub meta_paths_bytes: u64,
    /// Per-segment breakdown.
    #[serde(default)]
    pub segment_details: Vec<SegmentInfo>,
    /// Per-language file extension breakdown: vec of (language_name, vec of (extension, count)).
    #[serde(default)]
    pub language_extensions: Vec<(String, Vec<(String, usize)>)>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime_seconds: u64,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indexrs_core::search::LineMatch;
    use indexrs_core::types::{FileId, Language};

    use super::*;

    #[test]
    fn test_json_search_frame_result_serialization() {
        let frame = JsonSearchFrame::Result {
            file: FileMatch {
                file_id: FileId(1),
                path: PathBuf::from("src/main.rs"),
                language: Language::Rust,
                lines: vec![LineMatch {
                    line_number: 10,
                    content: "fn main() {}".to_string(),
                    ranges: vec![(0, 7)],
                    context_before: vec![],
                    context_after: vec![],
                }],
                score: 0.95,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""type":"result"#));
        assert!(json.contains("src/main.rs"));
        assert!(json.contains("fn main()"));
    }

    #[test]
    fn test_json_search_frame_stats_serialization() {
        let frame = JsonSearchFrame::Stats {
            stats: SearchStats {
                total_matches: 42,
                files_matched: 5,
                duration_ms: 123,
                page: 1,
                per_page: 20,
                total_pages: 3,
                has_next: true,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""type":"stats"#));
        assert!(json.contains(r#""total_matches":42"#));
        assert!(json.contains(r#""has_next":true"#));

        // Roundtrip
        let deserialized: JsonSearchFrame = serde_json::from_str(&json).unwrap();
        match deserialized {
            JsonSearchFrame::Stats { stats } => {
                assert_eq!(stats.total_matches, 42);
                assert_eq!(stats.files_matched, 5);
                assert_eq!(stats.duration_ms, 123);
                assert_eq!(stats.page, 1);
                assert_eq!(stats.per_page, 20);
                assert_eq!(stats.total_pages, 3);
                assert!(stats.has_next);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn test_file_response_serialization() {
        let resp = FileResponse {
            path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            total_lines: 100,
            lines: vec![
                "use std::io;".to_string(),
                "".to_string(),
                "fn main() {}".to_string(),
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("src/lib.rs"));
        assert!(json.contains(r#""total_lines":100"#));

        let deserialized: FileResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.path, "src/lib.rs");
        assert_eq!(deserialized.language, "rust");
        assert_eq!(deserialized.total_lines, 100);
        assert_eq!(deserialized.lines.len(), 3);
    }

    #[test]
    fn test_status_response_serialization() {
        let resp = StatusResponse {
            status: "ready".to_string(),
            files_indexed: 1234,
            segments: 3,
            index_bytes: 5000,
            last_indexed_ts: 1700000000,
            languages: vec![("Rust".to_string(), 100), ("Python".to_string(), 50)],
            tombstone_ratio: 0.05,
            path_valid: true,
            tombstoned_count: 10,
            content_bytes: 3000,
            trigrams_bytes: 1500,
            meta_paths_bytes: 500,
            segment_details: vec![],
            language_extensions: vec![
                ("Rust".to_string(), vec![("rs".to_string(), 100)]),
                (
                    "Python".to_string(),
                    vec![("py".to_string(), 45), ("pyi".to_string(), 5)],
                ),
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""status":"ready"#));
        assert!(json.contains(r#""files_indexed":1234"#));
        assert!(json.contains(r#""segments":3"#));

        let deserialized: StatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.status, "ready");
        assert_eq!(deserialized.files_indexed, 1234);
        assert_eq!(deserialized.segments, 3);
        assert_eq!(deserialized.index_bytes, 5000);
        assert_eq!(deserialized.last_indexed_ts, 1700000000);
        assert_eq!(deserialized.languages.len(), 2);
        assert!(deserialized.path_valid);
    }

    #[test]
    fn test_status_response_backward_compat() {
        // Old JSON without the new fields should deserialize with defaults.
        let old_json = r#"{
            "status": "ready",
            "files_indexed": 42,
            "segments": 2,
            "index_bytes": 1000,
            "last_indexed_ts": 1700000000,
            "languages": [],
            "tombstone_ratio": 0.0,
            "path_valid": true
        }"#;
        let resp: StatusResponse = serde_json::from_str(old_json).unwrap();
        assert_eq!(resp.tombstoned_count, 0);
        assert_eq!(resp.content_bytes, 0);
        assert_eq!(resp.trigrams_bytes, 0);
        assert_eq!(resp.meta_paths_bytes, 0);
        assert!(resp.segment_details.is_empty());
    }

    #[test]
    fn test_health_response_serialization() {
        let resp = HealthResponse {
            status: "healthy".to_string(),
            version: "0.1.0".to_string(),
            uptime_seconds: 3600,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""status":"healthy"#));
        assert!(json.contains(r#""version":"0.1.0"#));
        assert!(json.contains(r#""uptime_seconds":3600"#));

        let deserialized: HealthResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.status, "healthy");
        assert_eq!(deserialized.version, "0.1.0");
        assert_eq!(deserialized.uptime_seconds, 3600);
    }
}
