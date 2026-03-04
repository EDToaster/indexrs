use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use indexrs_core::search::FileMatch;
use serde::Deserialize;

use crate::AppState;

// ---------------------------------------------------------------------------
// Template types
// ---------------------------------------------------------------------------

/// A repo entry for the template dropdown.
pub struct RepoItem {
    pub name: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    repos: Vec<RepoItem>,
    selected_repo: String,
    status: String,
    repo_count: usize,
}

#[derive(Template)]
#[template(path = "search_results.html")]
struct SearchResultsTemplate {
    files: Vec<FileMatch>,
    repo: String,
    query: String,
    total_matches: usize,
    files_matched: usize,
    duration_ms: u64,
    page: usize,
    total_pages: usize,
    has_next: bool,
}

impl SearchResultsTemplate {
    /// Produce HTML for a line with highlight <mark> tags around matched ranges.
    fn highlight_line(content: &str, ranges: &[(usize, usize)]) -> String {
        if ranges.is_empty() {
            return html_escape(content);
        }

        let mut out = String::with_capacity(content.len() + ranges.len() * 13);
        let mut pos = 0;
        for &(start, end) in ranges {
            let start = start.min(content.len());
            let end = end.min(content.len());
            if start > pos {
                out.push_str(&html_escape(&content[pos..start]));
            }
            out.push_str("<mark>");
            out.push_str(&html_escape(&content[start..end]));
            out.push_str("</mark>");
            pos = end;
        }
        if pos < content.len() {
            out.push_str(&html_escape(&content[pos..]));
        }
        out
    }
}

/// A symbol result entry for the template.
pub struct SymbolItem {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: u32,
    pub score: f64,
}

#[derive(Template)]
#[template(path = "symbol_results.html")]
struct SymbolResultsTemplate {
    symbols: Vec<SymbolItem>,
    repo: String,
    query: String,
    total: usize,
    duration_ms: u64,
}

#[derive(Deserialize)]
pub struct SymbolSearchParams {
    q: Option<String>,
    #[serde(rename = "repo-select")]
    repo_select: Option<String>,
    kind: Option<String>,
}

/// Per-segment detail for the repos overview template.
pub struct SegmentDetailItem {
    pub name: String,
    pub entry_count: u32,
    pub tombstoned_count: u32,
    pub total_size: String,
    pub trigrams_bytes: String,
    pub content_bytes: String,
    pub meta_paths_bytes: String,
    pub tombstones_bytes: String,
    pub symbols_bytes: String,
}

/// Language info with per-extension breakdown for tooltips.
pub struct LanguageInfo {
    pub name: String,
    pub count: usize,
    pub extensions: Vec<(String, usize)>,
    /// Number of additional extensions not shown (beyond the top 10).
    pub more_extensions: usize,
}

/// A repo entry for the repos overview page.
pub struct RepoOverviewItem {
    pub name: String,
    pub path: String,
    pub status: String,
    pub files_indexed: usize,
    pub segments: usize,
    pub online: bool,
    pub index_bytes: String,
    pub last_indexed: String,
    pub languages: Vec<LanguageInfo>,
    pub tombstone_ratio: f32,
    pub tombstone_pct: String,
    pub needs_compaction: bool,
    pub path_valid: bool,
    pub tombstoned_count: u32,
    pub content_bytes: String,
    pub trigrams_bytes: String,
    pub meta_paths_bytes: String,
    pub content_pct: String,
    pub trigrams_pct: String,
    pub meta_pct: String,
    pub tombstones_bytes: String,
    pub tombstones_pct: String,
    pub symbols_bytes: String,
    pub symbols_pct: String,
    pub has_breakdown: bool,
    pub segment_details: Vec<SegmentDetailItem>,
}

#[derive(Template)]
#[template(path = "repos.html")]
struct ReposTemplate {
    repos: Vec<RepoOverviewItem>,
    repo_count: usize,
}

#[derive(Template)]
#[template(path = "file_preview.html")]
struct FilePreviewTemplate {
    repo: String,
    path: String,
    language: String,
    total_lines: usize,
    lines: Vec<(usize, String)>,
}

#[derive(Template)]
#[template(path = "symbol_outline.html")]
struct SymbolOutlineTemplate {
    symbols: Vec<SymbolItem>,
}

#[derive(Deserialize)]
pub struct OutlineParams {
    repo: String,
    path: String,
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_relative_time(epoch_secs: u64) -> String {
    if epoch_secs == 0 {
        return "never".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if epoch_secs > now {
        return "just now".to_string();
    }
    let delta = now - epoch_secs;
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SearchParams {
    q: Option<String>,
    #[serde(rename = "repo-select")]
    repo_select: Option<String>,
    page: Option<usize>,
    mode: Option<String>,
}

#[derive(Deserialize)]
pub struct RepoStatusParams {
    #[serde(rename = "repo-select")]
    repo_select: Option<String>,
}

// ---------------------------------------------------------------------------
// Daemon proxy helpers (minimal, avoids depending on Agent A's proxy.rs)
// ---------------------------------------------------------------------------

async fn proxy_search(
    daemon_bin: &std::path::Path,
    repo_path: &std::path::Path,
    query: &str,
    page: usize,
    per_page: usize,
) -> Result<(Vec<FileMatch>, indexrs_daemon::SearchStats), String> {
    let stream = indexrs_daemon::ensure_daemon(daemon_bin, repo_path)
        .await
        .map_err(|e| format!("daemon connect: {e}"))?;

    let request = indexrs_daemon::types::DaemonRequest::JsonSearch {
        query: query.to_string(),
        page,
        per_page,
        context_lines: 2,
        language: None,
        path_glob: None,
    };

    let result = indexrs_daemon::send_json_request(stream, &request)
        .await
        .map_err(|e| format!("daemon request: {e}"))?;

    let mut files = Vec::new();
    let mut stats = None;

    for payload in &result.payloads {
        if let Ok(frame) = serde_json::from_str::<indexrs_daemon::JsonSearchFrame>(payload) {
            match frame {
                indexrs_daemon::JsonSearchFrame::Result { file } => files.push(file),
                indexrs_daemon::JsonSearchFrame::Stats { stats: s } => stats = Some(s),
            }
        }
    }

    let stats = stats.unwrap_or(indexrs_daemon::SearchStats {
        total_matches: 0,
        files_matched: 0,
        duration_ms: result.duration_ms,
        page,
        per_page,
        total_pages: 0,
        has_next: false,
    });

    Ok((files, stats))
}

async fn proxy_get_file(
    daemon_bin: &std::path::Path,
    repo_path: &std::path::Path,
    file_path: &str,
) -> Result<indexrs_daemon::FileResponse, String> {
    let stream = indexrs_daemon::ensure_daemon(daemon_bin, repo_path)
        .await
        .map_err(|e| format!("daemon connect: {e}"))?;

    let request = indexrs_daemon::types::DaemonRequest::GetFile {
        path: file_path.to_string(),
        line_start: None,
        line_end: None,
    };

    let result = indexrs_daemon::send_json_request(stream, &request)
        .await
        .map_err(|e| format!("daemon request: {e}"))?;

    let payload = result
        .payloads
        .first()
        .ok_or_else(|| "no response from daemon".to_string())?;

    serde_json::from_str::<indexrs_daemon::FileResponse>(payload)
        .map_err(|e| format!("parse file response: {e}"))
}

async fn proxy_status_raw(
    daemon_bin: &std::path::Path,
    repo_path: &std::path::Path,
) -> Result<indexrs_daemon::StatusResponse, String> {
    let stream = indexrs_daemon::ensure_daemon(daemon_bin, repo_path)
        .await
        .map_err(|e| format!("daemon connect: {e}"))?;

    let request = indexrs_daemon::types::DaemonRequest::Status;

    let result = indexrs_daemon::send_json_request(stream, &request)
        .await
        .map_err(|e| format!("daemon request: {e}"))?;

    if let Some(payload) = result.payloads.first()
        && let Ok(status) = serde_json::from_str::<indexrs_daemon::StatusResponse>(payload)
    {
        return Ok(status);
    }

    Err("no valid status response".to_string())
}

async fn proxy_status(
    daemon_bin: &std::path::Path,
    repo_path: &std::path::Path,
) -> Result<String, String> {
    match proxy_status_raw(daemon_bin, repo_path).await {
        Ok(status) => Ok(format!(
            "{} ({} files, {} segments)",
            status.status, status.files_indexed, status.segments
        )),
        Err(_) => Ok("unknown".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Render helpers
// ---------------------------------------------------------------------------

fn render_template<T: Template>(tmpl: T) -> Response {
    match tmpl.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("template render error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template render error").into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET / — main page
pub async fn index(State(state): State<AppState>) -> Response {
    let repos_map = state.repos().await;
    let mut repo_names: Vec<String> = repos_map.keys().cloned().collect();
    repo_names.sort();

    let selected_repo = repo_names.first().cloned().unwrap_or_default();

    // Get status of selected repo
    let status = if let Some(path) = repos_map.get(&selected_repo) {
        proxy_status(state.daemon_bin(), path)
            .await
            .unwrap_or_else(|_| "offline".to_string())
    } else {
        "no repos".to_string()
    };

    let repos: Vec<RepoItem> = repo_names
        .into_iter()
        .map(|name| RepoItem { name })
        .collect();
    let repo_count = repos.len();

    render_template(IndexTemplate {
        repos,
        selected_repo,
        status,
        repo_count,
    })
}

/// GET /repo-status?repo-select=... — returns status badge text for the selected repo
pub async fn repo_status(
    State(state): State<AppState>,
    Query(params): Query<RepoStatusParams>,
) -> Response {
    let repo = params.repo_select.unwrap_or_default();
    let repos_map = state.repos().await;

    let status = if let Some(path) = repos_map.get(&repo) {
        proxy_status(state.daemon_bin(), path)
            .await
            .unwrap_or_else(|_| "offline".to_string())
    } else {
        "unknown".to_string()
    };

    Html(status).into_response()
}

/// GET /search-results?q=...&repo-select=...&page=1&mode=text|symbol
pub async fn search_results_fragment(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Response {
    if params.mode.as_deref() == Some("symbol") {
        let sym_params = SymbolSearchParams {
            q: params.q,
            repo_select: params.repo_select,
            kind: None,
        };
        return symbol_results_fragment(State(state), Query(sym_params)).await;
    }

    let query = params.q.unwrap_or_default();
    let repo = params.repo_select.unwrap_or_default();
    let page = params.page.unwrap_or(1).max(1);
    let per_page = 20;

    if query.is_empty() {
        return render_template(SearchResultsTemplate {
            files: vec![],
            repo,
            query,
            total_matches: 0,
            files_matched: 0,
            duration_ms: 0,
            page: 1,
            total_pages: 0,
            has_next: false,
        });
    }

    let repos_map = state.repos().await;
    let repo_path = match repos_map.get(&repo) {
        Some(p) => p.clone(),
        None => {
            return render_template(SearchResultsTemplate {
                files: vec![],
                repo,
                query,
                total_matches: 0,
                files_matched: 0,
                duration_ms: 0,
                page: 1,
                total_pages: 0,
                has_next: false,
            });
        }
    };

    match proxy_search(state.daemon_bin(), &repo_path, &query, page, per_page).await {
        Ok((files, stats)) => render_template(SearchResultsTemplate {
            files,
            repo,
            query,
            total_matches: stats.total_matches,
            files_matched: stats.files_matched,
            duration_ms: stats.duration_ms,
            page: stats.page,
            total_pages: stats.total_pages,
            has_next: stats.has_next,
        }),
        Err(e) => {
            tracing::error!("search proxy error: {e}");
            (StatusCode::BAD_GATEWAY, format!("Search failed: {e}")).into_response()
        }
    }
}

/// GET /file/{repo}/{*path}
pub async fn file_preview(
    State(state): State<AppState>,
    Path((repo, file_path)): Path<(String, String)>,
) -> Response {
    let repos_map = state.repos().await;
    let repo_path = match repos_map.get(&repo) {
        Some(p) => p.clone(),
        None => {
            return (StatusCode::NOT_FOUND, "Repository not found").into_response();
        }
    };

    match proxy_get_file(state.daemon_bin(), &repo_path, &file_path).await {
        Ok(file_resp) => {
            let lines: Vec<(usize, String)> = file_resp
                .lines
                .into_iter()
                .enumerate()
                .map(|(i, content)| (i + 1, content))
                .collect();

            render_template(FilePreviewTemplate {
                repo: repo.clone(),
                path: file_resp.path,
                language: file_resp.language,
                total_lines: file_resp.total_lines,
                lines,
            })
        }
        Err(e) => {
            tracing::error!("file preview proxy error: {e}");
            (StatusCode::BAD_GATEWAY, format!("Failed to load file: {e}")).into_response()
        }
    }
}

/// GET /symbol-results?q=...&repo-select=...&kind=...
pub async fn symbol_results_fragment(
    State(state): State<AppState>,
    Query(params): Query<SymbolSearchParams>,
) -> Response {
    let query = params.q.unwrap_or_default();
    let repo = params.repo_select.unwrap_or_default();
    let kind = params.kind;

    if query.is_empty() {
        return render_template(SymbolResultsTemplate {
            symbols: vec![],
            repo,
            query,
            total: 0,
            duration_ms: 0,
        });
    }

    let repos_map = state.repos().await;
    let repo_path = match repos_map.get(&repo) {
        Some(p) => p.clone(),
        None => {
            return render_template(SymbolResultsTemplate {
                symbols: vec![],
                repo,
                query,
                total: 0,
                duration_ms: 0,
            });
        }
    };

    let stream = match indexrs_daemon::ensure_daemon(state.daemon_bin(), &repo_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("daemon connect error: {e}");
            return (StatusCode::BAD_GATEWAY, format!("Daemon unavailable: {e}")).into_response();
        }
    };

    let request = indexrs_daemon::types::DaemonRequest::JsonSymbols {
        query: Some(query.clone()),
        kind,
        language: None,
        path_filter: None,
        max_results: Some(50),
        offset: None,
    };

    match indexrs_daemon::send_json_request(stream, &request).await {
        Ok(result) => {
            let mut symbols = Vec::new();
            let mut total = 0usize;
            let mut duration_ms = result.duration_ms;

            for payload in &result.payloads {
                if let Ok(frame) = serde_json::from_str::<indexrs_daemon::JsonSymbolsFrame>(payload)
                {
                    match frame {
                        indexrs_daemon::JsonSymbolsFrame::Symbol(m) => {
                            symbols.push(SymbolItem {
                                name: m.name,
                                kind: m.kind,
                                path: m.path,
                                line: m.line,
                                score: m.score,
                            });
                        }
                        indexrs_daemon::JsonSymbolsFrame::Stats { stats } => {
                            total = stats.total;
                            duration_ms = stats.duration_ms;
                        }
                    }
                }
            }

            if total == 0 {
                total = symbols.len();
            }

            render_template(SymbolResultsTemplate {
                symbols,
                repo,
                query,
                total,
                duration_ms,
            })
        }
        Err(e) => {
            tracing::error!("symbol search proxy error: {e}");
            (
                StatusCode::BAD_GATEWAY,
                format!("Symbol search failed: {e}"),
            )
                .into_response()
        }
    }
}

/// GET /repos — repo overview page
pub async fn repos_page(State(state): State<AppState>) -> Response {
    let repos_map = state.repos().await;
    let mut repo_names: Vec<String> = repos_map.keys().cloned().collect();
    repo_names.sort();

    let mut repos = Vec::with_capacity(repo_names.len());
    for name in &repo_names {
        let path = repos_map[name].clone();
        let sr_opt = proxy_status_raw(state.daemon_bin(), &path).await.ok();
        let online = sr_opt.is_some();
        let (
            status,
            files_indexed,
            segments,
            index_bytes,
            last_indexed_ts,
            raw_languages,
            tombstone_ratio,
            path_valid,
            tombstoned_count,
            content_bytes_raw,
            trigrams_bytes_raw,
            meta_paths_bytes_raw,
            tombstones_bytes_raw,
            symbols_bytes_raw,
            segment_details_raw,
            lang_extensions,
        ) = match sr_opt {
            Some(sr) => (
                sr.status.clone(),
                sr.files_indexed,
                sr.segments,
                sr.index_bytes,
                sr.last_indexed_ts,
                sr.languages.clone(),
                sr.tombstone_ratio,
                sr.path_valid,
                sr.tombstoned_count,
                sr.content_bytes,
                sr.trigrams_bytes,
                sr.meta_paths_bytes,
                sr.tombstones_bytes,
                sr.symbols_bytes,
                sr.segment_details,
                sr.language_extensions,
            ),
            None => (
                "offline".to_string(),
                0,
                0,
                0,
                0,
                vec![],
                0.0,
                path.is_dir(),
                0,
                0,
                0,
                0,
                0,
                0,
                vec![],
                vec![],
            ),
        };

        // Build LanguageInfo with extension breakdown.
        let ext_map: std::collections::HashMap<String, Vec<(String, usize)>> =
            lang_extensions.into_iter().collect();
        let languages: Vec<LanguageInfo> = raw_languages
            .into_iter()
            .map(|(name, count)| {
                let all_exts = ext_map.get(&name).cloned().unwrap_or_default();
                let more_extensions = all_exts.len().saturating_sub(10);
                let extensions = if all_exts.len() > 10 {
                    all_exts[..10].to_vec()
                } else {
                    all_exts
                };
                LanguageInfo {
                    name,
                    count,
                    extensions,
                    more_extensions,
                }
            })
            .collect();

        let total_breakdown = content_bytes_raw
            + trigrams_bytes_raw
            + meta_paths_bytes_raw
            + tombstones_bytes_raw
            + symbols_bytes_raw;
        let (content_pct, trigrams_pct, meta_pct, tombstones_pct, symbols_pct) =
            if total_breakdown > 0 {
                let c = content_bytes_raw as f64 / total_breakdown as f64 * 100.0;
                let t = trigrams_bytes_raw as f64 / total_breakdown as f64 * 100.0;
                let m = meta_paths_bytes_raw as f64 / total_breakdown as f64 * 100.0;
                let tb = tombstones_bytes_raw as f64 / total_breakdown as f64 * 100.0;
                let s = 100.0 - c - t - m - tb;
                (
                    format!("{c:.1}"),
                    format!("{t:.1}"),
                    format!("{m:.1}"),
                    format!("{tb:.1}"),
                    format!("{s:.1}"),
                )
            } else {
                ("0".into(), "0".into(), "0".into(), "0".into(), "0".into())
            };

        let segment_details: Vec<SegmentDetailItem> = segment_details_raw
            .into_iter()
            .map(|s| {
                let total = s.trigrams_bytes
                    + s.meta_paths_bytes
                    + s.content_bytes
                    + s.tombstones_bytes
                    + s.symbols_bytes
                    + s.sym_trigrams_bytes;
                SegmentDetailItem {
                    name: format!("seg_{:04}", s.id),
                    entry_count: s.entry_count,
                    tombstoned_count: s.tombstoned_count,
                    total_size: format_bytes(total),
                    trigrams_bytes: format_bytes(s.trigrams_bytes),
                    content_bytes: format_bytes(s.content_bytes),
                    meta_paths_bytes: format_bytes(s.meta_paths_bytes),
                    tombstones_bytes: format_bytes(s.tombstones_bytes),
                    symbols_bytes: format_bytes(s.symbols_bytes + s.sym_trigrams_bytes),
                }
            })
            .collect();

        let tombstone_pct = format!("{:.1}%", tombstone_ratio * 100.0);
        let needs_compaction = tombstone_ratio > 0.3;
        repos.push(RepoOverviewItem {
            name: name.clone(),
            path: path.display().to_string(),
            status,
            files_indexed,
            segments,
            online,
            index_bytes: format_bytes(index_bytes),
            last_indexed: format_relative_time(last_indexed_ts),
            languages,
            tombstone_ratio,
            tombstone_pct,
            needs_compaction,
            path_valid,
            tombstoned_count,
            content_bytes: format_bytes(content_bytes_raw),
            trigrams_bytes: format_bytes(trigrams_bytes_raw),
            meta_paths_bytes: format_bytes(meta_paths_bytes_raw),
            content_pct,
            trigrams_pct,
            meta_pct,
            tombstones_bytes: format_bytes(tombstones_bytes_raw),
            tombstones_pct,
            symbols_bytes: format_bytes(symbols_bytes_raw),
            symbols_pct,
            has_breakdown: total_breakdown > 0,
            segment_details,
        });
    }

    let repo_count = repos.len();
    render_template(ReposTemplate { repos, repo_count })
}

/// GET /symbol-outline?repo=...&path=...
pub async fn symbol_outline_fragment(
    State(state): State<AppState>,
    Query(params): Query<OutlineParams>,
) -> Response {
    let repos_map = state.repos().await;
    let repo_path = match repos_map.get(&params.repo) {
        Some(p) => p.clone(),
        None => return render_template(SymbolOutlineTemplate { symbols: vec![] }),
    };

    let stream = match indexrs_daemon::ensure_daemon(state.daemon_bin(), &repo_path).await {
        Ok(s) => s,
        Err(_) => return render_template(SymbolOutlineTemplate { symbols: vec![] }),
    };

    // Empty query + path_filter = return all symbols in file
    let request = indexrs_daemon::types::DaemonRequest::JsonSymbols {
        query: None,
        kind: None,
        language: None,
        path_filter: Some(params.path),
        max_results: Some(500),
        offset: None,
    };

    match indexrs_daemon::send_json_request(stream, &request).await {
        Ok(result) => {
            let mut symbols: Vec<SymbolItem> = Vec::new();
            for payload in &result.payloads {
                if let Ok(indexrs_daemon::JsonSymbolsFrame::Symbol(m)) =
                    serde_json::from_str(payload)
                {
                    symbols.push(SymbolItem {
                        name: m.name,
                        kind: m.kind,
                        path: m.path,
                        line: m.line,
                        score: m.score,
                    });
                }
            }
            // Sort by line number for outline view
            symbols.sort_by_key(|s| s.line);
            render_template(SymbolOutlineTemplate { symbols })
        }
        Err(_) => render_template(SymbolOutlineTemplate { symbols: vec![] }),
    }
}
