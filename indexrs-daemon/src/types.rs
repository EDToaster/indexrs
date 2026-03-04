use serde::{Deserialize, Serialize};

/// How to handle case sensitivity for a search query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaseMode {
    /// Match case exactly.
    Sensitive,
    /// Ignore case differences.
    Insensitive,
    /// Auto-detect: case-sensitive if query has uppercase, else insensitive.
    Smart,
}

/// Request from CLI client to daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRequest {
    Search {
        query: String,
        regex: bool,
        case_mode: CaseMode,
        limit: usize,
        context_lines: usize,
        language: Option<String>,
        path_glob: Option<String>,
        color: bool,
        cwd: Option<String>,
    },
    QuerySearch {
        query: String,
        limit: usize,
        context_lines: usize,
        color: bool,
        cwd: Option<String>,
    },
    Files {
        language: Option<String>,
        path_glob: Option<String>,
        sort: String,
        limit: Option<usize>,
        color: bool,
        cwd: Option<String>,
    },
    /// Structured search returning JSON-serializable FileMatch objects.
    JsonSearch {
        query: String,
        /// 1-indexed page number.
        page: usize,
        /// Results per page (max 100).
        per_page: usize,
        /// Lines of context above/below each match.
        context_lines: usize,
        /// Optional language filter.
        language: Option<String>,
        /// Optional path glob filter.
        path_glob: Option<String>,
    },
    /// Retrieve file contents with metadata.
    GetFile {
        /// Relative path from repo root.
        path: String,
        /// First line to return (1-indexed, default 1).
        line_start: Option<usize>,
        /// Last line to return (default: EOF).
        line_end: Option<usize>,
    },
    /// Structured index status.
    Status,
    /// Health check with metadata.
    Health,
    /// Symbol search request.
    Symbols {
        query: Option<String>,
        kind: Option<String>,
        language: Option<String>,
        limit: Option<usize>,
        color: bool,
        cwd: Option<String>,
    },
    /// Structured symbol search returning JSON-serializable SymbolMatch objects.
    JsonSymbols {
        query: Option<String>,
        kind: Option<String>,
        language: Option<String>,
        path_filter: Option<String>,
        max_results: Option<usize>,
        offset: Option<usize>,
    },
    Ping,
    Shutdown,
    Reindex {
        /// When true, force compaction after reindex regardless of heuristics.
        #[serde(default)]
        compact: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_search_roundtrip() {
        let req = DaemonRequest::JsonSearch {
            query: "fn main".to_string(),
            page: 1,
            per_page: 20,
            context_lines: 2,
            language: Some("rust".to_string()),
            path_glob: Some("src/**".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: DaemonRequest = serde_json::from_str(&json).unwrap();
        match deserialized {
            DaemonRequest::JsonSearch {
                query,
                page,
                per_page,
                context_lines,
                language,
                path_glob,
            } => {
                assert_eq!(query, "fn main");
                assert_eq!(page, 1);
                assert_eq!(per_page, 20);
                assert_eq!(context_lines, 2);
                assert_eq!(language, Some("rust".to_string()));
                assert_eq!(path_glob, Some("src/**".to_string()));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn test_get_file_roundtrip() {
        let req = DaemonRequest::GetFile {
            path: "src/main.rs".to_string(),
            line_start: Some(10),
            line_end: Some(20),
        };
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: DaemonRequest = serde_json::from_str(&json).unwrap();
        match deserialized {
            DaemonRequest::GetFile {
                path,
                line_start,
                line_end,
            } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(line_start, Some(10));
                assert_eq!(line_end, Some(20));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn test_status_roundtrip() {
        let req = DaemonRequest::Status;
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, DaemonRequest::Status));
    }

    #[test]
    fn test_health_roundtrip() {
        let req = DaemonRequest::Health;
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, DaemonRequest::Health));
    }

    #[test]
    fn test_symbols_roundtrip() {
        let req = DaemonRequest::Symbols {
            query: Some("process".to_string()),
            kind: Some("fn".to_string()),
            language: Some("rust".to_string()),
            limit: Some(50),
            color: true,
            cwd: Some("/repo/src".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: DaemonRequest = serde_json::from_str(&json).unwrap();
        match deserialized {
            DaemonRequest::Symbols {
                query,
                kind,
                language,
                limit,
                color,
                cwd,
            } => {
                assert_eq!(query, Some("process".to_string()));
                assert_eq!(kind, Some("fn".to_string()));
                assert_eq!(language, Some("rust".to_string()));
                assert_eq!(limit, Some(50));
                assert!(color);
                assert_eq!(cwd, Some("/repo/src".to_string()));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn test_json_symbols_roundtrip() {
        let req = DaemonRequest::JsonSymbols {
            query: Some("process".to_string()),
            kind: Some("fn".to_string()),
            language: None,
            path_filter: Some("src/".to_string()),
            max_results: Some(50),
            offset: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: DaemonRequest = serde_json::from_str(&json).unwrap();
        match deserialized {
            DaemonRequest::JsonSymbols {
                query,
                kind,
                language,
                path_filter,
                max_results,
                offset,
            } => {
                assert_eq!(query, Some("process".to_string()));
                assert_eq!(kind, Some("fn".to_string()));
                assert!(language.is_none());
                assert_eq!(path_filter, Some("src/".to_string()));
                assert_eq!(max_results, Some(50));
                assert!(offset.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}

/// Response from daemon to CLI client, sent as TLV binary frames.
#[derive(Debug, PartialEq)]
pub enum DaemonResponse {
    /// A single output line (file path or search match).
    Line { content: String },
    /// End of results with summary.
    Done {
        total: usize,
        duration_ms: u64,
        stale: bool,
    },
    /// Error message.
    Error { message: String },
    /// Ping response.
    Pong,
    /// Progress update (e.g. during reindex).
    Progress { message: String },
    /// JSON-serialized structured data (e.g., FileMatch, SearchStats).
    Json { payload: String },
}
