use serde::{Deserialize, Serialize};

/// Request from CLI client to daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRequest {
    Search {
        query: String,
        regex: bool,
        case_sensitive: bool,
        ignore_case: bool,
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
    Ping,
    Shutdown,
    Reindex,
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
