//! MCP resource and resource template handlers for indexrs.
//!
//! Exposes indexed data as MCP resources via `indexrs://` URIs:
//!
//! - `indexrs://status` -- overall index status
//! - `indexrs://repo/{repo}/file/{+path}` -- file contents from the index
//! - `indexrs://repo/{repo}/tree` -- directory tree listing
//! - `indexrs://repo/{repo}/status` -- per-repo detailed status

use std::collections::HashSet;
use std::sync::Arc;

use rmcp::model::{
    AnnotateAble, ListResourceTemplatesResult, ListResourcesResult, RawResource,
    RawResourceTemplate, ReadResourceResult, ResourceContents,
};

use indexrs_core::{IndexState, Language, Segment};

use crate::formatter::format_size;

/// Return the list of static resources.
pub fn list_resources() -> ListResourcesResult {
    ListResourcesResult {
        resources: vec![
            RawResource {
                uri: "indexrs://status".to_string(),
                name: "Index Status".to_string(),
                description: Some("Overall indexrs index status".to_string()),
                mime_type: Some("text/plain".to_string()),
                size: None,
                title: None,
                icons: None,
                meta: None,
            }
            .no_annotation(),
        ],
        next_cursor: None,
        meta: None,
    }
}

/// Return the list of resource templates.
pub fn list_resource_templates() -> ListResourceTemplatesResult {
    ListResourceTemplatesResult {
        resource_templates: vec![
            RawResourceTemplate {
                uri_template: "indexrs://repo/{repo}/file/{+path}".to_string(),
                name: "Indexed File".to_string(),
                description: Some("Contents of a file as stored in the index".to_string()),
                mime_type: Some("text/plain".to_string()),
                title: None,
                icons: None,
            }
            .no_annotation(),
            RawResourceTemplate {
                uri_template: "indexrs://repo/{repo}/tree".to_string(),
                name: "Repository Tree".to_string(),
                description: Some(
                    "Directory tree listing of all indexed files in a repository".to_string(),
                ),
                mime_type: Some("text/plain".to_string()),
                title: None,
                icons: None,
            }
            .no_annotation(),
            RawResourceTemplate {
                uri_template: "indexrs://repo/{repo}/status".to_string(),
                name: "Repository Status".to_string(),
                description: Some("Detailed indexing status for a specific repository".to_string()),
                mime_type: Some("text/plain".to_string()),
                title: None,
                icons: None,
            }
            .no_annotation(),
        ],
        next_cursor: None,
        meta: None,
    }
}

/// Parsed resource URI variants.
#[derive(Debug, PartialEq)]
pub enum ResourceUri {
    Status,
    RepoFile { repo: String, path: String },
    RepoTree { repo: String },
    RepoStatus { repo: String },
}

/// Parse an `indexrs://` URI into a structured variant.
///
/// Returns `None` for unrecognized or malformed URIs.
pub fn parse_uri(uri: &str) -> Option<ResourceUri> {
    let rest = uri.strip_prefix("indexrs://")?;

    if rest == "status" {
        return Some(ResourceUri::Status);
    }

    let rest = rest.strip_prefix("repo/")?;

    // Extract repo name (first path segment after "repo/")
    let slash_pos = rest.find('/')?;
    let repo = &rest[..slash_pos];
    let after_repo = &rest[slash_pos + 1..];

    if after_repo == "tree" {
        return Some(ResourceUri::RepoTree {
            repo: repo.to_string(),
        });
    }

    if after_repo == "status" {
        return Some(ResourceUri::RepoStatus {
            repo: repo.to_string(),
        });
    }

    if let Some(path) = after_repo.strip_prefix("file/")
        && !path.is_empty()
    {
        return Some(ResourceUri::RepoFile {
            repo: repo.to_string(),
            path: path.to_string(),
        });
    }

    None
}

/// Dispatch a `read_resource` request to the appropriate handler.
pub fn read_resource(
    state: &Arc<IndexState>,
    uri: &str,
) -> Result<ReadResourceResult, rmcp::ErrorData> {
    let parsed = parse_uri(uri).ok_or_else(|| {
        rmcp::ErrorData::invalid_params(format!("Invalid resource URI: {uri}"), None)
    })?;

    let content = match parsed {
        ResourceUri::Status => read_status(state),
        ResourceUri::RepoFile { path, .. } => read_file(state, &path)?,
        ResourceUri::RepoTree { .. } => read_tree(state),
        ResourceUri::RepoStatus { .. } => read_repo_status(state),
    };

    Ok(ReadResourceResult {
        contents: vec![ResourceContents::TextResourceContents {
            uri: uri.to_string(),
            mime_type: Some("text/plain".to_string()),
            text: content,
            meta: None,
        }],
    })
}

/// Produce an overall index status summary.
fn read_status(state: &Arc<IndexState>) -> String {
    let segments = state.snapshot();
    let mut total_files: u64 = 0;
    let mut total_tombstoned: u64 = 0;
    let mut total_size_bytes: u64 = 0;

    for seg in segments.iter() {
        let tombstones = seg.load_tombstones().unwrap_or_default();
        let reader = seg.metadata_reader();

        for result in reader.iter_all() {
            let meta = match result {
                Ok(m) => m,
                Err(_) => continue,
            };

            if tombstones.contains(meta.file_id) {
                total_tombstoned += 1;
            } else {
                total_files += 1;
                total_size_bytes += meta.size_bytes as u64;
            }
        }
    }

    let mut out = String::new();
    out.push_str("indexrs status: healthy\n\n");
    out.push_str(&format!("Segments: {}\n", segments.len()));
    out.push_str(&format!("Files: {total_files} indexed"));
    if total_tombstoned > 0 {
        out.push_str(&format!(" ({total_tombstoned} tombstoned)"));
    }
    out.push('\n');
    out.push_str(&format!("Total size: {}\n", format_size(total_size_bytes)));
    out
}

/// Read a file's content from the index by path.
///
/// Searches segments newest-first so the most recent version wins.
fn read_file(state: &Arc<IndexState>, path: &str) -> Result<String, rmcp::ErrorData> {
    let segments = state.snapshot();

    // Search segments in reverse order (newest first)
    for seg in segments.iter().rev() {
        let tombstones = seg.load_tombstones().unwrap_or_default();
        let reader = seg.metadata_reader();

        for result in reader.iter_all() {
            let meta = match result {
                Ok(m) => m,
                Err(_) => continue,
            };

            if tombstones.contains(meta.file_id) {
                continue;
            }

            if meta.path == path {
                let content_bytes = seg
                    .content_reader()
                    .read_content(meta.content_offset, meta.content_len)
                    .map_err(|e| {
                        rmcp::ErrorData::internal_error(
                            format!("Failed to read content for {path}: {e}"),
                            None,
                        )
                    })?;

                return String::from_utf8(content_bytes).map_err(|_| {
                    rmcp::ErrorData::internal_error(
                        format!("File {path} contains non-UTF-8 content"),
                        None,
                    )
                });
            }
        }
    }

    Err(rmcp::ErrorData::resource_not_found(
        format!("File not found: {path}"),
        None,
    ))
}

/// Build a sorted directory tree listing from all live files.
fn read_tree(state: &Arc<IndexState>) -> String {
    let segments = state.snapshot();
    let mut paths = collect_live_paths(&segments);
    paths.sort();

    let mut out = String::new();
    out.push_str(&format!("{} files\n", paths.len()));

    for path in &paths {
        let depth = path.matches('/').count();
        let indent = "  ".repeat(depth);
        let name = path.rsplit('/').next().unwrap_or(path);
        out.push_str(&format!("{indent}{name}\n"));
    }

    out
}

/// Produce per-repo status with file count, segment info, and language breakdown.
fn read_repo_status(state: &Arc<IndexState>) -> String {
    let segments = state.snapshot();
    let mut total_files: u64 = 0;
    let mut total_tombstoned: u64 = 0;
    let mut total_size_bytes: u64 = 0;
    let mut lang_counts: std::collections::HashMap<Language, u64> =
        std::collections::HashMap::new();

    for seg in segments.iter() {
        let tombstones = seg.load_tombstones().unwrap_or_default();
        let reader = seg.metadata_reader();

        for result in reader.iter_all() {
            let meta = match result {
                Ok(m) => m,
                Err(_) => continue,
            };

            if tombstones.contains(meta.file_id) {
                total_tombstoned += 1;
                continue;
            }

            total_files += 1;
            total_size_bytes += meta.size_bytes as u64;
            *lang_counts.entry(meta.language).or_insert(0) += 1;
        }
    }

    let mut out = String::new();
    out.push_str(&format!("Segments: {}\n", segments.len()));
    out.push_str(&format!("Files: {total_files} indexed"));
    if total_tombstoned > 0 {
        out.push_str(&format!(" / {total_tombstoned} tombstoned"));
    }
    out.push('\n');
    out.push_str(&format!("Total size: {}\n", format_size(total_size_bytes)));

    if !lang_counts.is_empty() {
        out.push_str("Languages:");
        let mut sorted: Vec<_> = lang_counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let parts: Vec<String> = sorted
            .iter()
            .map(|(lang, count)| format!(" {lang} ({count})"))
            .collect();
        out.push_str(&parts.join(","));
        out.push('\n');
    }

    out
}

/// Collect all non-tombstoned file paths across all segments, deduplicating
/// so that each path appears only once (newest segment wins).
fn collect_live_paths(segments: &[Arc<Segment>]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();

    // Iterate newest-first so we keep the latest version of each path
    for seg in segments.iter().rev() {
        let tombstones = seg.load_tombstones().unwrap_or_default();
        let reader = seg.metadata_reader();

        for result in reader.iter_all() {
            let meta = match result {
                Ok(m) => m,
                Err(_) => continue,
            };

            if tombstones.contains(meta.file_id) {
                continue;
            }

            if seen.insert(meta.path.clone()) {
                paths.push(meta.path);
            }
        }
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- URI parsing tests ----

    #[test]
    fn test_parse_status() {
        assert_eq!(parse_uri("indexrs://status"), Some(ResourceUri::Status));
    }

    #[test]
    fn test_parse_repo_file() {
        assert_eq!(
            parse_uri("indexrs://repo/myproject/file/src/main.rs"),
            Some(ResourceUri::RepoFile {
                repo: "myproject".to_string(),
                path: "src/main.rs".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_repo_file_nested() {
        assert_eq!(
            parse_uri("indexrs://repo/myproject/file/src/deep/nested/file.rs"),
            Some(ResourceUri::RepoFile {
                repo: "myproject".to_string(),
                path: "src/deep/nested/file.rs".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_repo_tree() {
        assert_eq!(
            parse_uri("indexrs://repo/myproject/tree"),
            Some(ResourceUri::RepoTree {
                repo: "myproject".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_repo_status() {
        assert_eq!(
            parse_uri("indexrs://repo/myproject/status"),
            Some(ResourceUri::RepoStatus {
                repo: "myproject".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_invalid_scheme() {
        assert_eq!(parse_uri("http://status"), None);
    }

    #[test]
    fn test_parse_invalid_empty() {
        assert_eq!(parse_uri(""), None);
    }

    #[test]
    fn test_parse_unknown_resource() {
        assert_eq!(parse_uri("indexrs://unknown"), None);
    }

    #[test]
    fn test_parse_repo_no_action() {
        assert_eq!(parse_uri("indexrs://repo/myproject"), None);
    }

    #[test]
    fn test_parse_repo_file_empty_path() {
        assert_eq!(parse_uri("indexrs://repo/myproject/file/"), None);
    }

    #[test]
    fn test_parse_repo_unknown_action() {
        assert_eq!(parse_uri("indexrs://repo/myproject/unknown"), None);
    }

    // ---- read_status tests ----

    #[test]
    fn test_read_status_empty() {
        let state = Arc::new(IndexState::new());
        let content = read_status(&state);
        assert!(content.contains("indexrs status: healthy"));
        assert!(content.contains("Segments: 0"));
        assert!(content.contains("Files: 0 indexed"));
    }

    #[test]
    fn test_read_status_with_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base).unwrap();

        let seg = Arc::new(
            indexrs_core::SegmentWriter::new(&base, indexrs_core::SegmentId(0))
                .build(vec![
                    indexrs_core::InputFile {
                        path: "a.rs".to_string(),
                        content: b"fn a() {}".to_vec(),
                        mtime: 0,
                    },
                    indexrs_core::InputFile {
                        path: "b.rs".to_string(),
                        content: b"fn b() {}".to_vec(),
                        mtime: 0,
                    },
                ])
                .unwrap(),
        );

        let state = Arc::new(IndexState::new());
        state.publish(vec![seg]);

        let content = read_status(&state);
        assert!(content.contains("Segments: 1"));
        assert!(content.contains("Files: 2 indexed"));
    }

    // ---- read_file tests ----

    #[test]
    fn test_read_file_found() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base).unwrap();

        let seg = Arc::new(
            indexrs_core::SegmentWriter::new(&base, indexrs_core::SegmentId(0))
                .build(vec![indexrs_core::InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {}".to_vec(),
                    mtime: 0,
                }])
                .unwrap(),
        );

        let state = Arc::new(IndexState::new());
        state.publish(vec![seg]);

        let result = read_file(&state, "src/main.rs");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "fn main() {}");
    }

    #[test]
    fn test_read_file_not_found() {
        let state = Arc::new(IndexState::new());
        let result = read_file(&state, "nonexistent.rs");
        assert!(result.is_err());
    }

    // ---- read_tree tests ----

    #[test]
    fn test_read_tree() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base).unwrap();

        let seg = Arc::new(
            indexrs_core::SegmentWriter::new(&base, indexrs_core::SegmentId(0))
                .build(vec![
                    indexrs_core::InputFile {
                        path: "src/main.rs".to_string(),
                        content: b"fn main() {}".to_vec(),
                        mtime: 0,
                    },
                    indexrs_core::InputFile {
                        path: "src/lib.rs".to_string(),
                        content: b"pub fn lib() {}".to_vec(),
                        mtime: 0,
                    },
                    indexrs_core::InputFile {
                        path: "Cargo.toml".to_string(),
                        content: b"[package]".to_vec(),
                        mtime: 0,
                    },
                ])
                .unwrap(),
        );

        let state = Arc::new(IndexState::new());
        state.publish(vec![seg]);

        let tree = read_tree(&state);
        assert!(tree.contains("3 files"));
        assert!(tree.contains("Cargo.toml"));
        assert!(tree.contains("main.rs"));
        assert!(tree.contains("lib.rs"));
    }

    #[test]
    fn test_read_tree_empty() {
        let state = Arc::new(IndexState::new());
        let tree = read_tree(&state);
        assert!(tree.contains("0 files"));
    }

    // ---- read_repo_status tests ----

    #[test]
    fn test_read_repo_status() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base).unwrap();

        let seg = Arc::new(
            indexrs_core::SegmentWriter::new(&base, indexrs_core::SegmentId(0))
                .build(vec![
                    indexrs_core::InputFile {
                        path: "main.rs".to_string(),
                        content: b"fn main() {}".to_vec(),
                        mtime: 0,
                    },
                    indexrs_core::InputFile {
                        path: "lib.py".to_string(),
                        content: b"def lib(): pass".to_vec(),
                        mtime: 0,
                    },
                ])
                .unwrap(),
        );

        let state = Arc::new(IndexState::new());
        state.publish(vec![seg]);

        let status = read_repo_status(&state);
        assert!(status.contains("Segments: 1"));
        assert!(status.contains("Files: 2 indexed"));
        assert!(status.contains("Languages:"));
        assert!(status.contains("Rust"));
        assert!(status.contains("Python"));
    }

    // ---- read_resource dispatch integration tests ----

    #[test]
    fn test_read_resource_status() {
        let state = Arc::new(IndexState::new());
        let result = read_resource(&state, "indexrs://status");
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.contents.len(), 1);
        match &result.contents[0] {
            ResourceContents::TextResourceContents { text, uri, .. } => {
                assert_eq!(uri, "indexrs://status");
                assert!(text.contains("indexrs status: healthy"));
            }
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn test_read_resource_invalid_uri() {
        let state = Arc::new(IndexState::new());
        let result = read_resource(&state, "http://invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_read_resource_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base).unwrap();

        let seg = Arc::new(
            indexrs_core::SegmentWriter::new(&base, indexrs_core::SegmentId(0))
                .build(vec![indexrs_core::InputFile {
                    path: "hello.txt".to_string(),
                    content: b"hello world".to_vec(),
                    mtime: 0,
                }])
                .unwrap(),
        );

        let state = Arc::new(IndexState::new());
        state.publish(vec![seg]);

        let result = read_resource(&state, "indexrs://repo/myproject/file/hello.txt");
        assert!(result.is_ok());
        let result = result.unwrap();
        match &result.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => {
                assert_eq!(text, "hello world");
            }
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn test_read_resource_tree() {
        let state = Arc::new(IndexState::new());
        let result = read_resource(&state, "indexrs://repo/myproject/tree");
        assert!(result.is_ok());
        let result = result.unwrap();
        match &result.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => {
                assert!(text.contains("0 files"));
            }
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn test_read_resource_repo_status() {
        let state = Arc::new(IndexState::new());
        let result = read_resource(&state, "indexrs://repo/myproject/status");
        assert!(result.is_ok());
        let result = result.unwrap();
        match &result.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => {
                assert!(text.contains("Segments: 0"));
                assert!(text.contains("Files: 0 indexed"));
            }
            _ => panic!("expected text content"),
        }
    }

    // ---- list_ tests ----

    #[test]
    fn test_list_resources() {
        let result = list_resources();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].uri, "indexrs://status");
    }

    #[test]
    fn test_list_resource_templates() {
        let result = list_resource_templates();
        assert_eq!(result.resource_templates.len(), 3);
        assert_eq!(
            result.resource_templates[0].uri_template,
            "indexrs://repo/{repo}/file/{+path}"
        );
        assert_eq!(
            result.resource_templates[1].uri_template,
            "indexrs://repo/{repo}/tree"
        );
        assert_eq!(
            result.resource_templates[2].uri_template,
            "indexrs://repo/{repo}/status"
        );
    }
}
