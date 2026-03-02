//! Hash-based diff: compare on-disk files against indexed segment metadata.
//!
//! When git-based change detection is unavailable (no git repo, no checkpoint,
//! or git diff fails), this module provides a fallback catch-up mechanism.
//! It walks the file tree, computes blake3 hashes, and compares them against
//! what is stored in segment metadata, emitting [`ChangeEvent`]s for any
//! differences.

use std::collections::HashMap;
use std::path::Path;

use crate::binary::should_index_file;
use crate::changes::{ChangeEvent, ChangeKind};
use crate::error::Result;
use crate::index_state::SegmentList;
use crate::tombstone::TombstoneSet;
use crate::walker::DirectoryWalkerBuilder;

/// Default maximum file size for indexing (1 MB).
const MAX_FILE_SIZE: u64 = 1_048_576;

/// Compare the on-disk file tree against indexed segments and return change events.
///
/// Walks `repo_root`, computes blake3 hashes, compares against segment metadata.
/// Returns `ChangeEvent`s for:
/// - **Created** -- new file on disk, not in any segment
/// - **Modified** -- file on disk has a different blake3 hash than the indexed version
/// - **Deleted** -- file in the index but no longer on disk
///
/// Tombstoned entries in segments are excluded from comparison (they represent
/// files that have already been removed or superseded).
///
/// Results are sorted by path for deterministic output.
pub fn hash_diff(repo_root: &Path, segments: &SegmentList) -> Result<Vec<ChangeEvent>> {
    // Build a map of path -> content_hash from all segments, skipping tombstoned entries.
    let mut indexed: HashMap<String, [u8; 16]> = HashMap::new();
    for segment in segments.iter() {
        let tombstones = {
            let path = segment.dir_path().join("tombstones.bin");
            let data = std::fs::read(&path)?;
            if data.is_empty() {
                TombstoneSet::new()
            } else {
                TombstoneSet::read_from(&path)?
            }
        };
        let reader = segment.metadata_reader();
        for entry in reader.iter_all() {
            let meta = entry?;
            if tombstones.contains(meta.file_id) {
                continue;
            }
            indexed.insert(meta.path.clone(), meta.content_hash);
        }
    }

    // Walk the file tree.
    let walker = DirectoryWalkerBuilder::new(repo_root).build();
    let walked = walker.run()?;

    let mut events = Vec::new();
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    for file in walked {
        let rel_path = match file.path.strip_prefix(repo_root) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let rel_str = rel_path.to_string_lossy().to_string();
        seen_paths.insert(rel_str.clone());

        let content = match std::fs::read(&file.path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !should_index_file(&file.path, &content, MAX_FILE_SIZE) {
            continue;
        }

        let hash = blake3::hash(&content);
        let mut hash_16 = [0u8; 16];
        hash_16.copy_from_slice(&hash.as_bytes()[..16]);

        match indexed.get(&rel_str) {
            None => {
                events.push(ChangeEvent {
                    path: rel_path.to_path_buf(),
                    kind: ChangeKind::Created,
                });
            }
            Some(existing_hash) if *existing_hash != hash_16 => {
                events.push(ChangeEvent {
                    path: rel_path.to_path_buf(),
                    kind: ChangeKind::Modified,
                });
            }
            _ => {} // Hash matches, unchanged.
        }
    }

    // Find deleted files: in index but not on disk.
    for path_str in indexed.keys() {
        if !seen_paths.contains(path_str) {
            events.push(ChangeEvent {
                path: std::path::PathBuf::from(path_str),
                kind: ChangeKind::Deleted,
            });
        }
    }

    events.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{InputFile, SegmentWriter};
    use crate::types::SegmentId;
    use std::sync::Arc;

    fn build_segment(
        segments_dir: &Path,
        id: u32,
        files: Vec<InputFile>,
    ) -> Arc<crate::segment::Segment> {
        let writer = SegmentWriter::new(segments_dir, SegmentId(id));
        let segment = writer.build(files).unwrap();
        Arc::new(segment)
    }

    /// Helper: set up a git-initialised tempdir with .indexrs/segments/.
    /// Returns (tempdir, repo_root, segments_dir).
    fn setup_test_repo() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        // git init so the walker respects gitignore rules
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .unwrap();

        let segments_dir = root.join(".indexrs").join("segments");
        std::fs::create_dir_all(&segments_dir).unwrap();

        (tmp, root, segments_dir)
    }

    #[test]
    fn test_hash_diff_empty_index_all_created() {
        let (_tmp, root, _segments_dir) = setup_test_repo();

        // Write two files on disk.
        std::fs::write(root.join("hello.rs"), b"fn hello() {}").unwrap();
        std::fs::write(root.join("world.rs"), b"fn world() {}").unwrap();

        // Empty segment list.
        let segments: SegmentList = Arc::new(vec![]);

        let events = hash_diff(&root, &segments).unwrap();

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.kind == ChangeKind::Created));

        let paths: Vec<String> = events
            .iter()
            .map(|e| e.path.to_string_lossy().to_string())
            .collect();
        assert!(paths.contains(&"hello.rs".to_string()));
        assert!(paths.contains(&"world.rs".to_string()));
    }

    #[test]
    fn test_hash_diff_unchanged_files_no_events() {
        let (_tmp, root, segments_dir) = setup_test_repo();

        let content = b"fn unchanged() { let x = 42; }";

        // Write file on disk.
        std::fs::write(root.join("stable.rs"), content).unwrap();

        // Build a segment with the same content.
        let seg = build_segment(
            &segments_dir,
            0,
            vec![InputFile {
                path: "stable.rs".to_string(),
                content: content.to_vec(),
                mtime: 0,
            }],
        );

        let segments: SegmentList = Arc::new(vec![seg]);

        let events = hash_diff(&root, &segments).unwrap();

        assert!(
            events.is_empty(),
            "expected no events for unchanged files, got: {events:?}"
        );
    }

    #[test]
    fn test_hash_diff_modified_file() {
        let (_tmp, root, segments_dir) = setup_test_repo();

        let old_content = b"fn original() { let x = 1; }";
        let new_content = b"fn modified() { let x = 2; }";

        // Write file with NEW content on disk.
        std::fs::write(root.join("changed.rs"), new_content).unwrap();

        // Build a segment with OLD content.
        let seg = build_segment(
            &segments_dir,
            0,
            vec![InputFile {
                path: "changed.rs".to_string(),
                content: old_content.to_vec(),
                mtime: 0,
            }],
        );

        let segments: SegmentList = Arc::new(vec![seg]);

        let events = hash_diff(&root, &segments).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path.to_string_lossy(), "changed.rs");
        assert_eq!(events[0].kind, ChangeKind::Modified);
    }

    #[test]
    fn test_hash_diff_deleted_file() {
        let (_tmp, root, segments_dir) = setup_test_repo();

        // Do NOT write "gone.rs" on disk -- it only exists in the index.

        let seg = build_segment(
            &segments_dir,
            0,
            vec![InputFile {
                path: "gone.rs".to_string(),
                content: b"fn gone() {}".to_vec(),
                mtime: 0,
            }],
        );

        let segments: SegmentList = Arc::new(vec![seg]);

        let events = hash_diff(&root, &segments).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path.to_string_lossy(), "gone.rs");
        assert_eq!(events[0].kind, ChangeKind::Deleted);
    }
}
