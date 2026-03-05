//! MCP server implementation.
//!
//! Activated by the `mcp` cargo feature (enabled by default).
//! Run via `ferret mcp [--repo PATH]`.

// Many items in this module are used only by tests or via rmcp macro expansion
// that dead-code analysis cannot see.
#![allow(dead_code)]

pub mod daemon_client;
pub mod errors;
pub mod formatter;
pub mod resources;
pub mod server;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::transport::io::stdio;

use server::FerretServer;

/// Run the MCP server over stdio.
pub async fn run_mcp(repo_root: PathBuf) -> Result<(), String> {
    let segments_dir = repo_root.join(".ferret_index").join("segments");
    let index_state = Arc::new(ferret_indexer_core::IndexState::new());

    match ferret_indexer_core::recover_segments(&segments_dir) {
        Ok(segments) => {
            if !segments.is_empty() {
                let count = segments.len();
                let arcs: Vec<Arc<ferret_indexer_core::Segment>> =
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
    let server = FerretServer::new(index_state, Some(repo_root), Some(daemon));

    let service = server
        .serve(stdio())
        .await
        .map_err(|e| format!("failed to start MCP server: {e}"))?;

    service
        .waiting()
        .await
        .map_err(|e| format!("MCP service error: {e}"))?;

    Ok(())
}
