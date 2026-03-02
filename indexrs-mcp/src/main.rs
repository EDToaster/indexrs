pub mod daemon_client;
pub mod errors;
pub mod formatter;
pub mod resources;
pub mod server;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;

use server::IndexrsServer;

#[derive(Parser)]
#[command(name = "indexrs-mcp", about = "indexrs MCP server")]
struct Cli {
    /// Path to the repository root. Walks up from cwd looking for .indexrs/ or .git/ if omitted.
    #[arg(long)]
    repo: Option<PathBuf>,
}

/// Find the repository root directory.
///
/// If `repo_arg` is provided, canonicalizes it.
/// Otherwise walks up from cwd looking for `.indexrs/` or `.git/`.
fn find_repo_root(repo_arg: Option<&PathBuf>) -> Result<PathBuf, String> {
    if let Some(repo) = repo_arg {
        return std::fs::canonicalize(repo)
            .map_err(|e| format!("invalid --repo path '{}': {e}", repo.display()));
    }
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;
    let mut dir = cwd.clone();
    loop {
        if dir.join(".indexrs").is_dir() || dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(format!(
                "not inside a git repository or indexrs project (searched from {})",
                cwd.display()
            ));
        }
    }
}

#[tokio::main]
async fn main() {
    // Log to stderr so stdout is reserved for MCP JSON-RPC protocol traffic.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let repo_root = match find_repo_root(cli.repo.as_ref()) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let segments_dir = repo_root.join(".indexrs").join("segments");
    let index_state = Arc::new(indexrs_core::IndexState::new());

    match indexrs_core::recover_segments(&segments_dir) {
        Ok(segments) => {
            if !segments.is_empty() {
                let count = segments.len();
                let arcs: Vec<Arc<indexrs_core::Segment>> =
                    segments.into_iter().map(Arc::new).collect();
                index_state.publish(arcs);
                eprintln!("loaded {count} segment(s) from {}", segments_dir.display());
            }
        }
        Err(e) => {
            eprintln!(
                "warning: failed to recover segments from {}: {e}",
                segments_dir.display()
            );
        }
    }

    let daemon = Arc::new(daemon_client::DaemonClient::new(repo_root.clone()));
    let server = IndexrsServer::new(index_state, Some(repo_root), Some(daemon));

    let service = server
        .serve(stdio())
        .await
        .expect("failed to start MCP server");

    service.waiting().await.expect("MCP service error");
}
