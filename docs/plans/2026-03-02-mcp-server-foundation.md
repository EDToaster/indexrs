# MCP Server Foundation Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the MCP server foundation (HHC-58, HHC-63, HHC-65) -- server setup with stdio transport, plain-text response formatter, and error handling with helpful messages.

**Architecture:** The `ferret-mcp` crate gets 4 new modules: `server.rs` (rmcp ServerHandler with FerretServer struct), `formatter.rs` (plain-text rendering optimized for LLMs), `errors.rs` (MCP error helpers returning CallToolResult with is_error=true), and `tools/mod.rs` (empty placeholder). The server uses the `#[tool(tool_box)]` macro pattern from rmcp for tool registration. All modules expose public interfaces for Phase 2 agents to build tools/resources on.

**Tech Stack:** Rust, rmcp 0.1.5 (with server + transport-io + macros features), schemars 0.8, tokio, ferret-indexer-core

---

### Task 1: Add schemars dependency to Cargo.toml

**Files:**
- Modify: `ferret-mcp/Cargo.toml`

**Step 1: Add schemars dependency**

Add `schemars = "0.8"` to `[dependencies]` in `ferret-mcp/Cargo.toml`. Also ensure `rmcp` has the `macros` feature enabled. The final Cargo.toml should be:

```toml
[package]
name = "ferret-mcp"
version = "0.1.0"
edition = "2024"

[dependencies]
rmcp = { version = "0.1", features = ["server", "transport-io", "macros"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
ferret-indexer-core = { path = "../ferret-indexer-core" }
schemars = "0.8"
```

**Step 2: Verify it compiles**

Run: `cargo check -p ferret-mcp`
Expected: Compiles successfully

**Step 3: Commit**

```bash
git add ferret-mcp/Cargo.toml
git commit -m "chore(mcp): add schemars and macros feature to dependencies"
```

---

### Task 2: Create errors.rs module with MCP error helpers

**Files:**
- Create: `ferret-mcp/src/errors.rs`
- Modify: `ferret-mcp/src/main.rs` (add `mod errors;`)

**Step 1: Write tests for error helpers**

Create `ferret-mcp/src/errors.rs` with tests first (TDD). The error helpers return `CallToolResult` with `is_error: Some(true)` and text content matching the design doc error formats.

```rust
//! MCP error handling with helpful messages.
//!
//! Returns errors using MCP's `isError: true` response field with
//! human-readable messages. Each function creates a `CallToolResult`
//! that tools can return directly.

use rmcp::model::{CallToolResult, Content};

/// Create an error response for when a repository is not found.
///
/// Lists available repositories so the LLM can self-correct.
pub fn repo_not_found(repo: &str, available: &[String]) -> CallToolResult {
    let msg = if available.is_empty() {
        format!("Error: Repository \"{repo}\" not found. No repositories are currently indexed.")
    } else {
        format!(
            "Error: Repository \"{repo}\" not found. Indexed repositories: {}",
            available.join(", ")
        )
    };
    CallToolResult::error(vec![Content::text(msg)])
}

/// Create an error response for when a file is not found in the index.
///
/// Includes did-you-mean suggestions if similar filenames exist.
pub fn file_not_found(path: &str, suggestions: &[String]) -> CallToolResult {
    let msg = if suggestions.is_empty() {
        format!("Error: File \"{path}\" not found in the index.")
    } else if suggestions.len() == 1 {
        format!(
            "Error: File \"{path}\" not found. Did you mean \"{}\"?",
            suggestions[0]
        )
    } else {
        format!(
            "Error: File \"{path}\" not found. Similar files: {}",
            suggestions.join(", ")
        )
    };
    CallToolResult::error(vec![Content::text(msg)])
}

/// Create an error response for an invalid query.
///
/// Shows the error message which should include position info when available.
pub fn invalid_query(msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(format!("Error: Invalid query: {msg}"))])
}

/// Create an error response for an invalid parameter value.
///
/// Shows the parameter name and what went wrong.
pub fn invalid_parameter(param: &str, msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(format!(
        "Error: Invalid parameter \"{param}\": {msg}"
    ))])
}

/// Create an error response when the index is currently being built.
///
/// Shows progress percentage if available.
pub fn index_building(progress_pct: Option<f64>) -> CallToolResult {
    let msg = match progress_pct {
        Some(pct) => format!(
            "Error: Index is currently being built ({:.0}% complete). Try again shortly.",
            pct
        ),
        None => "Error: Index is currently being built. Try again shortly.".to_string(),
    };
    CallToolResult::error(vec![Content::text(msg)])
}

/// Create a response for when a search returns no results.
///
/// Includes suggestions for refining the query.
pub fn no_results(query: &str, suggestions: &[String]) -> CallToolResult {
    let mut msg = format!("No matches found for \"{query}\".");
    if suggestions.is_empty() {
        msg.push_str(
            " Suggestions: check spelling, try a broader query, or remove filters.",
        );
    } else {
        msg.push_str(" Suggestions: ");
        msg.push_str(&suggestions.join("; "));
        msg.push('.');
    }
    // no_results is not an error -- it's a valid empty result
    CallToolResult::success(vec![Content::text(msg)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_text(result: &CallToolResult) -> &str {
        result.content[0]
            .raw
            .as_text()
            .expect("expected text content")
            .text
            .as_str()
    }

    #[test]
    fn test_repo_not_found_with_repos() {
        let result = repo_not_found("foo", &["ferret".into(), "myproject".into()]);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("\"foo\""));
        assert!(text.contains("ferret"));
        assert!(text.contains("myproject"));
    }

    #[test]
    fn test_repo_not_found_no_repos() {
        let result = repo_not_found("foo", &[]);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("No repositories are currently indexed"));
    }

    #[test]
    fn test_file_not_found_with_suggestions() {
        let result = file_not_found("src/missing.rs", &["src/main.rs".into()]);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("\"src/missing.rs\""));
        assert!(text.contains("Did you mean"));
        assert!(text.contains("src/main.rs"));
    }

    #[test]
    fn test_file_not_found_multiple_suggestions() {
        let result = file_not_found(
            "src/missing.rs",
            &["src/main.rs".into(), "src/lib.rs".into()],
        );
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("Similar files"));
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("src/lib.rs"));
    }

    #[test]
    fn test_file_not_found_no_suggestions() {
        let result = file_not_found("totally/unknown.rs", &[]);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("\"totally/unknown.rs\""));
        assert!(!text.contains("Did you mean"));
    }

    #[test]
    fn test_invalid_query() {
        let result = invalid_query("unmatched '(' at position 5");
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("Invalid query"));
        assert!(text.contains("position 5"));
    }

    #[test]
    fn test_invalid_parameter() {
        let result = invalid_parameter("context_lines", "must be between 0 and 10, got 25");
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("\"context_lines\""));
        assert!(text.contains("must be between 0 and 10"));
    }

    #[test]
    fn test_index_building_with_progress() {
        let result = index_building(Some(45.0));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("45%"));
        assert!(text.contains("Try again shortly"));
    }

    #[test]
    fn test_index_building_no_progress() {
        let result = index_building(None);
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("being built"));
        assert!(!text.contains("%"));
    }

    #[test]
    fn test_no_results_default_suggestions() {
        let result = no_results("foobar", &[]);
        // no_results is NOT an error
        assert_eq!(result.is_error, Some(false));
        let text = extract_text(&result);
        assert!(text.contains("\"foobar\""));
        assert!(text.contains("check spelling"));
    }

    #[test]
    fn test_no_results_custom_suggestions() {
        let result = no_results(
            "foobar",
            &[
                "try removing the path: filter".into(),
                "use a broader query".into(),
            ],
        );
        assert_eq!(result.is_error, Some(false));
        let text = extract_text(&result);
        assert!(text.contains("try removing the path: filter"));
        assert!(text.contains("use a broader query"));
    }
}
```

**Step 2: Add module declaration to main.rs**

Add `pub mod errors;` to `main.rs` (keep the existing main function).

**Step 3: Run tests**

Run: `cargo test -p ferret-mcp`
Expected: All error tests pass

**Step 4: Run clippy and fmt**

Run: `cargo clippy -p ferret-mcp -- -D warnings && cargo fmt --all -- --check`
Expected: No warnings, formatting OK

**Step 5: Commit**

```bash
git add ferret-mcp/src/errors.rs ferret-mcp/src/main.rs
git commit -m "feat(mcp): add error helpers with helpful MCP error messages (HHC-65)"
```

---

### Task 3: Create formatter.rs module with plain-text response formatting

**Files:**
- Create: `ferret-mcp/src/formatter.rs`
- Modify: `ferret-mcp/src/main.rs` (add `pub mod formatter;`)

This module formats search results as plain text optimized for LLM consumption (~40% fewer tokens than JSON), matching the design doc format.

**Step 1: Write the formatter module with tests**

Create `ferret-mcp/src/formatter.rs`. The module provides:
- `format_search_results()` - main search result formatter
- `format_file_list()` - file listing formatter
- `format_file_content()` - single file content formatter
- `format_index_status()` - index status formatter
- `format_staleness_warning()` - staleness warning generator
- `FormatOptions` - formatting options struct
- `FileInfo` - file info for file list
- `FileFormatMetadata` - metadata for file content display
- `IndexStatusInfo` - status info struct

The format matches the design doc:
```
Found 47 matches across 12 files (showing 1-12)

## src/index/builder.rs
L42:   fn build_trigram_index(...)
L43:       let mut index = TrigramIndex::new();
L44:*      for trigram in content.trigrams() {
```

Lines marked with `*` are matches. The `L{N}:` prefix always appears. Context lines have spaces in the gutter, matches have `*`.

**Step 2: Implementation**

See the code block below for the full implementation. Key details:
- `format_search_results` groups matches by file with `## path` headers
- Match lines use `L{n}:*` gutter marker, context lines use `L{n}: `
- Summary line always comes first: "Found N matches across M files (showing X-Y)"
- Large result hint when total_file_count > 100
- Staleness warning prepended when provided
- `format_file_content` shows line-numbered content with metadata header
- `format_file_list` shows paths with language and size
- `format_index_status` shows segment/file counts

**Step 3: Run tests**

Run: `cargo test -p ferret-mcp`
Expected: All tests pass

**Step 4: Run clippy and fmt**

Run: `cargo clippy -p ferret-mcp -- -D warnings && cargo fmt --all -- --check`

**Step 5: Commit**

```bash
git add ferret-mcp/src/formatter.rs ferret-mcp/src/main.rs
git commit -m "feat(mcp): add plain-text response formatter for LLM-optimized output (HHC-63)"
```

---

### Task 4: Create tools/mod.rs placeholder and server.rs with FerretServer

**Files:**
- Create: `ferret-mcp/src/tools/mod.rs`
- Create: `ferret-mcp/src/server.rs`
- Modify: `ferret-mcp/src/main.rs` (add modules, update main function)

**Step 1: Create empty tools module**

Create `ferret-mcp/src/tools/mod.rs` as an empty placeholder:

```rust
//! MCP tool implementations.
//!
//! Phase 2 agents will add individual tool modules here:
//! - search_code
//! - search_files
//! - get_file
//! - index_status
//! - reindex
```

**Step 2: Create server.rs with FerretServer**

The server holds shared state (IndexState, root paths) and implements `ServerHandler` via the `#[tool(tool_box)]` macro. It declares tools and resources capabilities.

```rust
use std::sync::Arc;
use rmcp::{ServerHandler, tool};
use rmcp::model::{ServerCapabilities, ServerInfo, Implementation};
use ferret_indexer_core::IndexState;

#[derive(Clone)]
pub struct FerretServer {
    pub index_state: Arc<IndexState>,
    pub root_path: Option<std::path::PathBuf>,
}

impl FerretServer {
    pub fn new(index_state: Arc<IndexState>, root_path: Option<std::path::PathBuf>) -> Self {
        Self { index_state, root_path }
    }
}

#[tool(tool_box)]
impl FerretServer {
    // Phase 2 agents will add tool methods here
}

#[tool(tool_box)]
impl ServerHandler for FerretServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Code search index server. Use search_code to find code, \
                 search_files to find files by name, get_file to read file contents."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: "ferret".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            ..Default::default()
        }
    }
}
```

**Step 3: Update main.rs with stdio transport**

```rust
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;

pub mod errors;
pub mod formatter;
pub mod server;
pub mod tools;

use server::FerretServer;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let index_state = std::sync::Arc::new(ferret_indexer_core::IndexState::new());
    let server = FerretServer::new(index_state, None);

    let service = server
        .serve(stdio())
        .await
        .expect("failed to start MCP server");

    service.waiting().await.expect("MCP service error");
}
```

**Step 4: Verify it compiles**

Run: `cargo check -p ferret-mcp`
Expected: Compiles

**Step 5: Run clippy and fmt**

Run: `cargo clippy -p ferret-mcp -- -D warnings && cargo fmt --all -- --check`

**Step 6: Run all workspace tests**

Run: `cargo test --workspace`
Expected: All tests pass

**Step 7: Commit**

```bash
git add ferret-mcp/src/server.rs ferret-mcp/src/tools/mod.rs ferret-mcp/src/main.rs
git commit -m "feat(mcp): set up rmcp server with stdio transport and FerretServer (HHC-58)"
```

---

### Task 5: Final validation

**Step 1: Run full CI checks**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```
Expected: All pass

**Step 2: Final commit if any fixups needed**

---
