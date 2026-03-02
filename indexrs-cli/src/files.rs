use std::collections::HashMap;

use globset::{Glob, GlobMatcher};
use indexrs_core::error::IndexError;
use indexrs_core::index_state::SegmentList;
use indexrs_core::metadata::FileMetadata;
use indexrs_core::types::SegmentId;

use crate::args::SortOrder;
use crate::color::ColorConfig;
use crate::output::{ExitCode, StreamingWriter};
use crate::paths::PathRewriter;

/// Filter options for the files command.
#[derive(Default)]
pub struct FilesFilter {
    pub language: Option<String>,
    pub path_glob: Option<String>,
    pub sort: SortOrder,
    pub limit: Option<usize>,
}

/// Collect all indexed files from the snapshot, applying filters and sorting.
///
/// Handles tombstone filtering and cross-segment deduplication (newest segment wins).
pub fn collect_files(
    snapshot: &SegmentList,
    filter: &FilesFilter,
) -> Result<Vec<FileMetadata>, IndexError> {
    let glob_matcher: Option<GlobMatcher> = filter
        .path_glob
        .as_ref()
        .map(|g| Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;

    // Collect files across segments, dedup by path (newest segment wins)
    let mut seen: HashMap<String, (SegmentId, FileMetadata)> = HashMap::new();

    for segment in snapshot.iter() {
        let tombstones = segment.load_tombstones()?;
        let reader = segment.metadata_reader();
        let seg_id = segment.segment_id();

        for entry in reader.iter_all() {
            let entry = entry?;
            if tombstones.contains(entry.file_id) {
                continue;
            }

            // Language filter
            if let Some(ref lang) = filter.language
                && !entry.language.to_string().eq_ignore_ascii_case(lang)
            {
                continue;
            }

            // Path glob filter
            if let Some(ref matcher) = glob_matcher
                && !matcher.is_match(&entry.path)
            {
                continue;
            }

            // Dedup: keep newest segment
            match seen.get(&entry.path) {
                Some((existing_seg, _)) if *existing_seg >= seg_id => continue,
                _ => {
                    seen.insert(entry.path.clone(), (seg_id, entry));
                }
            }
        }
    }

    let mut files: Vec<FileMetadata> = seen.into_values().map(|(_, meta)| meta).collect();

    // Sort
    match filter.sort {
        SortOrder::Path => files.sort_by(|a, b| a.path.cmp(&b.path)),
        SortOrder::Modified => files.sort_by(|a, b| b.mtime_epoch_secs.cmp(&a.mtime_epoch_secs)),
        SortOrder::Size => files.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes)),
    }

    // Limit
    if let Some(limit) = filter.limit {
        files.truncate(limit);
    }

    Ok(files)
}

/// Run the files command: collect, format, and stream file paths to stdout.
pub fn run_files<W: std::io::Write>(
    snapshot: &SegmentList,
    filter: &FilesFilter,
    color: &ColorConfig,
    path_rewriter: &PathRewriter,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
    let files = collect_files(snapshot, filter)?;

    if files.is_empty() {
        return Ok(ExitCode::NoResults);
    }

    for file in &files {
        let display_path = path_rewriter.rewrite(&file.path);
        let line = color.format_file_path(&display_path);
        if writer.write_line(&line).is_err() {
            // Broken pipe (SIGPIPE) — exit silently
            break;
        }
    }
    let _ = writer.finish();

    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexrs_core::SegmentManager;
    use indexrs_core::segment::InputFile;
    use std::path::Path;

    /// Build an index with test files and return the SegmentManager.
    fn build_test_index(dir: &Path) -> SegmentManager {
        let indexrs_dir = dir.join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        let manager = SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {}\n".to_vec(),
                    mtime: 200,
                },
                InputFile {
                    path: "src/lib.rs".to_string(),
                    content: b"pub fn hello() {}\n".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "README.md".to_string(),
                    content: b"# Hello\n".to_vec(),
                    mtime: 300,
                },
                InputFile {
                    path: "tests/test.py".to_string(),
                    content: b"def test(): pass\n".to_vec(),
                    mtime: 150,
                },
            ])
            .unwrap();
        manager
    }

    #[test]
    fn test_list_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let files = collect_files(&snapshot, &FilesFilter::default()).unwrap();
        assert_eq!(files.len(), 4);
    }

    #[test]
    fn test_list_files_filter_language() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let filter = FilesFilter {
            language: Some("rust".to_string()),
            ..Default::default()
        };
        let files = collect_files(&snapshot, &filter).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.path.ends_with(".rs")));
    }

    #[test]
    fn test_list_files_filter_path_glob() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let filter = FilesFilter {
            path_glob: Some("src/*".to_string()),
            ..Default::default()
        };
        let files = collect_files(&snapshot, &filter).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.path.starts_with("src/")));
    }

    #[test]
    fn test_list_files_sort_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let files = collect_files(&snapshot, &FilesFilter::default()).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn test_list_files_sort_by_modified() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let filter = FilesFilter {
            sort: SortOrder::Modified,
            ..Default::default()
        };
        let files = collect_files(&snapshot, &filter).unwrap();
        // Most recent first
        assert!(files[0].mtime_epoch_secs >= files[1].mtime_epoch_secs);
    }

    #[test]
    fn test_list_files_with_limit() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let filter = FilesFilter {
            limit: Some(2),
            ..Default::default()
        };
        let files = collect_files(&snapshot, &filter).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_run_files_rewrites_paths_to_cwd_relative() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let mut buf = Vec::new();
        let color = ColorConfig::new(false);
        let rewriter = PathRewriter::new(Path::new("/repo"), Path::new("/repo/src"));

        let exit = {
            let mut writer = StreamingWriter::new(&mut buf);
            run_files(
                &snapshot,
                &FilesFilter::default(),
                &color,
                &rewriter,
                &mut writer,
            )
            .unwrap()
        };
        let output = String::from_utf8(buf).unwrap();

        // "src/main.rs" should become "main.rs"
        assert!(
            output.contains("main.rs\n"),
            "expected rewritten src/main.rs -> main.rs, got: {output}"
        );
        // "README.md" should become "../README.md"
        assert!(
            output.contains("../README.md\n"),
            "expected ../README.md, got: {output}"
        );
        assert!(matches!(exit, ExitCode::Success));
    }
}
