//! MCP server implementation using rmcp.
//!
//! [`IndexrsServer`] is the main server struct that implements the rmcp
//! [`ServerHandler`] trait. It holds shared state (index segments, root paths)
//! and will host tool methods added by Phase 2 agents.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool};

use indexrs_core::IndexState;

/// The MCP server for indexrs.
#[derive(Clone)]
pub struct IndexrsServer {
    /// Snapshot-isolated access to active index segments.
    pub index_state: Arc<IndexState>,
    /// Root path of the indexed repository.
    pub root_path: Option<PathBuf>,
}

impl IndexrsServer {
    /// Create a new server with the given shared state.
    pub fn new(index_state: Arc<IndexState>, root_path: Option<PathBuf>) -> Self {
        Self {
            index_state,
            root_path,
        }
    }
}

#[tool(tool_box)]
impl IndexrsServer {
    #[tool(description = "Get indexrs server version and basic status")]
    fn ping(&self) -> String {
        format!("indexrs MCP server v{}", env!("CARGO_PKG_VERSION"))
    }
}

#[tool(tool_box)]
impl ServerHandler for IndexrsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Code search index server. Use search_code to find code, \
                 search_files to find files by name, get_file to read file contents, \
                 index_status to check index health, and reindex to update the index."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: "indexrs".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_creation() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None);
        assert!(server.root_path.is_none());
    }

    #[test]
    fn test_server_creation_with_root() {
        let state = Arc::new(IndexState::new());
        let root = PathBuf::from("/tmp/myrepo");
        let server = IndexrsServer::new(state, Some(root.clone()));
        assert_eq!(server.root_path, Some(root));
    }

    #[test]
    fn test_server_info() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None);
        let info = server.get_info();
        assert_eq!(info.server_info.name, "indexrs");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.instructions.is_some());
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.resources.is_some());
    }

    #[test]
    fn test_ping_tool() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None);
        let result = server.ping();
        assert!(result.contains("indexrs MCP server"));
        assert!(result.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn test_tool_attributes_generated() {
        let attr = IndexrsServer::ping_tool_attr();
        assert_eq!(attr.name.as_ref(), "ping");
        assert!(attr.description.as_ref().contains("indexrs server version"));
    }
}
