//! Index state management with snapshot isolation.
//!
//! [`IndexState`] holds the current list of active segments as an
//! `Arc<Vec<Arc<Segment>>>`. Readers call [`snapshot()`](IndexState::snapshot)
//! to get a consistent, lock-free view. Writers call
//! [`publish()`](IndexState::publish) to atomically swap in a new segment list.
//!
//! The `SegmentList` type alias provides a convenient name for the snapshot type.

use std::sync::{Arc, RwLock};

use crate::segment::Segment;

/// A snapshot of the active segment list. Cheap `Arc::clone()` for readers.
pub type SegmentList = Arc<Vec<Arc<Segment>>>;

/// Manages the current set of active segments with snapshot isolation.
///
/// Readers call [`snapshot()`](Self::snapshot) to get a consistent `SegmentList`
/// via a shared `RwLock` read — multiple readers proceed in parallel without
/// blocking each other. Writers call [`publish()`](Self::publish) to atomically
/// swap in a new segment list under an exclusive write lock.
///
/// # Concurrency Model
///
/// - **Readers**: `snapshot()` takes a shared read lock and clones the `Arc`,
///   so multiple readers never block each other. The returned `SegmentList`
///   is a frozen view that remains valid regardless of subsequent `publish()`.
///
/// - **Writers**: `publish()` takes an exclusive write lock. Only one thread
///   can publish at a time. Readers are briefly blocked during the `Arc` swap.
pub struct IndexState {
    /// The current segment list, wrapped in Arc for snapshot reads.
    /// RwLock allows concurrent readers; writers take exclusive access.
    current: RwLock<SegmentList>,
}

impl IndexState {
    /// Create a new IndexState with an empty segment list.
    pub fn new() -> Self {
        IndexState {
            current: RwLock::new(Arc::new(Vec::new())),
        }
    }

    /// Take a snapshot of the current segment list.
    ///
    /// Takes a shared read lock and clones the `Arc` — multiple readers
    /// proceed in parallel. The returned `SegmentList` is a frozen view
    /// that remains valid regardless of subsequent `publish()` calls.
    pub fn snapshot(&self) -> SegmentList {
        let guard = self.current.read().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&guard)
    }

    /// Atomically replace the segment list with a new one.
    ///
    /// Only one writer can publish at a time (serialized by RwLock write).
    /// Existing snapshots are unaffected -- they hold their own `Arc` references.
    pub fn publish(&self, new_segments: Vec<Arc<Segment>>) {
        let mut guard = self.current.write().unwrap_or_else(|e| e.into_inner());
        *guard = Arc::new(new_segments);
    }
}

impl Default for IndexState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{InputFile, SegmentWriter};
    use crate::types::SegmentId;

    /// Helper: build a segment with the given ID and files in a temp directory.
    fn build_test_segment(
        base_dir: &std::path::Path,
        segment_id: SegmentId,
        files: Vec<InputFile>,
    ) -> Arc<Segment> {
        let writer = SegmentWriter::new(base_dir, segment_id);
        Arc::new(writer.build(files).unwrap())
    }

    #[test]
    fn test_index_state_new_is_empty() {
        let state = IndexState::new();
        let snap = state.snapshot();
        assert!(snap.is_empty());
    }

    #[test]
    fn test_publish_and_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_test_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "a.rs".to_string(),
                content: b"fn alpha() {}".to_vec(),
                mtime: 0,
            }],
        );

        let state = IndexState::new();
        state.publish(vec![seg0.clone()]);

        let snap = state.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].segment_id(), SegmentId(0));
    }

    #[test]
    fn test_snapshot_is_isolated_from_publish() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_test_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "a.rs".to_string(),
                content: b"fn alpha() {}".to_vec(),
                mtime: 0,
            }],
        );

        let seg1 = build_test_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "b.rs".to_string(),
                content: b"fn beta() {}".to_vec(),
                mtime: 0,
            }],
        );

        let state = IndexState::new();
        state.publish(vec![seg0.clone()]);

        // Take a snapshot before publishing seg1
        let snap_before = state.snapshot();
        assert_eq!(snap_before.len(), 1);

        // Publish a new list with both segments
        state.publish(vec![seg0, seg1]);

        // The old snapshot should still see only 1 segment
        assert_eq!(snap_before.len(), 1);

        // A new snapshot sees both segments
        let snap_after = state.snapshot();
        assert_eq!(snap_after.len(), 2);
    }

    #[test]
    fn test_publish_replaces_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_test_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "a.rs".to_string(),
                content: b"fn alpha() {}".to_vec(),
                mtime: 0,
            }],
        );

        let seg1 = build_test_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "b.rs".to_string(),
                content: b"fn beta() {}".to_vec(),
                mtime: 0,
            }],
        );

        let state = IndexState::new();
        state.publish(vec![seg0, seg1]);
        assert_eq!(state.snapshot().len(), 2);

        // Publish empty list
        state.publish(vec![]);
        assert_eq!(state.snapshot().len(), 0);
    }

    #[test]
    fn test_default_trait() {
        let state = IndexState::default();
        assert!(state.snapshot().is_empty());
    }

    // ---- Integration tests with search_segments ----

    use crate::multi_search::search_segments;
    use std::path::PathBuf;

    #[test]
    fn test_index_state_search_integration() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_test_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() {\n    println!(\"hello world\");\n}\n".to_vec(),
                mtime: 0,
            }],
        );

        let seg1 = build_test_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "lib.rs".to_string(),
                content: b"pub fn greet() {\n    println!(\"greetings\");\n}\n".to_vec(),
                mtime: 0,
            }],
        );

        let state = IndexState::new();
        state.publish(vec![seg0, seg1]);

        let snapshot = state.snapshot();
        let result = search_segments(&snapshot, "println").unwrap();

        assert_eq!(result.files.len(), 2);
        let paths: Vec<&str> = result
            .files
            .iter()
            .map(|f| f.path.to_str().unwrap())
            .collect();
        assert!(paths.contains(&"main.rs"));
        assert!(paths.contains(&"lib.rs"));
        assert_eq!(result.total_match_count, 2);
    }

    #[test]
    fn test_index_state_snapshot_isolation_during_search() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".ferret_index/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_test_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() { println!(\"v1\"); }\n".to_vec(),
                mtime: 100,
            }],
        );

        let state = IndexState::new();
        state.publish(vec![seg0]);

        // Take snapshot before adding a new segment
        let snap_v1 = state.snapshot();

        let seg1 = build_test_segment(
            &base_dir,
            SegmentId(1),
            vec![InputFile {
                path: "extra.rs".to_string(),
                content: b"fn extra() { println!(\"new\"); }\n".to_vec(),
                mtime: 200,
            }],
        );

        // Publish updated list (both segments)
        let snap_current = state.snapshot();
        let mut new_list: Vec<Arc<Segment>> = snap_current.iter().cloned().collect();
        new_list.push(seg1);
        state.publish(new_list);

        // Search on the OLD snapshot should only find main.rs
        let result_v1 = search_segments(&snap_v1, "println").unwrap();
        assert_eq!(result_v1.files.len(), 1);
        assert_eq!(result_v1.files[0].path, PathBuf::from("main.rs"));

        // Search on a NEW snapshot should find both
        let snap_v2 = state.snapshot();
        let result_v2 = search_segments(&snap_v2, "println").unwrap();
        assert_eq!(result_v2.files.len(), 2);
    }
}
