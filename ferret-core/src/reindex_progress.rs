//! Structured progress events emitted during reindex operations.

use serde::{Deserialize, Serialize};

/// A structured progress event emitted during reindex.
///
/// Sent as JSON over the daemon wire protocol inside
/// `DaemonResponse::Progress { message }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReindexProgress {
    /// Change detection started.
    DetectingChanges,
    /// Fell back to hash-based scanning (git unavailable).
    ScanningFallback,
    /// Change detection complete.
    ChangesDetected {
        created: usize,
        modified: usize,
        deleted: usize,
    },
    /// No changes found.
    NoChanges,
    /// Waiting for the write lock (another operation is in progress).
    WaitingForLock,
    /// Reading and filtering changed files before indexing.
    PreparingFiles { current: usize, total: usize },
    /// Building a segment: file `files_done` of `files_total` processed.
    BuildingSegment {
        segment_id: u32,
        files_done: usize,
        files_total: usize,
    },
    /// Writing tombstones for old file entries.
    Tombstoning { count: u32 },
    /// Segment compaction started.
    CompactingSegments { input_segments: usize },
    /// Live entries collected from segments for compaction.
    CompactingCollected {
        live_files: usize,
        tombstoned: usize,
    },
    /// Decompressing file content during compaction.
    CompactingFiles { current: usize, total: usize },
    /// Writing a compacted segment.
    CompactingWriting {
        segment_id: u32,
        files_done: usize,
        files_total: usize,
    },
    /// Compaction finished.
    CompactionComplete {
        input_segments: usize,
        output_segments: usize,
        duration_ms: u64,
    },
    /// Reindex finished successfully.
    Complete { changes_applied: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serde_roundtrip_detecting_changes() {
        let event = ReindexProgress::DetectingChanges;
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"type":"detecting_changes"}"#);
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_building_segment() {
        let event = ReindexProgress::BuildingSegment {
            segment_id: 3,
            files_done: 100,
            files_total: 500,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_changes_detected() {
        let event = ReindexProgress::ChangesDetected {
            created: 10,
            modified: 20,
            deleted: 5,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_compacting_collected() {
        let event = ReindexProgress::CompactingCollected {
            live_files: 100,
            tombstoned: 25,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_compacting_files() {
        let event = ReindexProgress::CompactingFiles {
            current: 50,
            total: 200,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_compacting_writing() {
        let event = ReindexProgress::CompactingWriting {
            segment_id: 5,
            files_done: 30,
            files_total: 100,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_compaction_complete() {
        let event = ReindexProgress::CompactionComplete {
            input_segments: 5,
            output_segments: 1,
            duration_ms: 3200,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_complete() {
        let event = ReindexProgress::Complete {
            changes_applied: 42,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }
}
