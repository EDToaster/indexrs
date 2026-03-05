use std::path::Path;

use serde::de::DeserializeOwned;

use ferret_indexer_daemon::types::DaemonRequest;
use ferret_indexer_daemon::{
    FileResponse, HealthResponse, JsonSearchFrame, JsonSymbolsFrame, SearchStats, StatusResponse,
    SymbolMatchResponse, SymbolsStats, ensure_daemon, send_json_request,
};

use crate::error::ApiError;
use ferret_indexer_core::search::FileMatch;

/// Send a JsonSearch request to the daemon and return matched files + stats.
#[allow(clippy::too_many_arguments)]
pub async fn search(
    daemon_bin: &Path,
    repo_root: &Path,
    query: String,
    page: usize,
    per_page: usize,
    context_lines: usize,
    language: Option<String>,
    path_glob: Option<String>,
) -> Result<(Vec<FileMatch>, SearchStats), ApiError> {
    let request = DaemonRequest::JsonSearch {
        query,
        page,
        per_page,
        context_lines,
        language,
        path_glob,
    };

    let result = send_request(daemon_bin, repo_root, &request).await?;

    let mut files = Vec::new();
    let mut stats = None;

    for payload in result.payloads {
        let frame: JsonSearchFrame = serde_json::from_str(&payload)
            .map_err(|e| ApiError::internal(format!("failed to parse search frame: {e}")))?;
        match frame {
            JsonSearchFrame::Result { file } => files.push(file),
            JsonSearchFrame::Stats { stats: frame_stats } => stats = Some(frame_stats),
        }
    }

    let stats = stats.unwrap_or(SearchStats {
        total_matches: files.len(),
        files_matched: files.len(),
        duration_ms: result.duration_ms,
        page,
        per_page,
        total_pages: 1,
        has_next: false,
    });

    Ok((files, stats))
}

/// Send a JsonSymbols request to the daemon and return matched symbols + stats.
#[allow(clippy::too_many_arguments)]
pub async fn symbols(
    daemon_bin: &Path,
    repo_root: &Path,
    query: Option<String>,
    kind: Option<String>,
    language: Option<String>,
    path_filter: Option<String>,
    max_results: Option<usize>,
    offset: Option<usize>,
) -> Result<(Vec<SymbolMatchResponse>, SymbolsStats), ApiError> {
    let request = DaemonRequest::JsonSymbols {
        query,
        kind,
        language,
        path_filter,
        max_results,
        offset,
    };

    let result = send_request(daemon_bin, repo_root, &request).await?;

    let mut symbols = Vec::new();
    let mut stats = None;

    for payload in result.payloads {
        let frame: JsonSymbolsFrame = serde_json::from_str(&payload)
            .map_err(|e| ApiError::internal(format!("failed to parse symbols frame: {e}")))?;
        match frame {
            JsonSymbolsFrame::Symbol(m) => symbols.push(m),
            JsonSymbolsFrame::Stats { stats: s } => stats = Some(s),
        }
    }

    let stats = stats.unwrap_or(SymbolsStats {
        total: symbols.len(),
        duration_ms: result.duration_ms,
    });

    Ok((symbols, stats))
}

/// Send a GetFile request to the daemon.
pub async fn get_file(
    daemon_bin: &Path,
    repo_root: &Path,
    path: String,
    line_start: Option<usize>,
    line_end: Option<usize>,
) -> Result<FileResponse, ApiError> {
    let request = DaemonRequest::GetFile {
        path,
        line_start,
        line_end,
    };

    let result = send_request(daemon_bin, repo_root, &request).await?;
    parse_first_payload(&result)
}

/// Send a Status request to the daemon.
pub async fn status(daemon_bin: &Path, repo_root: &Path) -> Result<StatusResponse, ApiError> {
    let request = DaemonRequest::Status;
    let result = send_request(daemon_bin, repo_root, &request).await?;
    parse_first_payload(&result)
}

/// Send a Health request to the daemon.
pub async fn daemon_health(
    daemon_bin: &Path,
    repo_root: &Path,
) -> Result<HealthResponse, ApiError> {
    let request = DaemonRequest::Health;
    let result = send_request(daemon_bin, repo_root, &request).await?;
    parse_first_payload(&result)
}

/// Parse the first JSON payload from a daemon result into a typed response.
fn parse_first_payload<T: DeserializeOwned>(
    result: &ferret_indexer_daemon::JsonResult,
) -> Result<T, ApiError> {
    result
        .payloads
        .first()
        .ok_or_else(|| ApiError::internal("no response from daemon"))
        .and_then(|payload| {
            serde_json::from_str(payload)
                .map_err(|e| ApiError::internal(format!("failed to parse daemon response: {e}")))
        })
}

/// Connect to the daemon (spawning if needed) and send a request.
async fn send_request(
    daemon_bin: &Path,
    repo_root: &Path,
    request: &DaemonRequest,
) -> Result<ferret_indexer_daemon::JsonResult, ApiError> {
    let stream = ensure_daemon(daemon_bin, repo_root, false)
        .await
        .map_err(|e| ApiError::service_unavailable(format!("daemon unavailable: {e}")))?;

    send_json_request(stream, request)
        .await
        .map_err(|e| ApiError::internal(format!("daemon request failed: {e}")))
}
