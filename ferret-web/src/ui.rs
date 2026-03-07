use std::fmt::Write;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use ferret_indexer_core::highlight::{Token, TokenKind};
use ferret_indexer_core::search::FileMatch;
use serde::Deserialize;

use crate::AppState;

// ---------------------------------------------------------------------------
// Template types
// ---------------------------------------------------------------------------

/// A repo entry for the sidebar.
pub struct RepoItem {
    pub name: String,
    pub path: String,
    pub status: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    repos: Vec<RepoItem>,
    selected_repo: String,
    repo_count: usize,
}

#[derive(Template)]
#[template(path = "search_results.html")]
struct SearchResultsTemplate {
    files: Vec<FileMatch>,
    repo: String,
    query: String,
    query_encoded: String,
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

    /// Produce HTML with both syntax highlighting and match `<mark>` tags.
    fn highlight_line_with_tokens(
        content: &str,
        tokens: &[Token],
        ranges: &[(usize, usize)],
    ) -> String {
        tokenize_html_with_marks(content, tokens, ranges)
    }

    /// Produce HTML with syntax highlighting for a context line.
    fn highlight_context(content: &str, tokens: &[Token]) -> String {
        tokenize_html(content, tokens)
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
    pub highlights_bytes: String,
    pub temporary: bool,
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
    pub highlights_bytes: String,
    pub highlights_pct: String,
    pub has_breakdown: bool,
    pub segment_details: Vec<SegmentDetailItem>,
    pub temp_bytes: String,
    pub has_temp_bytes: bool,
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

/// Percent-encode a string for use in URL query values (RFC 3986 unreserved chars).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                write!(out, "%{byte:02X}").unwrap();
            }
        }
    }
    out
}

/// Map a `TokenKind` to its CSS class suffix, or `None` for kinds that inherit
/// the default foreground color (Plain, Variable, Operator, Punctuation, Label, Other).
fn token_css_class(kind: TokenKind) -> Option<&'static str> {
    match kind {
        TokenKind::Keyword => Some("tok-keyword"),
        TokenKind::String => Some("tok-string"),
        TokenKind::Comment => Some("tok-comment"),
        TokenKind::Number => Some("tok-number"),
        TokenKind::Function => Some("tok-function"),
        TokenKind::Type => Some("tok-type"),
        TokenKind::Macro => Some("tok-macro"),
        TokenKind::Attribute => Some("tok-attribute"),
        TokenKind::Constant => Some("tok-constant"),
        TokenKind::Module => Some("tok-module"),
        _ => None,
    }
}

/// Render a line's content with syntax-highlight `<span>` wrappers.
/// Falls back to plain `html_escape` when tokens are empty.
fn tokenize_html(content: &str, tokens: &[Token]) -> String {
    if tokens.is_empty() {
        return html_escape(content);
    }
    let mut out = String::with_capacity(content.len() * 2);
    let mut pos = 0;
    for tok in tokens {
        let end = (pos + tok.len).min(content.len());
        let slice = &content[pos..end];
        if let Some(cls) = token_css_class(tok.kind) {
            out.push_str("<span class=\"");
            out.push_str(cls);
            out.push_str("\">");
            out.push_str(&html_escape(slice));
            out.push_str("</span>");
        } else {
            out.push_str(&html_escape(slice));
        }
        pos = end;
    }
    // Remainder (if tokens don't cover the full line)
    if pos < content.len() {
        out.push_str(&html_escape(&content[pos..]));
    }
    out
}

/// Render a line with both syntax-highlight spans and `<mark>` tags for search
/// match ranges.  Match `<mark>` tags nest inside syntax spans when they overlap.
/// Falls back to `highlight_line()` when tokens are empty.
fn tokenize_html_with_marks(content: &str, tokens: &[Token], ranges: &[(usize, usize)]) -> String {
    if tokens.is_empty() {
        return SearchResultsTemplate::highlight_line(content, ranges);
    }
    if ranges.is_empty() {
        return tokenize_html(content, tokens);
    }

    let mut out = String::with_capacity(content.len() * 3);
    let mut tok_pos: usize = 0; // byte offset tracking token boundaries
    let mut range_idx = 0;

    for tok in tokens {
        let tok_start = tok_pos;
        let tok_end = (tok_pos + tok.len).min(content.len());
        tok_pos = tok_end;

        let cls = token_css_class(tok.kind);

        // We need to emit the token span, interleaving <mark> tags for any
        // overlapping match ranges.
        if let Some(c) = cls {
            out.push_str("<span class=\"");
            out.push_str(c);
            out.push_str("\">");
        }

        let mut cursor = tok_start;
        while cursor < tok_end {
            // Advance past ranges that end before cursor
            while range_idx < ranges.len() && ranges[range_idx].1 <= cursor {
                range_idx += 1;
            }

            if range_idx < ranges.len() {
                let (rs, re) = ranges[range_idx];
                let rs = rs.min(content.len());
                let re = re.min(content.len());

                if rs > cursor {
                    // Gap before next match range (within this token)
                    let gap_end = rs.min(tok_end);
                    out.push_str(&html_escape(&content[cursor..gap_end]));
                    cursor = gap_end;
                } else {
                    // We're inside a match range
                    let mark_end = re.min(tok_end);
                    out.push_str("<mark>");
                    out.push_str(&html_escape(&content[cursor..mark_end]));
                    out.push_str("</mark>");
                    cursor = mark_end;
                }
            } else {
                // No more ranges — emit rest of token
                out.push_str(&html_escape(&content[cursor..tok_end]));
                cursor = tok_end;
            }
        }

        if cls.is_some() {
            out.push_str("</span>");
        }
    }

    // Remainder beyond tokens
    if tok_pos < content.len() {
        out.push_str(&html_escape(&content[tok_pos..]));
    }

    out
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
) -> Result<(Vec<FileMatch>, ferret_indexer_daemon::SearchStats), String> {
    let stream = ferret_indexer_daemon::ensure_daemon(daemon_bin, repo_path, false)
        .await
        .map_err(|e| format!("daemon connect: {e}"))?;

    let request = ferret_indexer_daemon::types::DaemonRequest::JsonSearch {
        query: query.to_string(),
        page,
        per_page,
        context_lines: 2,
        language: None,
        path_glob: None,
    };

    let result = ferret_indexer_daemon::send_json_request(stream, &request)
        .await
        .map_err(|e| format!("daemon request: {e}"))?;

    let mut files = Vec::new();
    let mut stats = None;

    for payload in &result.payloads {
        if let Ok(frame) = serde_json::from_str::<ferret_indexer_daemon::JsonSearchFrame>(payload) {
            match frame {
                ferret_indexer_daemon::JsonSearchFrame::Result { file } => files.push(file),
                ferret_indexer_daemon::JsonSearchFrame::Stats { stats: s } => stats = Some(s),
            }
        }
    }

    let stats = stats.unwrap_or(ferret_indexer_daemon::SearchStats {
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
) -> Result<ferret_indexer_daemon::FileResponse, String> {
    let stream = ferret_indexer_daemon::ensure_daemon(daemon_bin, repo_path, false)
        .await
        .map_err(|e| format!("daemon connect: {e}"))?;

    let request = ferret_indexer_daemon::types::DaemonRequest::GetFile {
        path: file_path.to_string(),
        line_start: None,
        line_end: None,
    };

    let result = ferret_indexer_daemon::send_json_request(stream, &request)
        .await
        .map_err(|e| format!("daemon request: {e}"))?;

    let payload = result
        .payloads
        .first()
        .ok_or_else(|| "no response from daemon".to_string())?;

    serde_json::from_str::<ferret_indexer_daemon::FileResponse>(payload)
        .map_err(|e| format!("parse file response: {e}"))
}

async fn proxy_status_raw(
    daemon_bin: &std::path::Path,
    repo_path: &std::path::Path,
) -> Result<ferret_indexer_daemon::StatusResponse, String> {
    let stream = ferret_indexer_daemon::ensure_daemon(daemon_bin, repo_path, false)
        .await
        .map_err(|e| format!("daemon connect: {e}"))?;

    let request = ferret_indexer_daemon::types::DaemonRequest::Status;

    let result = ferret_indexer_daemon::send_json_request(stream, &request)
        .await
        .map_err(|e| format!("daemon request: {e}"))?;

    if let Some(payload) = result.payloads.first()
        && let Ok(status) = serde_json::from_str::<ferret_indexer_daemon::StatusResponse>(payload)
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

    // Build repo items with status for each repo
    let mut repos = Vec::with_capacity(repo_names.len());
    for name in &repo_names {
        let path = &repos_map[name];
        let status = proxy_status(state.daemon_bin(), path)
            .await
            .unwrap_or_else(|_| "offline".to_string());
        repos.push(RepoItem {
            name: name.clone(),
            path: path.display().to_string(),
            status,
        });
    }
    let repo_count = repos.len();

    render_template(IndexTemplate {
        repos,
        selected_repo,
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
            query_encoded: urlencode(&query),
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
                query_encoded: urlencode(&query),
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
        Ok((files, stats)) => {
            let query_encoded = urlencode(&query);
            render_template(SearchResultsTemplate {
                files,
                repo,
                query_encoded,
                query,
                total_matches: stats.total_matches,
                files_matched: stats.files_matched,
                duration_ms: stats.duration_ms,
                page: stats.page,
                total_pages: stats.total_pages,
                has_next: stats.has_next,
            })
        }
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
                .iter()
                .zip(
                    file_resp
                        .highlight_tokens
                        .iter()
                        .map(Some)
                        .chain(std::iter::repeat(None)),
                )
                .enumerate()
                .map(|(i, (content, tokens))| {
                    let html = tokenize_html(content, tokens.map_or(&[], |t| t.as_slice()));
                    (i + 1, html)
                })
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

    let stream = match ferret_indexer_daemon::ensure_daemon(state.daemon_bin(), &repo_path, false)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("daemon connect error: {e}");
            return (StatusCode::BAD_GATEWAY, format!("Daemon unavailable: {e}")).into_response();
        }
    };

    let request = ferret_indexer_daemon::types::DaemonRequest::JsonSymbols {
        query: Some(query.clone()),
        kind,
        language: None,
        path_filter: None,
        max_results: Some(50),
        offset: None,
    };

    match ferret_indexer_daemon::send_json_request(stream, &request).await {
        Ok(result) => {
            let mut symbols = Vec::new();
            let mut total = 0usize;
            let mut duration_ms = result.duration_ms;

            for payload in &result.payloads {
                if let Ok(frame) =
                    serde_json::from_str::<ferret_indexer_daemon::JsonSymbolsFrame>(payload)
                {
                    match frame {
                        ferret_indexer_daemon::JsonSymbolsFrame::Symbol(m) => {
                            symbols.push(SymbolItem {
                                name: m.name,
                                kind: m.kind,
                                path: m.path,
                                line: m.line,
                                score: m.score,
                            });
                        }
                        ferret_indexer_daemon::JsonSymbolsFrame::Stats { stats } => {
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
            highlights_bytes_raw,
            segment_details_raw,
            lang_extensions,
            temp_bytes_raw,
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
                sr.highlights_bytes,
                sr.segment_details,
                sr.language_extensions,
                sr.temp_bytes,
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
                0,
                vec![],
                vec![],
                0,
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
            + symbols_bytes_raw
            + highlights_bytes_raw;
        let (content_pct, trigrams_pct, meta_pct, tombstones_pct, symbols_pct, highlights_pct) =
            if total_breakdown > 0 {
                let c = content_bytes_raw as f64 / total_breakdown as f64 * 100.0;
                let t = trigrams_bytes_raw as f64 / total_breakdown as f64 * 100.0;
                let m = meta_paths_bytes_raw as f64 / total_breakdown as f64 * 100.0;
                let tb = tombstones_bytes_raw as f64 / total_breakdown as f64 * 100.0;
                let h = highlights_bytes_raw as f64 / total_breakdown as f64 * 100.0;
                let s = 100.0 - c - t - m - tb - h;
                (
                    format!("{c:.1}"),
                    format!("{t:.1}"),
                    format!("{m:.1}"),
                    format!("{tb:.1}"),
                    format!("{s:.1}"),
                    format!("{h:.1}"),
                )
            } else {
                (
                    "0".into(),
                    "0".into(),
                    "0".into(),
                    "0".into(),
                    "0".into(),
                    "0".into(),
                )
            };

        let segment_details: Vec<SegmentDetailItem> = segment_details_raw
            .into_iter()
            .map(|s| {
                let total = s.trigrams_bytes
                    + s.meta_paths_bytes
                    + s.content_bytes
                    + s.tombstones_bytes
                    + s.symbols_bytes
                    + s.sym_trigrams_bytes
                    + s.highlights_bytes;
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
                    highlights_bytes: format_bytes(s.highlights_bytes),
                    temporary: s.temporary,
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
            highlights_bytes: format_bytes(highlights_bytes_raw),
            highlights_pct,
            has_breakdown: total_breakdown > 0,
            segment_details,
            temp_bytes: format_bytes(temp_bytes_raw),
            has_temp_bytes: temp_bytes_raw > 0,
        });
    }

    let repo_count = repos.len();
    render_template(ReposTemplate { repos, repo_count })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_indexer_core::highlight::{Token, TokenKind};

    // -- html_escape ---------------------------------------------------------

    #[test]
    fn html_escape_plain_text() {
        assert_eq!(html_escape("hello world"), "hello world");
    }

    #[test]
    fn html_escape_all_special_chars() {
        assert_eq!(
            html_escape(r#"<div class="a&b">"#),
            "&lt;div class=&quot;a&amp;b&quot;&gt;"
        );
    }

    #[test]
    fn html_escape_empty() {
        assert_eq!(html_escape(""), "");
    }

    // -- urlencode -----------------------------------------------------------

    #[test]
    fn urlencode_passthrough() {
        assert_eq!(urlencode("hello-world_1.0~beta"), "hello-world_1.0~beta");
    }

    #[test]
    fn urlencode_spaces_and_special() {
        assert_eq!(urlencode("fn main()"), "fn%20main%28%29");
    }

    #[test]
    fn urlencode_empty() {
        assert_eq!(urlencode(""), "");
    }

    #[test]
    fn urlencode_all_encoded() {
        assert_eq!(urlencode("a b+c"), "a%20b%2Bc");
    }

    // -- format_bytes --------------------------------------------------------

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_below_kb() {
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn format_bytes_exactly_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
    }

    #[test]
    fn format_bytes_kb_range() {
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    #[test]
    fn format_bytes_exactly_mb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn format_bytes_mb_range() {
        assert_eq!(format_bytes(2_621_440), "2.5 MB");
    }

    #[test]
    fn format_bytes_exactly_gb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn format_bytes_gb_range() {
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    // -- format_relative_time ------------------------------------------------

    #[test]
    fn format_relative_time_zero_is_never() {
        assert_eq!(format_relative_time(0), "never");
    }

    #[test]
    fn format_relative_time_future_is_just_now() {
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 9999;
        assert_eq!(format_relative_time(future), "just now");
    }

    #[test]
    fn format_relative_time_seconds_ago() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = format_relative_time(now - 30);
        assert!(result.ends_with("s ago"), "expected seconds, got: {result}");
    }

    #[test]
    fn format_relative_time_minutes_ago() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = format_relative_time(now - 300);
        assert!(result.ends_with("m ago"), "expected minutes, got: {result}");
    }

    #[test]
    fn format_relative_time_hours_ago() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = format_relative_time(now - 7200);
        assert!(result.ends_with("h ago"), "expected hours, got: {result}");
    }

    #[test]
    fn format_relative_time_days_ago() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = format_relative_time(now - 172800);
        assert!(result.ends_with("d ago"), "expected days, got: {result}");
    }

    // -- token_css_class -----------------------------------------------------

    #[test]
    fn token_css_class_keyword() {
        assert_eq!(token_css_class(TokenKind::Keyword), Some("tok-keyword"));
    }

    #[test]
    fn token_css_class_plain_returns_none() {
        assert_eq!(token_css_class(TokenKind::Plain), None);
    }

    #[test]
    fn token_css_class_variable_returns_none() {
        assert_eq!(token_css_class(TokenKind::Variable), None);
    }

    #[test]
    fn token_css_class_all_styled_kinds() {
        let cases = [
            (TokenKind::Keyword, "tok-keyword"),
            (TokenKind::String, "tok-string"),
            (TokenKind::Comment, "tok-comment"),
            (TokenKind::Number, "tok-number"),
            (TokenKind::Function, "tok-function"),
            (TokenKind::Type, "tok-type"),
            (TokenKind::Macro, "tok-macro"),
            (TokenKind::Attribute, "tok-attribute"),
            (TokenKind::Constant, "tok-constant"),
            (TokenKind::Module, "tok-module"),
        ];
        for (kind, expected) in cases {
            assert_eq!(token_css_class(kind), Some(expected), "failed for {kind:?}");
        }
    }

    // -- highlight_line (SearchResultsTemplate) ------------------------------

    #[test]
    fn highlight_line_no_ranges() {
        let result = SearchResultsTemplate::highlight_line("hello world", &[]);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn highlight_line_single_range() {
        let result = SearchResultsTemplate::highlight_line("hello world", &[(6, 11)]);
        assert_eq!(result, "hello <mark>world</mark>");
    }

    #[test]
    fn highlight_line_multiple_ranges() {
        let result = SearchResultsTemplate::highlight_line("abcdef", &[(0, 2), (4, 6)]);
        assert_eq!(result, "<mark>ab</mark>cd<mark>ef</mark>");
    }

    #[test]
    fn highlight_line_adjacent_ranges() {
        let result = SearchResultsTemplate::highlight_line("abcd", &[(0, 2), (2, 4)]);
        assert_eq!(result, "<mark>ab</mark><mark>cd</mark>");
    }

    #[test]
    fn highlight_line_escapes_html_in_content() {
        let result = SearchResultsTemplate::highlight_line("<b>hi</b>", &[(3, 5)]);
        assert_eq!(result, "&lt;b&gt;<mark>hi</mark>&lt;/b&gt;");
    }

    #[test]
    fn highlight_line_range_clamped_to_content_length() {
        let result = SearchResultsTemplate::highlight_line("abc", &[(1, 100)]);
        assert_eq!(result, "a<mark>bc</mark>");
    }

    // -- tokenize_html -------------------------------------------------------

    fn tok(len: usize, kind: TokenKind) -> Token {
        Token { len, kind }
    }

    #[test]
    fn tokenize_html_empty_tokens_falls_back() {
        assert_eq!(tokenize_html("<b>", &[]), "&lt;b&gt;");
    }

    #[test]
    fn tokenize_html_single_keyword() {
        let tokens = [tok(2, TokenKind::Keyword)];
        let result = tokenize_html("fn", &tokens);
        assert_eq!(result, "<span class=\"tok-keyword\">fn</span>");
    }

    #[test]
    fn tokenize_html_plain_token_no_span() {
        let tokens = [tok(5, TokenKind::Plain)];
        let result = tokenize_html("hello", &tokens);
        assert_eq!(result, "hello");
    }

    #[test]
    fn tokenize_html_mixed_tokens() {
        // "fn main" => keyword "fn", plain " ", function "main"
        let tokens = [
            tok(2, TokenKind::Keyword),
            tok(1, TokenKind::Plain),
            tok(4, TokenKind::Function),
        ];
        let result = tokenize_html("fn main", &tokens);
        assert_eq!(
            result,
            "<span class=\"tok-keyword\">fn</span> <span class=\"tok-function\">main</span>"
        );
    }

    #[test]
    fn tokenize_html_tokens_shorter_than_content() {
        let tokens = [tok(2, TokenKind::Keyword)];
        let result = tokenize_html("fn main", &tokens);
        assert_eq!(result, "<span class=\"tok-keyword\">fn</span> main");
    }

    #[test]
    fn tokenize_html_escapes_within_spans() {
        let tokens = [tok(6, TokenKind::String)];
        let result = tokenize_html("\"a<b>\"", &tokens);
        assert_eq!(
            result,
            "<span class=\"tok-string\">&quot;a&lt;b&gt;&quot;</span>"
        );
    }

    // -- tokenize_html_with_marks --------------------------------------------

    #[test]
    fn tokenize_with_marks_no_tokens_delegates() {
        let result = tokenize_html_with_marks("hello world", &[], &[(0, 5)]);
        assert_eq!(result, "<mark>hello</mark> world");
    }

    #[test]
    fn tokenize_with_marks_no_ranges_delegates() {
        let tokens = [tok(5, TokenKind::Keyword)];
        let result = tokenize_html_with_marks("hello", &tokens, &[]);
        assert_eq!(result, "<span class=\"tok-keyword\">hello</span>");
    }

    #[test]
    fn tokenize_with_marks_range_within_single_token() {
        // "fn main()" with keyword "fn", plain " ", function "main", punct "()"
        let tokens = [
            tok(2, TokenKind::Keyword),
            tok(1, TokenKind::Plain),
            tok(4, TokenKind::Function),
            tok(2, TokenKind::Punctuation),
        ];
        // Mark "main"
        let result = tokenize_html_with_marks("fn main()", &tokens, &[(3, 7)]);
        assert_eq!(
            result,
            "<span class=\"tok-keyword\">fn</span> <span class=\"tok-function\"><mark>main</mark></span>()"
        );
    }

    #[test]
    fn tokenize_with_marks_range_spanning_tokens() {
        // "let x" with keyword "let", plain " ", variable "x"
        let tokens = [
            tok(3, TokenKind::Keyword),
            tok(1, TokenKind::Plain),
            tok(1, TokenKind::Variable),
        ];
        // Mark spans from "et" through " x" (bytes 1..5)
        let result = tokenize_html_with_marks("let x", &tokens, &[(1, 5)]);
        assert_eq!(
            result,
            "<span class=\"tok-keyword\">l<mark>et</mark></span><mark> </mark><mark>x</mark>"
        );
    }

    #[test]
    fn tokenize_with_marks_html_escaped_in_marks() {
        let tokens = [tok(5, TokenKind::String)];
        let result = tokenize_html_with_marks("a<b>c", &tokens, &[(1, 4)]);
        assert_eq!(
            result,
            "<span class=\"tok-string\">a<mark>&lt;b&gt;</mark>c</span>"
        );
    }
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

    let stream =
        match ferret_indexer_daemon::ensure_daemon(state.daemon_bin(), &repo_path, false).await {
            Ok(s) => s,
            Err(_) => return render_template(SymbolOutlineTemplate { symbols: vec![] }),
        };

    // Empty query + path_filter = return all symbols in file
    let request = ferret_indexer_daemon::types::DaemonRequest::JsonSymbols {
        query: None,
        kind: None,
        language: None,
        path_filter: Some(params.path),
        max_results: Some(500),
        offset: None,
    };

    match ferret_indexer_daemon::send_json_request(stream, &request).await {
        Ok(result) => {
            let mut symbols: Vec<SymbolItem> = Vec::new();
            for payload in &result.payloads {
                if let Ok(ferret_indexer_daemon::JsonSymbolsFrame::Symbol(m)) =
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
