pub mod errors;
pub mod formatter;
pub mod server;
pub mod tools;

use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::transport::io::stdio;

use server::IndexrsServer;

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

    let index_state = Arc::new(indexrs_core::IndexState::new());
    let server = IndexrsServer::new(index_state, None);

    let service = server
        .serve(stdio())
        .await
        .expect("failed to start MCP server");

    service.waiting().await.expect("MCP service error");
}
