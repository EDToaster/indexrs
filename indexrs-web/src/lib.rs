pub mod api;
pub mod error;
pub mod proxy;
pub mod sse;
pub mod static_files;
pub mod ui;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::{delete, get, post};
use serde::Serialize;
use tokio::sync::RwLock;

/// Shared application state passed to all handlers via axum's State extractor.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    /// Map of repo name -> absolute path to repo root.
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
pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/repos/{name}/search", get(api::search))
        .route("/repos/{name}/files/{*path}", get(api::get_file))
        .route("/repos/{name}/status", get(api::index_status))
        .route("/repos/{name}/refresh", post(api::refresh_index))
        .route("/repos", get(api::list_repos))
        .route("/repos", post(api::add_repo))
        .route("/repos/{name}", delete(api::remove_repo))
        .route("/repos/{name}/symbols", get(api::symbols))
        .route("/repos/{name}/search/stream", get(sse::search_stream))
        .route("/repos/{name}/status/stream", get(sse::status_stream));

    Router::new()
        .route("/", get(ui::index))
        .route("/search-results", get(ui::search_results_fragment))
        .route("/symbol-results", get(ui::symbol_results_fragment))
        .route("/repo-status", get(ui::repo_status))
        .route("/repos", get(ui::repos_page))
        .route("/file/{repo}/{*path}", get(ui::file_preview))
        .route("/symbol-outline", get(ui::symbol_outline_fragment))
        .route("/static/{*path}", get(static_files::static_handler))
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
