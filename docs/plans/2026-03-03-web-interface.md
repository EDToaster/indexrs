# Web Interface Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the axum web server and htmx frontend for indexrs that proxies all operations to per-repo daemons over Unix sockets.

**Architecture:** New `indexrs-web` library crate containing an axum server. The web server is a stateless proxy — it reads `repos.toml`, connects to per-repo daemons over Unix sockets, and either returns JSON (API) or renders HTML fragments (htmx UI). Static assets are embedded via `rust-embed`. Templates use `askama`.

**Tech Stack:** axum 0.8, tower-http 0.6, askama 0.13, rust-embed 8, htmx 2.0, tokio 1

**Design Doc:** `docs/design/web-interface.md`

---

## Parallel Agent Assignment

| Agent | Worktree Branch | Focus | Key Files Created |
|-------|----------------|-------|-------------------|
| **Lead** | `main` | Foundation (Task 0), merging, integration (Task 4) | `indexrs-web/Cargo.toml`, `src/lib.rs` |
| **Agent A** | `feat/web-api` | JSON API endpoints + daemon proxy + error types | `src/api.rs`, `src/proxy.rs`, `src/error.rs` |
| **Agent B** | `feat/web-frontend` | Static files + askama templates + UI routes + static serving | `static/*`, `templates/*`, `src/ui.rs`, `src/static_files.rs` |
| **Agent C** | `feat/web-streaming-cli` | SSE endpoints + CLI `web` subcommand | `src/sse.rs`, `indexrs-cli/src/web.rs` |

All `src/` paths above are relative to `indexrs-web/`.

**Execution order:**
1. Lead does **Task 0** (foundation), merges to `main`
2. Agents A, B, C create worktrees from `main`, work on **Tasks 1, 2, 3** in parallel
3. Lead merges branches: A → main, B → main, C → main (resolving `lib.rs` router conflicts)
4. Lead does **Task 4** (integration testing with Playwright)

**Playwright testing:** Each agent compiles and runs the server in their worktree. They use Playwright MCP tools (`browser_navigate`, `browser_snapshot`, `browser_evaluate`) to verify behavior. Agent A tests API responses via `fetch()`. Agent B tests UI rendering and interaction. Agent C tests SSE event delivery.

**Testing prerequisite:** Before Playwright tests, each agent must have a repo with an index. Use `cargo run -p indexrs-cli -- init` in the worktree root (the indexrs repo itself is a good test target since it has Rust files). Agents must also register the repo: `cargo run -p indexrs-cli -- repos add . --name test-repo`.

---

## Task 0: Foundation — Crate Skeleton + Health Endpoint (Lead)

**Goal:** Create `indexrs-web` crate with minimal router and health endpoint. Prove compilation and server startup work.

### Files

- Create: `indexrs-web/Cargo.toml`
- Create: `indexrs-web/src/lib.rs`
- Modify: `Cargo.toml` (root workspace)

### Step 1: Create `indexrs-web/Cargo.toml`

```toml
[package]
name = "indexrs-web"
version = "0.1.0"
edition = "2024"

[dependencies]
axum = "0.8"
tower-http = { version = "0.6", features = ["cors", "compression-gzip"] }
askama = "0.13"
askama_axum = "0.5"
rust-embed = "8"
mime_guess = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["net", "rt-multi-thread", "macros", "signal"] }
indexrs-core = { path = "../indexrs-core" }
indexrs-daemon = { path = "../indexrs-daemon" }
tracing = "0.1"
```

**NOTE:** Verify all dependency versions are latest before implementation. The versions above are approximate. Run `cargo add <dep>` to get latest, or check crates.io. In particular:
- `askama` and `askama_axum` versions must be compatible with each other
- `tower-http` must be compatible with `axum`
- If `askama_axum` doesn't exist as a separate crate in the current ecosystem, use `askama` with `features = ["with-axum"]` instead

### Step 2: Create `indexrs-web/src/lib.rs`

```rust
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use axum::Router;
use serde::Serialize;
use tokio::sync::RwLock;

/// Shared application state passed to all handlers via axum's State extractor.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    /// Map of repo name → absolute path to repo root.
    repos: RwLock<HashMap<String, PathBuf>>,
    /// Path to the indexrs binary (for ensure_daemon).
    daemon_bin: PathBuf,
    /// Server start time (for uptime calculation).
    start_time: Instant,
}

impl AppState {
    pub fn new(repos: HashMap<String, PathBuf>, daemon_bin: PathBuf) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                repos: RwLock::new(repos),
                daemon_bin,
                start_time: Instant::now(),
            }),
        }
    }

    pub async fn repos(&self) -> HashMap<String, PathBuf> {
        self.inner.repos.read().await.clone()
    }

    pub async fn repo_path(&self, name: &str) -> Option<PathBuf> {
        self.inner.repos.read().await.get(name).cloned()
    }

    pub fn daemon_bin(&self) -> &PathBuf {
        &self.inner.daemon_bin
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.inner.start_time.elapsed().as_secs()
    }

    pub async fn add_repo(&self, name: String, path: PathBuf) {
        self.inner.repos.write().await.insert(name, path);
    }

    pub async fn remove_repo(&self, name: &str) -> bool {
        self.inner.repos.write().await.remove(name).is_some()
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    uptime_seconds: u64,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: state.uptime_seconds(),
    })
}

/// Build the full axum router. Agents A, B, C will add routes here.
fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/health", get(health));

    Router::new()
        .nest("/api/v1", api)
        .with_state(state)
}

/// Start the web server on the given port.
pub async fn start_server(
    repos: HashMap<String, PathBuf>,
    daemon_bin: PathBuf,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = AppState::new(repos, daemon_bin);

    // Ensure daemons are running for all registered repos.
    let repos_snapshot = state.repos().await;
    for (name, path) in &repos_snapshot {
        match indexrs_daemon::ensure_daemon(state.daemon_bin(), path).await {
            Ok(_stream) => tracing::info!("daemon ready for repo '{name}'"),
            Err(e) => tracing::warn!("failed to start daemon for repo '{name}': {e}"),
        }
    }

    let app = build_router(state);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    eprintln!("indexrs web interface: http://localhost:{port}");
    if !repos_snapshot.is_empty() {
        let names: Vec<&str> = repos_snapshot.keys().map(|s| s.as_str()).collect();
        eprintln!("  repos: {} ({} repos)", names.join(", "), names.len());
    } else {
        eprintln!("  no repos registered (use POST /api/v1/repos to add)");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
    eprintln!("\nshutting down...");
}
```

### Step 3: Add to workspace

In root `Cargo.toml`, add `"indexrs-web"` to the members list:

```toml
[workspace]
resolver = "3"
members = [
    "indexrs-core",
    "indexrs-cli",
    "indexrs-daemon",
    "indexrs-web",
]
```

### Step 4: Verify compilation

Run: `cargo check -p indexrs-web`
Expected: compiles clean (may need to adjust dependency versions if any don't resolve)

### Step 5: Verify clippy + fmt

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

### Step 6: Commit

```bash
git add indexrs-web/ Cargo.toml
git commit -m "feat(web): create indexrs-web crate with health endpoint"
```

---

## Task 1: JSON API Endpoints + Daemon Proxy (Agent A)

**Goal:** Implement all `/api/v1/` JSON endpoints. The web server proxies requests to per-repo daemons.

### Files

- Create: `indexrs-web/src/error.rs`
- Create: `indexrs-web/src/proxy.rs`
- Create: `indexrs-web/src/api.rs`
- Modify: `indexrs-web/src/lib.rs` (add modules + routes)

### Step 1: Create `indexrs-web/src/error.rs`

This module defines a unified API error type that serializes to the JSON error format from the design doc.

```rust
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Serialize;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code,
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

impl ApiError {
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, code, message: message.into() }
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, code, message: message.into() }
    }

    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self { status: StatusCode::SERVICE_UNAVAILABLE, code: "service_unavailable", message: message.into() }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, code: "internal_error", message: message.into() }
    }

    pub fn repo_not_found(name: &str) -> Self {
        Self::not_found("repo_not_found", format!("Repository '{name}' not found"))
    }
}
```

### Step 2: Create `indexrs-web/src/proxy.rs`

Helper functions that open a connection to a repo's daemon and send typed requests. Each function returns a deserialized response or `ApiError`.

```rust
use std::path::Path;

use indexrs_daemon::types::DaemonRequest;
use indexrs_daemon::{
    FileResponse, HealthResponse, JsonSearchFrame, SearchStats, StatusResponse,
    send_json_request, ensure_daemon,
};

use crate::error::ApiError;

/// Connect to a repo's daemon and send a JsonSearch request.
/// Returns (file_matches_json_frames, stats_frame).
pub async fn search(
    daemon_bin: &Path,
    repo_root: &Path,
    query: &str,
    page: usize,
    per_page: usize,
    context_lines: usize,
    language: Option<String>,
    path_glob: Option<String>,
) -> Result<(Vec<indexrs_core::search::FileMatch>, SearchStats), ApiError> {
    let request = DaemonRequest::JsonSearch {
        query: query.to_string(),
        page,
        per_page,
        context_lines,
        language,
        path_glob,
    };

    let stream = ensure_daemon(daemon_bin, repo_root)
        .await
        .map_err(|e| ApiError::service_unavailable(format!("daemon unavailable: {e}")))?;

    let result = send_json_request(stream, &request)
        .await
        .map_err(|e| ApiError::internal(format!("daemon error: {e}")))?;

    let mut files = Vec::new();
    let mut stats = None;

    for payload in &result.payloads {
        match serde_json::from_str::<JsonSearchFrame>(payload) {
            Ok(JsonSearchFrame::Result { file }) => files.push(file),
            Ok(JsonSearchFrame::Stats { stats: s }) => stats = Some(s),
            Err(e) => tracing::warn!("failed to parse search frame: {e}"),
        }
    }

    let stats = stats.unwrap_or(SearchStats {
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

/// Connect to a repo's daemon and send a GetFile request.
pub async fn get_file(
    daemon_bin: &Path,
    repo_root: &Path,
    path: &str,
    line_start: Option<usize>,
    line_end: Option<usize>,
) -> Result<FileResponse, ApiError> {
    let request = DaemonRequest::GetFile {
        path: path.to_string(),
        line_start,
        line_end,
    };

    let stream = ensure_daemon(daemon_bin, repo_root)
        .await
        .map_err(|e| ApiError::service_unavailable(format!("daemon unavailable: {e}")))?;

    let result = send_json_request(stream, &request)
        .await
        .map_err(|e| ApiError::internal(format!("daemon error: {e}")))?;

    result.payloads.first()
        .ok_or_else(|| ApiError::not_found("file_not_found", format!("File '{path}' not found")))
        .and_then(|p| {
            serde_json::from_str::<FileResponse>(p)
                .map_err(|e| ApiError::internal(format!("invalid response: {e}")))
        })
}

/// Connect to a repo's daemon and send a Status request.
pub async fn status(
    daemon_bin: &Path,
    repo_root: &Path,
) -> Result<StatusResponse, ApiError> {
    let request = DaemonRequest::Status;

    let stream = ensure_daemon(daemon_bin, repo_root)
        .await
        .map_err(|e| ApiError::service_unavailable(format!("daemon unavailable: {e}")))?;

    let result = send_json_request(stream, &request)
        .await
        .map_err(|e| ApiError::internal(format!("daemon error: {e}")))?;

    result.payloads.first()
        .ok_or_else(|| ApiError::internal("no status response"))
        .and_then(|p| {
            serde_json::from_str::<StatusResponse>(p)
                .map_err(|e| ApiError::internal(format!("invalid response: {e}")))
        })
}

/// Connect to a repo's daemon and send a Health request.
pub async fn daemon_health(
    daemon_bin: &Path,
    repo_root: &Path,
) -> Result<HealthResponse, ApiError> {
    let request = DaemonRequest::Health;

    let stream = ensure_daemon(daemon_bin, repo_root)
        .await
        .map_err(|e| ApiError::service_unavailable(format!("daemon unavailable: {e}")))?;

    let result = send_json_request(stream, &request)
        .await
        .map_err(|e| ApiError::internal(format!("daemon error: {e}")))?;

    result.payloads.first()
        .ok_or_else(|| ApiError::internal("no health response"))
        .and_then(|p| {
            serde_json::from_str::<HealthResponse>(p)
                .map_err(|e| ApiError::internal(format!("invalid response: {e}")))
        })
}
```

### Step 3: Create `indexrs-web/src/api.rs`

All JSON API handlers. Each handler extracts parameters, calls proxy functions, and returns JSON.

```rust
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};

use indexrs_core::registry;

use crate::error::ApiError;
use crate::proxy;
use crate::AppState;

// ---------- Search ----------

#[derive(Deserialize)]
pub struct SearchParams {
    q: String,
    #[serde(default = "default_page")]
    page: usize,
    #[serde(default = "default_per_page")]
    per_page: usize,
    #[serde(default = "default_context")]
    context: usize,
    #[serde(default)]
    stats_only: bool,
    language: Option<String>,
    path: Option<String>,
}

fn default_page() -> usize { 1 }
fn default_per_page() -> usize { 25 }
fn default_context() -> usize { 2 }

#[derive(Serialize)]
pub struct SearchResponse {
    stats: StatsBlock,
    #[serde(skip_serializing_if = "Option::is_none")]
    results: Option<Vec<indexrs_core::search::FileMatch>>,
    pagination: PaginationBlock,
}

#[derive(Serialize)]
pub struct StatsBlock {
    total_matches: usize,
    files_matched: usize,
    duration_ms: u64,
}

#[derive(Serialize)]
pub struct PaginationBlock {
    page: usize,
    per_page: usize,
    total_pages: usize,
    has_next: bool,
}

pub async fn search(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let repo_root = state.repo_path(&name).await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    if params.q.is_empty() {
        return Err(ApiError::bad_request("invalid_query", "Query parameter 'q' is required"));
    }

    let per_page = params.per_page.min(100);
    let page = params.page.max(1);

    let (files, stats) = proxy::search(
        state.daemon_bin(),
        &repo_root,
        &params.q,
        page,
        per_page,
        params.context,
        params.language,
        params.path,
    ).await?;

    Ok(Json(SearchResponse {
        stats: StatsBlock {
            total_matches: stats.total_matches,
            files_matched: stats.files_matched,
            duration_ms: stats.duration_ms,
        },
        results: if params.stats_only { None } else { Some(files) },
        pagination: PaginationBlock {
            page: stats.page,
            per_page: stats.per_page,
            total_pages: stats.total_pages,
            has_next: stats.has_next,
        },
    }))
}

// ---------- File Retrieval ----------

#[derive(Deserialize)]
pub struct FileParams {
    line_start: Option<usize>,
    line_end: Option<usize>,
}

pub async fn get_file(
    State(state): State<AppState>,
    Path((name, file_path)): Path<(String, String)>,
    Query(params): Query<FileParams>,
) -> Result<Json<indexrs_daemon::FileResponse>, ApiError> {
    let repo_root = state.repo_path(&name).await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    let resp = proxy::get_file(
        state.daemon_bin(),
        &repo_root,
        &file_path,
        params.line_start,
        params.line_end,
    ).await?;

    Ok(Json(resp))
}

// ---------- Status ----------

pub async fn index_status(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<indexrs_daemon::StatusResponse>, ApiError> {
    let repo_root = state.repo_path(&name).await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    let resp = proxy::status(state.daemon_bin(), &repo_root).await?;
    Ok(Json(resp))
}

// ---------- Refresh ----------

#[derive(Serialize)]
pub struct RefreshResponse {
    message: &'static str,
    repo: String,
}

pub async fn refresh_index(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<RefreshResponse>), ApiError> {
    let repo_root = state.repo_path(&name).await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    // Send reindex request to daemon (fire-and-forget — the daemon handles it async).
    let request = indexrs_daemon::types::DaemonRequest::Reindex;
    let stream = indexrs_daemon::ensure_daemon(state.daemon_bin(), &repo_root)
        .await
        .map_err(|e| ApiError::service_unavailable(format!("daemon unavailable: {e}")))?;

    // We don't wait for reindex to complete — just confirm it was accepted.
    let _ = indexrs_daemon::send_json_request(stream, &request).await;

    Ok((StatusCode::ACCEPTED, Json(RefreshResponse {
        message: "Reindex started",
        repo: name,
    })))
}

// ---------- Repo Management ----------

#[derive(Serialize)]
pub struct RepoInfo {
    name: String,
    path: String,
    status: String,
    files_indexed: usize,
}

#[derive(Serialize)]
pub struct ListReposResponse {
    repos: Vec<RepoInfo>,
}

pub async fn list_repos(
    State(state): State<AppState>,
) -> Result<Json<ListReposResponse>, ApiError> {
    let repos = state.repos().await;
    let mut repo_list = Vec::new();

    for (name, path) in &repos {
        let (status, files) = match proxy::status(state.daemon_bin(), path).await {
            Ok(s) => (s.status, s.files_indexed),
            Err(_) => ("offline".to_string(), 0),
        };
        repo_list.push(RepoInfo {
            name: name.clone(),
            path: path.to_string_lossy().into_owned(),
            status,
            files_indexed: files,
        });
    }

    Ok(Json(ListReposResponse { repos: repo_list }))
}

#[derive(Deserialize)]
pub struct AddRepoRequest {
    path: String,
    name: Option<String>,
}

pub async fn add_repo(
    State(state): State<AppState>,
    Json(body): Json<AddRepoRequest>,
) -> Result<(StatusCode, Json<RepoInfo>), ApiError> {
    let path = std::path::Path::new(&body.path);
    let canonical = path.canonicalize()
        .map_err(|_| ApiError::bad_request("invalid_path", format!("Path '{}' does not exist", body.path)))?;

    if !canonical.join(".indexrs").exists() {
        return Err(ApiError::bad_request(
            "not_initialized",
            format!("No index found at '{}'. Run 'indexrs init' first.", canonical.display()),
        ));
    }

    let name = body.name.unwrap_or_else(|| {
        canonical.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unnamed".to_string())
    });

    // Update config file.
    let mut config = registry::load_config()
        .map_err(|e| ApiError::internal(format!("failed to load config: {e}")))?;
    if !registry::add_repo(&mut config, &canonical, Some(&name)) {
        return Err(ApiError::bad_request("duplicate_repo", format!("Repo '{name}' already registered")));
    }
    registry::save_config(&config)
        .map_err(|e| ApiError::internal(format!("failed to save config: {e}")))?;

    // Update in-memory state and start daemon.
    state.add_repo(name.clone(), canonical.clone()).await;
    let _ = indexrs_daemon::ensure_daemon(state.daemon_bin(), &canonical).await;

    Ok((StatusCode::CREATED, Json(RepoInfo {
        name,
        path: canonical.to_string_lossy().into_owned(),
        status: "ready".to_string(),
        files_indexed: 0,
    })))
}

pub async fn remove_repo(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    if !state.remove_repo(&name).await {
        return Err(ApiError::repo_not_found(&name));
    }

    // Update config file.
    let mut config = registry::load_config()
        .map_err(|e| ApiError::internal(format!("failed to load config: {e}")))?;
    registry::remove_repo(&mut config, &name);
    registry::save_config(&config)
        .map_err(|e| ApiError::internal(format!("failed to save config: {e}")))?;

    Ok(StatusCode::NO_CONTENT)
}
```

### Step 4: Wire modules and routes into `lib.rs`

Add to the top of `lib.rs`:

```rust
pub mod api;
pub mod error;
pub mod proxy;
```

Update `build_router()` in `lib.rs` to register the API routes:

```rust
fn build_router(state: AppState) -> Router {
    let api = Router::new()
        // Repo-scoped endpoints
        .route("/repos/{name}/search", get(api::search))
        .route("/repos/{name}/files/{*path}", get(api::get_file))
        .route("/repos/{name}/status", get(api::index_status))
        .route("/repos/{name}/refresh", post(api::refresh_index))
        // Repo management
        .route("/repos", get(api::list_repos))
        .route("/repos", post(api::add_repo))
        .route("/repos/{name}", delete(api::remove_repo))
        // Global
        .route("/health", get(health));

    Router::new()
        .nest("/api/v1", api)
        .with_state(state)
}
```

Add `use axum::routing::{get, post, delete};` to the imports.

### Step 5: Verify compilation and lints

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

### Step 6: Playwright API test

Start the server (requires indexed repo):

```bash
# In the worktree root (which is the indexrs repo itself):
cargo run -p indexrs-cli -- init
cargo run -p indexrs-cli -- repos add . --name test-repo
# Start web server (you'll need a small binary or test harness — see note below)
```

**Note:** Since there's no CLI `web` subcommand yet (that's Task 3), Agent A should create a small test binary or use a test function that calls `indexrs_web::start_server()` directly. Alternatively, write a `#[tokio::test]` that boots the server on a random port and uses `reqwest` or `axum::test` to hit endpoints:

```rust
// In indexrs-web/src/lib.rs or a tests/ file
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for oneshot

    #[tokio::test]
    async fn test_health_endpoint() {
        let state = AppState::new(HashMap::new(), PathBuf::from("/dev/null"));
        let app = build_router(state);

        let resp = app
            .oneshot(Request::builder().uri("/api/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }
}
```

For Playwright testing, use `browser_navigate` to `http://localhost:4040/api/v1/health` and `browser_evaluate` to check the JSON response. Use `browser_navigate` to `http://localhost:4040/api/v1/repos/test-repo/search?q=fn+main` and verify the JSON structure.

### Step 7: Commit

```bash
git add indexrs-web/src/error.rs indexrs-web/src/proxy.rs indexrs-web/src/api.rs indexrs-web/src/lib.rs
git commit -m "feat(web): add JSON API endpoints with daemon proxy"
```

---

## Task 2: Frontend Static Files + Templates + UI Routes (Agent B)

**Goal:** Create all frontend assets (HTML, CSS, JS), askama templates, static file serving, and UI route handlers.

### Files

- Create: `indexrs-web/static/htmx.min.js` (vendored from https://unpkg.com/htmx.org)
- Create: `indexrs-web/static/style.css`
- Create: `indexrs-web/static/app.js`
- Create: `indexrs-web/templates/index.html`
- Create: `indexrs-web/templates/search_results.html`
- Create: `indexrs-web/templates/file_preview.html`
- Create: `indexrs-web/src/static_files.rs`
- Create: `indexrs-web/src/ui.rs`
- Modify: `indexrs-web/src/lib.rs` (add modules + routes)

### Step 1: Vendor htmx

Download htmx.min.js (version 2.0.x) and save to `indexrs-web/static/htmx.min.js`. This can be downloaded from `https://unpkg.com/htmx.org@2/dist/htmx.min.js`.

If download isn't possible, create a placeholder that will be replaced. The file must exist for rust-embed to include it.

### Step 2: Create `indexrs-web/static/style.css`

Follow the design doc's CSS custom properties for light/dark mode. Key layout: header bar, search bar, results list, file preview. Use system fonts, minimal design. Target ~200 lines.

```css
/* indexrs web interface styles */
*,
*::before,
*::after {
  box-sizing: border-box;
  margin: 0;
  padding: 0;
}

:root {
  --bg: #ffffff;
  --bg-secondary: #f6f8fa;
  --fg: #1a1a1a;
  --fg-dim: #6b7280;
  --match-bg: #fff3cd;
  --match-fg: #664d03;
  --border: #e0e0e0;
  --accent: #0969da;
  --danger: #cf222e;
  --success: #1a7f37;
  --warning: #9a6700;
  --font-mono: ui-monospace, "SF Mono", "Cascadia Code", Menlo, Consolas, monospace;
  --font-sans: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
  --radius: 6px;
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d1117;
    --bg-secondary: #161b22;
    --fg: #e6edf3;
    --fg-dim: #8b949e;
    --match-bg: #3b2e00;
    --match-fg: #f0c000;
    --border: #30363d;
    --accent: #58a6ff;
    --danger: #f85149;
    --success: #3fb950;
    --warning: #d29922;
  }
}

html, body {
  font-family: var(--font-sans);
  background: var(--bg);
  color: var(--fg);
  line-height: 1.5;
  height: 100%;
}

/* -- Header -- */
.header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 8px 16px;
  border-bottom: 1px solid var(--border);
  background: var(--bg-secondary);
}

.header__title {
  font-size: 16px;
  font-weight: 600;
  font-family: var(--font-mono);
}

.header__controls {
  display: flex;
  gap: 8px;
  align-items: center;
}

.badge {
  display: inline-flex;
  align-items: center;
  padding: 2px 8px;
  border-radius: 12px;
  font-size: 12px;
  font-weight: 500;
}

.badge--ready { background: var(--success); color: white; }
.badge--indexing { background: var(--warning); color: white; }
.badge--error { background: var(--danger); color: white; }
.badge--offline { background: var(--fg-dim); color: white; }

/* -- Search Bar -- */
.search-bar {
  padding: 12px 16px;
  border-bottom: 1px solid var(--border);
}

.search-input {
  width: 100%;
  padding: 8px 12px;
  font-family: var(--font-mono);
  font-size: 14px;
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  outline: none;
}

.search-input:focus {
  border-color: var(--accent);
  box-shadow: 0 0 0 2px rgba(9, 105, 218, 0.3);
}

/* -- Stats Line -- */
.stats-line {
  padding: 6px 16px;
  font-size: 13px;
  color: var(--fg-dim);
  border-bottom: 1px solid var(--border);
  display: flex;
  justify-content: space-between;
}

/* -- Results -- */
.results {
  padding: 0 16px 16px;
}

.file-result {
  margin-top: 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  overflow: hidden;
}

.file-header {
  display: flex;
  justify-content: space-between;
  padding: 6px 12px;
  background: var(--bg-secondary);
  border-bottom: 1px solid var(--border);
  font-size: 13px;
  font-family: var(--font-mono);
}

.file-header a {
  color: var(--accent);
  text-decoration: none;
}

.file-header a:hover {
  text-decoration: underline;
}

.file-lang {
  color: var(--fg-dim);
}

.code-lines {
  font-family: var(--font-mono);
  font-size: 13px;
  line-height: 1.5;
  overflow-x: auto;
}

.code-line {
  display: flex;
  padding: 0 12px;
  white-space: pre;
}

.code-line--match {
  background: var(--match-bg);
}

.code-line--context {
  color: var(--fg-dim);
}

.line-number {
  display: inline-block;
  min-width: 48px;
  padding-right: 12px;
  text-align: right;
  color: var(--fg-dim);
  user-select: none;
  flex-shrink: 0;
}

.line-content {
  flex: 1;
  overflow-x: auto;
}

mark {
  background: var(--match-bg);
  color: var(--match-fg);
  border-radius: 2px;
  padding: 0 1px;
}

/* -- Pagination -- */
.pagination {
  display: flex;
  gap: 4px;
  justify-content: center;
  padding: 16px;
}

.pagination a,
.pagination span {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  min-width: 32px;
  height: 32px;
  padding: 0 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  font-size: 13px;
  text-decoration: none;
  color: var(--fg);
}

.pagination a:hover {
  background: var(--bg-secondary);
}

.pagination .current {
  background: var(--accent);
  color: white;
  border-color: var(--accent);
}

/* -- Repo Selector -- */
.repo-select {
  font-family: var(--font-mono);
  font-size: 13px;
  padding: 4px 8px;
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: var(--radius);
}

/* -- Spinner -- */
.htmx-indicator {
  display: none;
}

.htmx-request .htmx-indicator,
.htmx-request.htmx-indicator {
  display: inline-block;
}

/* -- Keyboard help overlay -- */
.help-overlay {
  display: none;
  position: fixed;
  inset: 0;
  background: rgba(0,0,0,0.5);
  z-index: 100;
  align-items: center;
  justify-content: center;
}

.help-overlay.active {
  display: flex;
}

.help-content {
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 24px;
  max-width: 400px;
}

.help-content table {
  width: 100%;
  font-size: 14px;
}

.help-content td {
  padding: 4px 8px;
}

.help-content kbd {
  font-family: var(--font-mono);
  background: var(--bg-secondary);
  border: 1px solid var(--border);
  border-radius: 3px;
  padding: 1px 5px;
  font-size: 12px;
}

/* -- Back link (file preview) -- */
.back-link {
  padding: 8px 16px;
  border-bottom: 1px solid var(--border);
  font-size: 13px;
}

.back-link a {
  color: var(--accent);
  text-decoration: none;
}

/* -- Empty state -- */
.empty-state {
  text-align: center;
  padding: 48px 16px;
  color: var(--fg-dim);
}
```

### Step 3: Create `indexrs-web/static/app.js`

Keyboard shortcuts and minor interactions. ~100 lines.

```javascript
// indexrs keyboard shortcuts and interactions
(function() {
  'use strict';

  let selectedIndex = -1;

  function getFileResults() {
    return document.querySelectorAll('.file-result');
  }

  function updateSelection() {
    const results = getFileResults();
    results.forEach((el, i) => {
      el.style.outline = i === selectedIndex ? '2px solid var(--accent)' : '';
    });
    if (results[selectedIndex]) {
      results[selectedIndex].scrollIntoView({ block: 'nearest' });
    }
  }

  function isInputFocused() {
    const active = document.activeElement;
    return active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA' || active.tagName === 'SELECT');
  }

  document.addEventListener('keydown', function(e) {
    const helpOverlay = document.querySelector('.help-overlay');

    // Close help overlay on any key
    if (helpOverlay && helpOverlay.classList.contains('active')) {
      helpOverlay.classList.remove('active');
      e.preventDefault();
      return;
    }

    // Inside search input
    if (isInputFocused()) {
      if (e.key === 'Escape') {
        document.activeElement.blur();
        e.preventDefault();
      }
      if (e.key === 'l' && e.ctrlKey) {
        document.activeElement.value = '';
        document.activeElement.dispatchEvent(new Event('input'));
        e.preventDefault();
      }
      return;
    }

    // Global shortcuts (outside input)
    switch (e.key) {
      case '/':
        e.preventDefault();
        const searchInput = document.querySelector('.search-input');
        if (searchInput) searchInput.focus();
        break;

      case 'Escape':
        selectedIndex = -1;
        updateSelection();
        break;

      case 'j':
        e.preventDefault();
        selectedIndex = Math.min(selectedIndex + 1, getFileResults().length - 1);
        updateSelection();
        break;

      case 'k':
        e.preventDefault();
        selectedIndex = Math.max(selectedIndex - 1, 0);
        updateSelection();
        break;

      case 'Enter':
        if (selectedIndex >= 0) {
          const link = getFileResults()[selectedIndex]?.querySelector('.file-header a');
          if (link) link.click();
        }
        break;

      case 'n': {
        const nextLink = document.querySelector('.pagination .next-page');
        if (nextLink) nextLink.click();
        break;
      }

      case 'p': {
        const prevLink = document.querySelector('.pagination .prev-page');
        if (prevLink) prevLink.click();
        break;
      }

      case 'q':
      case 'Backspace': {
        const backLink = document.querySelector('.back-link a');
        if (backLink) backLink.click();
        break;
      }

      case '?':
        e.preventDefault();
        if (helpOverlay) helpOverlay.classList.add('active');
        break;
    }
  });

  // Reset selection when results change (htmx swap)
  document.body.addEventListener('htmx:afterSwap', function() {
    selectedIndex = -1;
  });

  // Auto-focus search on page load
  document.addEventListener('DOMContentLoaded', function() {
    const searchInput = document.querySelector('.search-input');
    if (searchInput) searchInput.focus();
  });
})();
```

### Step 4: Create askama templates

**`indexrs-web/templates/index.html`** — Main page shell. htmx loads search results as fragments.

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>indexrs</title>
  <link rel="stylesheet" href="/static/style.css">
</head>
<body>
  <div class="header">
    <span class="header__title">indexrs</span>
    <div class="header__controls">
      <select class="repo-select" id="repo-select"
              hx-get="/search-results"
              hx-trigger="change"
              hx-target="#results"
              hx-include="[name='q']">
        {% for repo in repos %}
        <option value="{{ repo.name }}" {% if repo.name == selected_repo %}selected{% endif %}>{{ repo.name }}</option>
        {% endfor %}
      </select>
      <span class="badge badge--{{ status }}">{{ status }}</span>
      <span class="badge">{{ repo_count }} repos</span>
    </div>
  </div>

  <div class="search-bar">
    <input type="search"
           class="search-input"
           name="q"
           placeholder="Search: language:rust fn handle..."
           autocomplete="off"
           hx-get="/search-results"
           hx-trigger="keyup changed delay:150ms, search"
           hx-target="#results"
           hx-include="#repo-select"
           hx-indicator="#search-spinner">
    <span id="search-spinner" class="htmx-indicator">searching...</span>
  </div>

  <div id="results">
    <div class="empty-state">
      <p>Type to search across your codebase</p>
      <p style="font-size: 13px; margin-top: 8px;">Press <kbd>?</kbd> for keyboard shortcuts</p>
    </div>
  </div>

  <div class="help-overlay">
    <div class="help-content">
      <h3 style="margin-bottom: 12px;">Keyboard Shortcuts</h3>
      <table>
        <tr><td><kbd>/</kbd></td><td>Focus search bar</td></tr>
        <tr><td><kbd>Escape</kbd></td><td>Clear selection / blur input</td></tr>
        <tr><td><kbd>j</kbd> / <kbd>k</kbd></td><td>Next / previous result</td></tr>
        <tr><td><kbd>Enter</kbd></td><td>Open selected file</td></tr>
        <tr><td><kbd>q</kbd></td><td>Back to search results</td></tr>
        <tr><td><kbd>n</kbd> / <kbd>p</kbd></td><td>Next / previous page</td></tr>
        <tr><td><kbd>?</kbd></td><td>This help</td></tr>
      </table>
    </div>
  </div>

  <script src="/static/htmx.min.js"></script>
  <script src="/static/app.js"></script>
</body>
</html>
```

**`indexrs-web/templates/search_results.html`** — HTML fragment returned for htmx search requests.

```html
{% if !files.is_empty() %}
<div class="stats-line">
  <span>{{ total_matches }} matches in {{ files_matched }} files ({{ duration_ms }}ms)</span>
  <span>page {{ page }} of {{ total_pages }}</span>
</div>

{% for file in &files %}
<div class="file-result">
  <div class="file-header">
    <a href="/file/{{ repo }}/{{ file.path.display() }}">{{ file.path.display() }}</a>
    <span>
      <span class="file-lang">{{ file.language }}</span>
      <span style="margin-left: 8px;">{{ file.lines.len() }}</span>
    </span>
  </div>
  <div class="code-lines">
    {% for line in &file.lines %}
      {% for ctx in &line.context_before %}
      <div class="code-line code-line--context">
        <span class="line-number">{{ ctx.line_number }}</span>
        <span class="line-content">{{ ctx.content }}</span>
      </div>
      {% endfor %}
      <div class="code-line code-line--match">
        <span class="line-number">{{ line.line_number }}</span>
        <span class="line-content">{{ line.content }}</span>
      </div>
      {% for ctx in &line.context_after %}
      <div class="code-line code-line--context">
        <span class="line-number">{{ ctx.line_number }}</span>
        <span class="line-content">{{ ctx.content }}</span>
      </div>
      {% endfor %}
    {% endfor %}
  </div>
</div>
{% endfor %}

{% if total_pages > 1 %}
<div class="pagination">
  {% if page > 1 %}
  <a class="prev-page" href="#"
     hx-get="/search-results?page={{ page - 1 }}"
     hx-target="#results"
     hx-include="[name='q'], #repo-select">&lt;</a>
  {% endif %}
  {% for p in 1..=total_pages %}
    {% if p == page %}
    <span class="current">{{ p }}</span>
    {% else %}
    <a href="#"
       hx-get="/search-results?page={{ p }}"
       hx-target="#results"
       hx-include="[name='q'], #repo-select">{{ p }}</a>
    {% endif %}
  {% endfor %}
  {% if has_next %}
  <a class="next-page" href="#"
     hx-get="/search-results?page={{ page + 1 }}"
     hx-target="#results"
     hx-include="[name='q'], #repo-select">&gt;</a>
  {% endif %}
</div>
{% endif %}

{% else %}
  {% if query.is_empty() %}
  <div class="empty-state">
    <p>Type to search across your codebase</p>
  </div>
  {% else %}
  <div class="empty-state">
    <p>No results for "{{ query }}"</p>
  </div>
  {% endif %}
{% endif %}
```

**`indexrs-web/templates/file_preview.html`** — Full file preview page.

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{{ path }} - indexrs</title>
  <link rel="stylesheet" href="/static/style.css">
</head>
<body>
  <div class="header">
    <span class="header__title">indexrs</span>
  </div>

  <div class="back-link">
    <a href="/" onclick="history.back(); return false;">&lt; Back to results</a>
    &nbsp;&nbsp;
    <span style="font-family: var(--font-mono);">{{ path }}</span>
    &nbsp;&nbsp;
    <span class="file-lang">{{ language }}</span>
    &nbsp;&nbsp;
    <span style="color: var(--fg-dim);">{{ total_lines }}L</span>
  </div>

  <div class="code-lines" style="padding: 8px 0;">
    {% for line in &lines %}
    <div class="code-line">
      <span class="line-number">{{ line.0 }}</span>
      <span class="line-content">{{ line.1 }}</span>
    </div>
    {% endfor %}
  </div>

  <script src="/static/htmx.min.js"></script>
  <script src="/static/app.js"></script>
</body>
</html>
```

### Step 5: Create `indexrs-web/src/static_files.rs`

```rust
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::extract::Path;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "static/"]
struct Assets;

pub async fn static_handler(Path(path): Path<String>) -> impl IntoResponse {
    Assets::get(&path)
        .map(|file| {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            let mut headers = vec![
                (header::CONTENT_TYPE, mime.to_string()),
            ];
            // Cache static assets aggressively (they're embedded, so versioned with binary)
            headers.push((
                header::CACHE_CONTROL,
                "public, max-age=31536000, immutable".to_string(),
            ));
            (headers, file.data.to_vec())
        })
        .ok_or(StatusCode::NOT_FOUND)
}
```

### Step 6: Create `indexrs-web/src/ui.rs`

Handlers for the web UI pages. These proxy to the daemon and render HTML via askama.

```rust
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::Html;
use serde::Deserialize;

use crate::error::ApiError;
use crate::proxy;
use crate::AppState;

// ---------- Index Page ----------

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    repos: Vec<RepoItem>,
    selected_repo: String,
    status: String,
    repo_count: usize,
}

struct RepoItem {
    name: String,
}

pub async fn index(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    let repos_map = state.repos().await;
    let repos: Vec<RepoItem> = repos_map.keys().map(|n| RepoItem { name: n.clone() }).collect();
    let selected = repos.first().map(|r| r.name.clone()).unwrap_or_default();
    let repo_count = repos.len();

    // Get status of selected repo
    let status = if let Some(path) = repos_map.get(&selected) {
        proxy::status(state.daemon_bin(), path).await
            .map(|s| s.status)
            .unwrap_or_else(|_| "offline".to_string())
    } else {
        "offline".to_string()
    };

    let tmpl = IndexTemplate { repos, selected_repo: selected, status, repo_count };
    Ok(Html(tmpl.render().map_err(|e| ApiError::internal(format!("template error: {e}")))?))
}

// ---------- Search Results Fragment ----------

#[derive(Deserialize)]
pub struct SearchResultsParams {
    #[serde(default)]
    q: String,
    #[serde(rename = "repo-select")]
    repo_select: Option<String>,
    #[serde(default = "default_page")]
    page: usize,
}

fn default_page() -> usize { 1 }

#[derive(Template)]
#[template(path = "search_results.html")]
struct SearchResultsTemplate {
    files: Vec<indexrs_core::search::FileMatch>,
    repo: String,
    query: String,
    total_matches: usize,
    files_matched: usize,
    duration_ms: u64,
    page: usize,
    total_pages: usize,
    has_next: bool,
}

pub async fn search_results_fragment(
    State(state): State<AppState>,
    Query(params): Query<SearchResultsParams>,
) -> Result<Html<String>, ApiError> {
    let repos = state.repos().await;
    let repo_name = params.repo_select
        .or_else(|| repos.keys().next().cloned())
        .unwrap_or_default();

    if params.q.is_empty() || params.q.len() < 3 {
        let tmpl = SearchResultsTemplate {
            files: vec![],
            repo: repo_name,
            query: params.q,
            total_matches: 0,
            files_matched: 0,
            duration_ms: 0,
            page: 1,
            total_pages: 0,
            has_next: false,
        };
        return Ok(Html(tmpl.render().map_err(|e| ApiError::internal(format!("template error: {e}")))?));
    }

    let repo_root = state.repo_path(&repo_name).await
        .ok_or_else(|| ApiError::repo_not_found(&repo_name))?;

    let (files, stats) = proxy::search(
        state.daemon_bin(),
        &repo_root,
        &params.q,
        params.page,
        25,
        2,
        None,
        None,
    ).await?;

    let tmpl = SearchResultsTemplate {
        files,
        repo: repo_name,
        query: params.q,
        total_matches: stats.total_matches,
        files_matched: stats.files_matched,
        duration_ms: stats.duration_ms,
        page: stats.page,
        total_pages: stats.total_pages,
        has_next: stats.has_next,
    };
    Ok(Html(tmpl.render().map_err(|e| ApiError::internal(format!("template error: {e}")))?))
}

// ---------- File Preview ----------

#[derive(Template)]
#[template(path = "file_preview.html")]
struct FilePreviewTemplate {
    path: String,
    language: String,
    total_lines: usize,
    lines: Vec<(usize, String)>,  // (line_number, content)
}

pub async fn file_preview(
    State(state): State<AppState>,
    Path((repo, file_path)): Path<(String, String)>,
) -> Result<Html<String>, ApiError> {
    let repo_root = state.repo_path(&repo).await
        .ok_or_else(|| ApiError::repo_not_found(&repo))?;

    let resp = proxy::get_file(state.daemon_bin(), &repo_root, &file_path, None, None).await?;

    let lines: Vec<(usize, String)> = resp.lines.iter()
        .enumerate()
        .map(|(i, l)| (i + 1, l.clone()))
        .collect();

    let tmpl = FilePreviewTemplate {
        path: resp.path,
        language: resp.language,
        total_lines: resp.total_lines,
        lines,
    };
    Ok(Html(tmpl.render().map_err(|e| ApiError::internal(format!("template error: {e}")))?))
}
```

### Step 7: Wire into `lib.rs`

Add modules:

```rust
pub mod static_files;
pub mod ui;
```

Update `build_router()`:

```rust
fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/repos/{name}/search", get(api::search))
        .route("/repos/{name}/files/{*path}", get(api::get_file))
        .route("/repos/{name}/status", get(api::index_status))
        .route("/repos/{name}/refresh", post(api::refresh_index))
        .route("/repos", get(api::list_repos))
        .route("/repos", post(api::add_repo))
        .route("/repos/{name}", delete(api::remove_repo))
        .route("/health", get(health));

    Router::new()
        // Web UI routes
        .route("/", get(ui::index))
        .route("/search-results", get(ui::search_results_fragment))
        .route("/file/{repo}/{*path}", get(ui::file_preview))
        .route("/static/{*path}", get(static_files::static_handler))
        // JSON API
        .nest("/api/v1", api)
        .with_state(state)
}
```

### Step 8: Verify compilation and lints

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

**Note:** askama template compilation happens at build time. If templates have syntax errors, compilation will fail with clear error messages. Fix iteratively.

### Step 9: Playwright UI test

Start the server, then use Playwright MCP tools:

1. `browser_navigate` to `http://localhost:4040` — verify the page loads
2. `browser_snapshot` — verify search input, header, and repo selector are present
3. `browser_fill_form` to type "fn main" in the search input
4. Wait ~200ms for htmx debounce, then `browser_snapshot` — verify results appear
5. `browser_click` on a file path link — verify file preview renders
6. `browser_evaluate` to check keyboard shortcut: simulate pressing '/' and verify search input is focused

### Step 10: Commit

```bash
git add indexrs-web/static/ indexrs-web/templates/ indexrs-web/src/static_files.rs indexrs-web/src/ui.rs indexrs-web/src/lib.rs
git commit -m "feat(web): add frontend assets, templates, and UI routes"
```

---

## Task 3: SSE Streaming + CLI Web Subcommand (Agent C)

**Goal:** Add SSE streaming endpoints for live search and status updates, plus the `indexrs web` CLI subcommand.

### Files

- Create: `indexrs-web/src/sse.rs`
- Modify: `indexrs-web/src/lib.rs` (add module + routes)
- Create: `indexrs-cli/src/web.rs`
- Modify: `indexrs-cli/src/args.rs` (add Web subcommand)
- Modify: `indexrs-cli/src/main.rs` (dispatch Web command)
- Modify: `indexrs-cli/Cargo.toml` (add indexrs-web dependency)

### Step 1: Create `indexrs-web/src/sse.rs`

SSE endpoints for streaming search results and status updates.

```rust
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, Sse};
use futures::stream::Stream;
use serde::Deserialize;
use tokio_stream::StreamExt;

use indexrs_daemon::types::DaemonRequest;
use indexrs_daemon::{JsonSearchFrame, ensure_daemon};
use crate::error::ApiError;
use crate::AppState;

#[derive(Deserialize)]
pub struct StreamSearchParams {
    q: String,
    #[serde(default = "default_per_page")]
    per_page: usize,
    #[serde(default = "default_context")]
    context: usize,
    language: Option<String>,
    path: Option<String>,
}

fn default_per_page() -> usize { 25 }
fn default_context() -> usize { 2 }

/// GET /api/v1/repos/{name}/search/stream
///
/// Streams search results as SSE events. Each file match is an individual
/// "result" event, followed by a "stats" event and "done" event.
pub async fn search_stream(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<StreamSearchParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let repo_root = state.repo_path(&name).await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    if params.q.is_empty() {
        return Err(ApiError::bad_request("invalid_query", "Query parameter 'q' is required"));
    }

    let daemon_bin = state.daemon_bin().clone();
    let per_page = params.per_page.min(100);

    let request = DaemonRequest::JsonSearch {
        query: params.q,
        page: 1,
        per_page,
        context_lines: params.context,
        language: params.language,
        path_glob: params.path,
    };

    // Connect to daemon and stream TLV frames as SSE events.
    let stream = ensure_daemon(&daemon_bin, &repo_root)
        .await
        .map_err(|e| ApiError::service_unavailable(format!("daemon unavailable: {e}")))?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader);

    // Send the request
    use tokio::io::AsyncWriteExt;
    let json = serde_json::to_string(&request)
        .map_err(|e| ApiError::internal(format!("serialization error: {e}")))?;
    writer.write_all(format!("{json}\n").as_bytes())
        .await
        .map_err(|e| ApiError::internal(format!("write error: {e}")))?;

    // Create an async stream that reads TLV frames and yields SSE events.
    let event_stream = async_stream::stream! {
        loop {
            match indexrs_daemon::wire::read_response(&mut reader).await {
                Ok(indexrs_daemon::types::DaemonResponse::Json { payload }) => {
                    match serde_json::from_str::<JsonSearchFrame>(&payload) {
                        Ok(JsonSearchFrame::Result { .. }) => {
                            yield Ok(Event::default().event("result").data(payload));
                        }
                        Ok(JsonSearchFrame::Stats { .. }) => {
                            yield Ok(Event::default().event("stats").data(payload));
                        }
                        Err(_) => {
                            yield Ok(Event::default().event("result").data(payload));
                        }
                    }
                }
                Ok(indexrs_daemon::types::DaemonResponse::Done { .. }) => {
                    yield Ok(Event::default().event("done").data("{}"));
                    break;
                }
                Ok(indexrs_daemon::types::DaemonResponse::Error { message }) => {
                    yield Ok(Event::default().event("error").data(message));
                    break;
                }
                Ok(_) => continue,  // skip Line, Progress, Pong
                Err(_) => break,     // connection lost
            }
        }
    };

    Ok(Sse::new(event_stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

/// GET /api/v1/repos/{name}/status/stream
///
/// Streams index status updates as SSE events. Polls the daemon periodically.
pub async fn status_stream(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let repo_root = state.repo_path(&name).await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    let daemon_bin = state.daemon_bin().clone();

    let event_stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;

            let request = DaemonRequest::Status;
            match ensure_daemon(&daemon_bin, &repo_root).await {
                Ok(stream) => {
                    match indexrs_daemon::send_json_request(stream, &request).await {
                        Ok(result) => {
                            if let Some(payload) = result.payloads.first() {
                                yield Ok(Event::default().event("status").data(payload.clone()));
                            }
                        }
                        Err(_) => {
                            yield Ok(Event::default().event("status").data(
                                r#"{"status":"error","files_indexed":0,"segments":0}"#.to_string()
                            ));
                        }
                    }
                }
                Err(_) => {
                    yield Ok(Event::default().event("status").data(
                        r#"{"status":"offline","files_indexed":0,"segments":0}"#.to_string()
                    ));
                }
            }
        }
    };

    Ok(Sse::new(event_stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}
```

**Note on dependencies:** This module requires `async-stream` and `tokio-stream` crates. Add to `indexrs-web/Cargo.toml`:

```toml
async-stream = "0.3"
tokio-stream = "0.1"
futures = "0.3"
```

### Step 2: Wire SSE routes into `lib.rs`

Add module:

```rust
pub mod sse;
```

Add routes in `build_router()`:

```rust
.route("/repos/{name}/search/stream", get(sse::search_stream))
.route("/repos/{name}/status/stream", get(sse::status_stream))
```

### Step 3: Add indexrs-web dependency to CLI

In `indexrs-cli/Cargo.toml`, add:

```toml
indexrs-web = { path = "../indexrs-web" }
```

### Step 4: Create `indexrs-cli/src/web.rs`

The `web` subcommand handler. Reads repos.toml, builds the repo map, and starts the server.

```rust
use std::collections::HashMap;
use std::path::PathBuf;

use indexrs_core::error::IndexError;
use indexrs_core::registry;

/// Run the web server with all registered repos.
pub async fn run_web(port: u16) -> Result<(), IndexError> {
    let config = registry::load_config()?;

    let mut repos = HashMap::new();
    for entry in &config.repo {
        let name = entry.effective_name().to_string();
        repos.insert(name, entry.path.clone());
    }

    if repos.is_empty() {
        eprintln!("warning: no repos registered. Use 'indexrs repos add <path>' to add one.");
    }

    let daemon_bin = std::env::current_exe().map_err(IndexError::Io)?;

    indexrs_web::start_server(repos, daemon_bin, port)
        .await
        .map_err(|e| IndexError::Io(std::io::Error::other(e.to_string())))
}
```

### Step 5: Add Web subcommand to CLI args

In `indexrs-cli/src/args.rs`, add to the `Command` enum:

```rust
/// Start the web interface
Web {
    /// Port to listen on
    #[arg(short, long, default_value_t = 4040)]
    port: u16,
},
```

### Step 6: Wire Web command in `main.rs`

Add to `main.rs`:

```rust
mod web;
```

Add match arm in the `run()` function:

```rust
Command::Web { port } => {
    web::run_web(port).await?;
    Ok(ExitCode::Success)
}
```

### Step 7: Verify compilation and lints

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: PASS

### Step 8: Playwright SSE test

Start the server via CLI:

```bash
cargo run -p indexrs-cli -- web --port 4040
```

Use Playwright MCP:

1. `browser_navigate` to `http://localhost:4040`
2. `browser_evaluate` to test SSE:
```javascript
const evtSource = new EventSource('/api/v1/repos/test-repo/status/stream');
return new Promise((resolve) => {
  evtSource.addEventListener('status', (e) => {
    evtSource.close();
    resolve(JSON.parse(e.data));
  });
  setTimeout(() => { evtSource.close(); resolve(null); }, 5000);
});
```
3. Verify the returned object has `status`, `files_indexed`, `segments` fields.

### Step 9: Commit

```bash
git add indexrs-web/src/sse.rs indexrs-web/src/lib.rs indexrs-web/Cargo.toml \
        indexrs-cli/src/web.rs indexrs-cli/src/args.rs indexrs-cli/src/main.rs indexrs-cli/Cargo.toml
git commit -m "feat(web): add SSE streaming endpoints and CLI web subcommand"
```

---

## Task 4: Integration + Merge + End-to-End Testing (Lead)

**Goal:** Merge all agent branches, resolve conflicts, and run comprehensive Playwright end-to-end tests.

### Step 1: Merge branches

```bash
# From main branch
git merge feat/web-api
git merge feat/web-frontend     # likely conflicts in lib.rs — resolve by combining routes
git merge feat/web-streaming-cli # likely conflicts in lib.rs — resolve by combining routes
```

The primary merge conflicts will be in `indexrs-web/src/lib.rs` (the `build_router()` function and module declarations). Resolve by combining all module declarations and all routes from all three branches.

### Step 2: Verify full compilation

Run:

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

All must pass.

### Step 3: End-to-end Playwright test suite

Start the server:

```bash
# Ensure the repo is initialized and registered
cargo run -p indexrs-cli -- init
cargo run -p indexrs-cli -- repos add . --name indexrs
cargo run -p indexrs-cli -- web --port 4040
```

**Test 1: Health endpoint**
- `browser_navigate` to `http://localhost:4040/api/v1/health`
- Verify JSON contains `"status": "ok"` and `"version"`

**Test 2: Repo listing**
- `browser_navigate` to `http://localhost:4040/api/v1/repos`
- Verify JSON contains repo with name "indexrs"

**Test 3: Search API**
- `browser_navigate` to `http://localhost:4040/api/v1/repos/indexrs/search?q=fn+main`
- Verify JSON has `stats`, `results`, `pagination` fields
- Verify `stats.total_matches > 0`

**Test 4: File retrieval API**
- `browser_navigate` to `http://localhost:4040/api/v1/repos/indexrs/files/src/main.rs`
- Verify JSON has `path`, `language`, `lines` fields

**Test 5: Web UI loads**
- `browser_navigate` to `http://localhost:4040`
- `browser_snapshot` — verify search input, header, repo selector exist

**Test 6: Search-as-you-type**
- `browser_navigate` to `http://localhost:4040`
- `browser_type` "fn main" into the search input (use selector `.search-input`)
- Wait 300ms
- `browser_snapshot` — verify `.file-result` elements exist in results
- Verify stats line shows match count and duration

**Test 7: File preview navigation**
- From test 6 results, `browser_click` on a file path link
- `browser_snapshot` — verify file preview page loads with line numbers and code

**Test 8: Keyboard shortcuts**
- `browser_navigate` to `http://localhost:4040`
- `browser_press_key` `/` — verify search input gets focus
- `browser_press_key` `Escape` — verify search input loses focus
- `browser_press_key` `?` — verify help overlay appears

**Test 9: SSE search stream**
- `browser_evaluate` to test:
```javascript
const resp = await fetch('/api/v1/repos/indexrs/search/stream?q=fn+main');
const reader = resp.body.getReader();
const decoder = new TextDecoder();
let text = '';
while (true) {
  const { value, done } = await reader.read();
  if (done) break;
  text += decoder.decode(value);
  if (text.includes('event: done')) break;
}
return text.includes('event: result');
```
- Verify returns `true`

**Test 10: Dark mode**
- `browser_evaluate`:
```javascript
window.matchMedia('(prefers-color-scheme: dark)').matches
```
- Note the result, then check CSS custom properties are applied correctly

### Step 4: Fix any issues found

Iterate on any test failures until all 10 tests pass.

### Step 5: Final commit

```bash
git add -A
git commit -m "feat(web): complete web interface with API, UI, and SSE streaming"
```

---

## Dependency Summary

New crates added to the workspace:

| Crate | Version | Purpose |
|-------|---------|---------|
| axum | 0.8 | HTTP framework |
| tower-http | 0.6 | CORS, compression middleware |
| askama | 0.13 | Template rendering |
| askama_axum | 0.5 | askama + axum integration |
| rust-embed | 8 | Embed static files in binary |
| mime_guess | 2 | MIME type detection |
| async-stream | 0.3 | Async stream construction (SSE) |
| tokio-stream | 0.1 | Stream adapters |
| futures | 0.3 | Stream trait |

**IMPORTANT:** Verify all versions are latest at implementation time. Run `cargo add <dep>` to automatically resolve the latest compatible version. Dependency versions above are approximate — actual latest versions may differ.

## Notes for Agents

1. **Each agent works in their own worktree.** Never push to main directly.
2. **Before starting work**, run `cargo check --workspace` to verify the foundation (Task 0) is clean.
3. **Askama template syntax** varies by version. Consult askama docs for the exact version used. Key patterns: `{% for x in &items %}`, `{{ x }}`, `{% if condition %}`.
4. **rust-embed `#[folder]`** path is relative to the crate root (i.e., `indexrs-web/`). So `#[folder = "static/"]` refers to `indexrs-web/static/`.
5. **The web server binds to `127.0.0.1` only** — never `0.0.0.0`. This is a local dev tool.
6. **Playwright MCP testing**: Install browser first with `browser_install` tool. Use `browser_navigate` + `browser_snapshot` for visual checks, `browser_evaluate` for programmatic assertions.
7. **htmx vendoring**: Download from unpkg or cdnjs. Do not use a CDN link in HTML — everything must work offline.
