use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::ApiError;
use crate::proxy;

// -- Query parameter structs --

#[derive(Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    #[serde(default = "default_page")]
    pub page: usize,
    #[serde(default = "default_per_page")]
    pub per_page: usize,
    #[serde(default = "default_context")]
    pub context: usize,
    #[serde(default)]
    pub stats_only: bool,
    pub language: Option<String>,
    pub path: Option<String>,
}

fn default_page() -> usize {
    1
}
fn default_per_page() -> usize {
    25
}
fn default_context() -> usize {
    2
}

#[derive(Deserialize)]
pub struct FileParams {
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
}

#[derive(Deserialize)]
pub struct SymbolParams {
    pub q: Option<String>,
    pub kind: Option<String>,
    pub language: Option<String>,
    pub path: Option<String>,
    #[serde(default = "default_symbol_limit")]
    pub max_results: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_symbol_limit() -> usize {
    100
}

// -- Response structs --

#[derive(Serialize)]
pub struct SearchResponse {
    pub results: Vec<ferret_indexer_core::search::FileMatch>,
    pub stats: ferret_indexer_daemon::SearchStats,
}

#[derive(Serialize)]
pub struct StatsOnlyResponse {
    pub stats: ferret_indexer_daemon::SearchStats,
}

#[derive(Serialize)]
pub struct SymbolSearchResponse {
    pub symbols: Vec<ferret_indexer_daemon::SymbolMatchResponse>,
    pub stats: ferret_indexer_daemon::SymbolsStats,
}

#[derive(Serialize)]
pub struct RepoListEntry {
    pub name: String,
    pub path: PathBuf,
    pub status: Option<ferret_indexer_daemon::StatusResponse>,
}

#[derive(Deserialize)]
pub struct AddRepoBody {
    pub path: String,
    pub name: Option<String>,
}

#[derive(Serialize)]
pub struct AddRepoResponse {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Serialize)]
pub struct RefreshResponse {
    pub message: String,
}

// -- Handlers --

/// `GET /repos/{name}/search?q=...&page=1&per_page=25&context=2&stats_only=false&language=&path=`
pub async fn search(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<SearchParams>,
) -> Result<impl IntoResponse, ApiError> {
    let repo_path = state
        .repo_path(&name)
        .await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    let query = params
        .q
        .filter(|q| !q.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing or empty 'q' parameter"))?;

    let (results, stats) = proxy::search(
        state.daemon_bin(),
        &repo_path,
        query,
        params.page,
        params.per_page,
        params.context,
        params.language,
        params.path,
    )
    .await?;

    if params.stats_only {
        Ok(Json(serde_json::to_value(StatsOnlyResponse { stats }).unwrap()).into_response())
    } else {
        Ok(Json(SearchResponse { results, stats }).into_response())
    }
}

/// `GET /repos/{name}/symbols?q=...&kind=...&language=...&path=...&max_results=100&offset=0`
pub async fn symbols(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<SymbolParams>,
) -> Result<Json<SymbolSearchResponse>, ApiError> {
    let repo_path = state
        .repo_path(&name)
        .await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    if params.q.is_none() && params.path.is_none() {
        return Err(ApiError::bad_request("'q' or 'path' parameter required"));
    }

    let (symbols, stats) = proxy::symbols(
        state.daemon_bin(),
        &repo_path,
        params.q,
        params.kind,
        params.language,
        params.path,
        Some(params.max_results.min(500)),
        Some(params.offset),
    )
    .await?;

    Ok(Json(SymbolSearchResponse { symbols, stats }))
}

/// `GET /repos/{name}/files/{*path}?line_start=&line_end=`
pub async fn get_file(
    State(state): State<AppState>,
    Path((name, file_path)): Path<(String, String)>,
    Query(params): Query<FileParams>,
) -> Result<Json<ferret_indexer_daemon::FileResponse>, ApiError> {
    let repo_path = state
        .repo_path(&name)
        .await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    let response = proxy::get_file(
        state.daemon_bin(),
        &repo_path,
        file_path,
        params.line_start,
        params.line_end,
    )
    .await?;

    Ok(Json(response))
}

/// `GET /repos/{name}/status`
pub async fn index_status(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ferret_indexer_daemon::StatusResponse>, ApiError> {
    let repo_path = state
        .repo_path(&name)
        .await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    let response = proxy::status(state.daemon_bin(), &repo_path).await?;
    Ok(Json(response))
}

/// `POST /repos/{name}/refresh`
pub async fn refresh_index(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let repo_path = state
        .repo_path(&name)
        .await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    // Connect to daemon and send Reindex request (fire-and-forget).
    let stream = ferret_indexer_daemon::ensure_daemon(state.daemon_bin(), &repo_path, false)
        .await
        .map_err(|e| ApiError::service_unavailable(format!("daemon unavailable: {e}")))?;

    let request = ferret_indexer_daemon::types::DaemonRequest::Reindex { compact: false };
    // Send the request but don't wait for full result processing - just accept.
    let _ = ferret_indexer_daemon::send_json_request(stream, &request).await;

    Ok((
        StatusCode::ACCEPTED,
        Json(RefreshResponse {
            message: format!("reindex started for '{name}'"),
        }),
    ))
}

/// `GET /repos`
pub async fn list_repos(
    State(state): State<AppState>,
) -> Result<Json<Vec<RepoListEntry>>, ApiError> {
    let repos = state.repos().await;
    let mut entries = Vec::with_capacity(repos.len());

    for (name, path) in &repos {
        let status = proxy::status(state.daemon_bin(), path).await.ok();
        entries.push(RepoListEntry {
            name: name.clone(),
            path: path.clone(),
            status,
        });
    }

    Ok(Json(entries))
}

/// `POST /repos` with body `{"path":"...","name":"..."}`
pub async fn add_repo(
    State(state): State<AppState>,
    Json(body): Json<AddRepoBody>,
) -> Result<impl IntoResponse, ApiError> {
    let path = PathBuf::from(&body.path);

    // Validate the path exists and has an .ferret_index directory.
    if !path.is_dir() {
        return Err(ApiError::bad_request(format!(
            "path '{}' does not exist or is not a directory",
            body.path
        )));
    }

    if !path.join(".ferret_index").is_dir() {
        return Err(ApiError::bad_request(format!(
            "path '{}' has no .ferret_index directory; run ferret index first",
            body.path
        )));
    }

    // Determine effective name.
    let name = body.name.clone().unwrap_or_else(|| {
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    });

    // Update persistent registry config.
    let mut config = ferret_indexer_core::registry::load_config()
        .map_err(|e| ApiError::internal(format!("failed to load config: {e}")))?;

    if !ferret_indexer_core::registry::add_repo(&mut config, path.clone(), body.name) {
        return Err(ApiError::bad_request(format!(
            "repository '{name}' or path '{}' already registered",
            body.path
        )));
    }

    ferret_indexer_core::registry::save_config(&config)
        .map_err(|e| ApiError::internal(format!("failed to save config: {e}")))?;

    // Update in-memory state.
    state.add_repo(name.clone(), path.clone()).await;

    Ok((StatusCode::CREATED, Json(AddRepoResponse { name, path })))
}

/// `DELETE /repos/{name}`
pub async fn remove_repo(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    // Check repo exists in memory.
    if state.repo_path(&name).await.is_none() {
        return Err(ApiError::repo_not_found(&name));
    }

    // Update persistent registry config.
    let mut config = ferret_indexer_core::registry::load_config()
        .map_err(|e| ApiError::internal(format!("failed to load config: {e}")))?;

    ferret_indexer_core::registry::remove_repo(&mut config, &name);

    ferret_indexer_core::registry::save_config(&config)
        .map_err(|e| ApiError::internal(format!("failed to save config: {e}")))?;

    // Update in-memory state.
    state.remove_repo(&name).await;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    fn test_app() -> axum::Router {
        let state = AppState::new(HashMap::new(), PathBuf::from("/usr/bin/false"));
        crate::build_router(state)
    }

    #[tokio::test]
    async fn test_health_returns_200() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/v1/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_search_unknown_repo_returns_404() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/v1/repos/nonexistent/search?q=test")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_repos_returns_empty_array() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/v1/repos")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let repos: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(repos.is_empty());
    }

    #[tokio::test]
    async fn test_status_unknown_repo_returns_404() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/v1/repos/nonexistent/status")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_symbols_unknown_repo_returns_404() {
        let app = test_app();
        let req = Request::builder()
            .uri("/api/v1/repos/nonexistent/symbols?q=main")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_delete_unknown_repo_returns_404() {
        let app = test_app();
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/v1/repos/nonexistent")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
