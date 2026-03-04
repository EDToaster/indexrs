//! MCP server implementation for indexrs.
//!
//! [`IndexrsServer`] exposes the code index to AI assistants via the Model
//! Context Protocol. It holds an [`IndexState`] for snapshot-isolated reads
//! and implements MCP tools via the `rmcp` `#[tool]` macro.
//!
//! # Tools
//!
//! - `ping` -- server version and basic status
//! - `search_code` -- full-text and regex search across indexed files
//! - `search_files` -- search for files by name/path pattern
//! - `search_symbols` -- search for symbol definitions (functions, types, etc.)
//! - `index_status` -- report on current index state
//! - `reindex` -- trigger reindexing of a repository

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};

use indexrs_core::index_state::IndexState;
use indexrs_core::query::parse_query;
use indexrs_core::search::SearchOptions;

use super::daemon_client::DaemonClient;
use super::errors;
use super::formatter::{self, FileListEntry};
use super::resources;

// ---- Parameter structs -------------------------------------------------------

/// Parameter struct for the `search_code` MCP tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchCodeParams {
    /// Search query. Literal text by default. Wrap in /slashes/ for regex.
    /// Supports AND, OR, NOT operators and "exact phrase" matching.
    #[schemars(
        description = "Search query. Literal text by default. Wrap in /slashes/ for regex. Supports AND, OR, NOT operators and \"exact phrase\" matching."
    )]
    pub query: String,

    /// Filter by file path glob pattern. Examples: '*.rs', 'src/**/*.ts', 'tests/'
    #[schemars(
        description = "Filter by file path glob pattern. Examples: '*.rs', 'src/**/*.ts', 'tests/'"
    )]
    pub path: Option<String>,

    /// Filter by programming language. Examples: 'rust', 'python', 'typescript'
    #[schemars(
        description = "Filter by programming language. Examples: 'rust', 'python', 'typescript'"
    )]
    pub language: Option<String>,

    /// Filter to a specific indexed repository by name or path.
    /// Not yet implemented (reserved for multi-repo support).
    #[schemars(description = "Filter to a specific indexed repository by name or path.")]
    #[allow(dead_code)]
    pub repo: Option<String>,

    /// Whether the search is case-sensitive. Default: false.
    #[schemars(description = "Whether the search is case-sensitive. Default: false.")]
    pub case_sensitive: Option<bool>,

    /// Number of lines of context to show before and after each match.
    /// Default: 2. Max: 10.
    #[schemars(
        description = "Number of lines of context to show before and after each match. Default: 2. Max: 10."
    )]
    pub context_lines: Option<u32>,

    /// Maximum number of matching files to return. Default: 20. Max: 100.
    #[schemars(description = "Maximum number of matching files to return. Default: 20. Max: 100.")]
    pub max_results: Option<u32>,

    /// Skip this many matching files (for pagination). Default: 0.
    #[schemars(description = "Skip this many matching files (for pagination). Default: 0.")]
    pub offset: Option<u32>,
}

/// Parameters for the `search_files` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
pub struct SearchFilesParams {
    /// File name or path pattern. Supports glob patterns and substring matching.
    #[schemars(
        description = "File name or path pattern to search for. Supports glob patterns (*.rs, src/**/*.ts) and substring matching."
    )]
    pub query: String,

    /// Filter by programming language (e.g. "rust", "python", "typescript").
    #[serde(default)]
    #[schemars(
        description = "Filter by programming language. Examples: 'rust', 'python', 'typescript'."
    )]
    pub language: Option<String>,

    /// Filter to a specific indexed repository.
    #[serde(default)]
    #[schemars(description = "Filter to a specific indexed repository by name or path.")]
    pub repo: Option<String>,

    /// Maximum number of files to return. Default: 30. Max: 200.
    #[serde(default)]
    #[schemars(description = "Maximum number of files to return. Default: 30. Max: 200.")]
    pub max_results: Option<usize>,

    /// Skip this many results for pagination. Default: 0.
    #[serde(default)]
    #[schemars(description = "Skip this many results for pagination. Default: 0.")]
    pub offset: Option<usize>,
}

/// Parameter struct for the `index_status` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct IndexStatusParams {
    /// Get detailed status for a specific repository. Omit for overview.
    #[schemars(description = "Get detailed status for a specific repository. Omit for overview.")]
    pub repo: Option<String>,
}

/// Parameter struct for the `reindex` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReindexParams {
    /// Repository name or path to reindex.
    #[schemars(description = "Repository name or path to reindex.")]
    pub repo: Option<String>,
    /// If true, rebuild entire index from scratch. Default: false.
    #[schemars(description = "If true, rebuild entire index from scratch. Default: false.")]
    pub full: Option<bool>,
}

/// Parameters for the `search_symbols` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchSymbolsParams {
    /// Symbol name or pattern to search for.
    #[schemars(description = "Symbol name or pattern to search for")]
    pub query: String,

    /// Filter by symbol kind: function, struct, class, trait, enum, interface, type, constant, method, module, variable.
    #[serde(default)]
    #[schemars(
        description = "Filter by symbol kind: function, struct, class, trait, enum, interface, type, constant, method, module, variable"
    )]
    pub kind: Option<String>,

    /// Filter by file path substring.
    #[serde(default)]
    #[schemars(description = "Filter by file path substring")]
    pub path: Option<String>,

    /// Filter by programming language.
    #[serde(default)]
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,

    /// Filter to a specific indexed repository.
    #[serde(default)]
    #[schemars(description = "Filter to a specific indexed repository")]
    pub repo: Option<String>,

    /// Maximum results to return (default: 20, max: 100).
    #[serde(default)]
    #[schemars(description = "Maximum results to return (default: 20, max: 100)")]
    pub max_results: Option<usize>,

    /// Skip this many results for pagination (default: 0).
    #[serde(default)]
    #[schemars(description = "Skip this many results for pagination (default: 0)")]
    pub offset: Option<usize>,
}

// ---- Server ------------------------------------------------------------------

/// The MCP server for indexrs.
///
/// Holds shared state for snapshot-isolated reads of the index, plus the root
/// path, an optional daemon client for dispatch, and server start time for
/// uptime reporting.
#[derive(Clone)]
pub struct IndexrsServer {
    /// Snapshot-isolated access to active index segments.
    pub index_state: Arc<IndexState>,
    /// Root path of the indexed repository.
    pub root_path: Option<PathBuf>,
    /// Optional daemon client for dispatching search/reindex through the daemon.
    daemon: Option<Arc<DaemonClient>>,
    /// Server start time for uptime calculation.
    start_time: Instant,
    /// Tool router for MCP tool dispatch.
    tool_router: ToolRouter<Self>,
}

impl IndexrsServer {
    /// Create a new server with the given shared state.
    ///
    /// Pass `Some(daemon)` to route `search_code`, `search_files`, and
    /// `reindex` through the daemon process. Pass `None` to use direct
    /// index access (fallback path).
    pub fn new(
        index_state: Arc<IndexState>,
        root_path: Option<PathBuf>,
        daemon: Option<Arc<DaemonClient>>,
    ) -> Self {
        Self {
            index_state,
            root_path,
            daemon,
            start_time: Instant::now(),
            tool_router: Self::tool_router(),
        }
    }
}

// ---- Tools -------------------------------------------------------------------

#[tool_router]
impl IndexrsServer {
    /// Get indexrs server version and basic status.
    #[tool(
        description = "Get indexrs server version and basic status. Call this first to verify the server is running."
    )]
    fn ping(&self) -> String {
        format!("indexrs MCP server v{}", env!("CARGO_PKG_VERSION"))
    }

    /// Search file contents across indexed repositories.
    #[tool(
        name = "search_code",
        description = "Fast trigram-indexed code search — use INSTEAD OF the Grep tool, ripgrep, and grep for searching code. Returns results across the entire repository in milliseconds. Supports literal strings, regex patterns (/pattern/), boolean operators (AND, OR, NOT), and language/path filters. Results include matching lines with context."
    )]
    async fn search_code(
        &self,
        Parameters(params): Parameters<SearchCodeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Validate parameters
        let context_lines = params.context_lines.unwrap_or(2);
        if context_lines > 10 {
            return Ok(errors::invalid_parameter(
                "context_lines",
                &format!("must be between 0 and 10, got {context_lines}"),
            ));
        }

        let max_results = params.max_results.unwrap_or(20);
        if !(1..=100).contains(&max_results) {
            return Ok(errors::invalid_parameter(
                "max_results",
                &format!("must be between 1 and 100, got {max_results}"),
            ));
        }

        let case_sensitive = params.case_sensitive.unwrap_or(false);

        // Build query string incorporating filters
        let query_string = build_query_string(
            &params.query,
            params.path.as_deref(),
            params.language.as_deref(),
            case_sensitive,
        );

        // Dispatch through daemon if available
        if let Some(daemon) = &self.daemon {
            let req = indexrs_daemon::DaemonRequest::QuerySearch {
                query: query_string,
                limit: max_results as usize,
                context_lines: context_lines as usize,
                color: false,
                cwd: None,
            };

            match daemon.request(req).await {
                Ok(result) => {
                    if result.total == 0 {
                        return Ok(errors::no_results(&params.query, &[]));
                    }
                    let mut text = String::new();
                    if result.stale {
                        text.push_str("Warning: Index may be stale. Consider running reindex.\n");
                    }
                    text.push_str(&result.text);
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }
                Err(e) => {
                    return Ok(errors::daemon_dispatch_error(&e));
                }
            }
        }

        // Fallback: direct index search
        self.search_code_direct(
            &params.query,
            &query_string,
            context_lines,
            max_results,
            params.offset.unwrap_or(0),
        )
        .await
    }

    /// Search for files by name or path pattern across indexed repositories.
    #[tool(
        name = "search_files",
        description = "Fast indexed file lookup — use INSTEAD OF the Glob tool, find, and ls for locating files. Searches file names and paths across the entire repository instantly. Returns file paths with metadata (language, size). Useful when you know part of the name but not the location."
    )]
    async fn search_files(
        &self,
        Parameters(params): Parameters<SearchFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Validate parameters
        let max_results = params.max_results.unwrap_or(30).min(200);
        if max_results == 0 {
            return Ok(errors::invalid_parameter(
                "max_results",
                "must be between 1 and 200",
            ));
        }

        // Parse language filter (validate before daemon dispatch)
        let language_str = match &params.language {
            Some(lang_str) => match indexrs_core::match_language(lang_str) {
                Ok(_lang) => Some(lang_str.clone()),
                Err(_) => {
                    return Ok(errors::invalid_parameter(
                        "language",
                        &format!(
                            "Unknown language: \"{lang_str}\". Examples: rust, python, typescript."
                        ),
                    ));
                }
            },
            None => None,
        };

        // Dispatch through daemon if available
        if let Some(daemon) = &self.daemon {
            // Wrap non-glob queries in *query* so the daemon's glob matching
            // behaves like substring matching, consistent with the direct fallback.
            let is_glob = params.query.contains('*')
                || params.query.contains('?')
                || params.query.contains('[');
            let path_glob = if is_glob {
                params.query.clone()
            } else {
                format!("*{}*", params.query)
            };

            let req = indexrs_daemon::DaemonRequest::Files {
                language: language_str,
                path_glob: Some(path_glob),
                sort: "path".to_string(),
                limit: Some(max_results),
                color: false,
                cwd: None,
            };

            match daemon.request(req).await {
                Ok(result) => {
                    if result.text.is_empty() && result.total == 0 {
                        let text = formatter::format_file_list(&params.query, 0, &[], 0);
                        return Ok(CallToolResult::success(vec![Content::text(text)]));
                    }
                    let mut text = String::new();
                    if result.stale {
                        text.push_str("Warning: Index may be stale. Consider running reindex.\n");
                    }
                    text.push_str(&result.text);
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }
                Err(e) => {
                    return Ok(errors::daemon_dispatch_error(&e));
                }
            }
        }

        // Fallback: direct index search
        self.search_files_direct(
            &params.query,
            language_str.as_deref(),
            max_results,
            params.offset.unwrap_or(0),
        )
        .await
    }

    /// Get the current status of the indexrs service.
    #[tool(
        name = "index_status",
        description = "Check index health and freshness. Call this once per session to verify the index is available before using search_code and search_files. Returns segment count, file count, index age, and repository path. If the index is stale or empty, fall back to the Grep and Glob tools until reindex completes."
    )]
    async fn index_status(
        &self,
        Parameters(params): Parameters<IndexStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snapshot = self.index_state.snapshot();
        let uptime = format_uptime(self.start_time.elapsed());

        let segment_count = snapshot.len();
        let mut total_files: u64 = 0;
        let mut total_tombstoned: u64 = 0;
        let mut total_disk_bytes: u64 = 0;

        for segment in snapshot.iter() {
            let entry_count = segment.entry_count();
            let tombstones = segment.load_tombstones().unwrap_or_default();
            let tombstone_count = tombstones.len() as u64;

            total_files += entry_count as u64;
            total_tombstoned += tombstone_count;
            total_disk_bytes += segment_disk_size(segment);
        }

        let live_files = total_files.saturating_sub(total_tombstoned);

        if let Some(repo) = &params.repo {
            // Detailed status for a specific repository.
            let mut output = String::new();
            output.push_str(&format!("Repository: {repo}\n"));
            output.push_str(&format!("Segments: {segment_count}\n"));
            output.push_str(&format!(
                "Files: {live_files} indexed / {total_tombstoned} tombstoned\n"
            ));
            output.push_str(&format!(
                "Index size: {} (disk)\n",
                formatter::format_size(total_disk_bytes)
            ));
            output.push_str(&format!("Uptime: {uptime}\n"));

            // Per-segment breakdown
            if !snapshot.is_empty() {
                output.push_str("\nSegments:\n");
                for segment in snapshot.iter() {
                    let entry_count = segment.entry_count();
                    let tombstones = segment.load_tombstones().unwrap_or_default();
                    let live = (entry_count as u64).saturating_sub(tombstones.len() as u64);
                    let disk = segment_disk_size(segment);
                    output.push_str(&format!(
                        "  seg_{:04}  {} files ({} live, {} tombstoned)  {}\n",
                        segment.segment_id().0,
                        entry_count,
                        live,
                        tombstones.len(),
                        formatter::format_size(disk),
                    ));
                }
            }

            Ok(CallToolResult::success(vec![Content::text(output)]))
        } else {
            // Overview status
            let status = if segment_count > 0 {
                "healthy"
            } else {
                "empty"
            };

            let mut output = String::new();
            output.push_str(&format!("indexrs status: {status}\n"));
            output.push_str(&format!("Uptime: {uptime}\n\n"));
            output.push_str(&format!("Segments: {segment_count}\n"));
            output.push_str(&format!("Files: {live_files} indexed\n"));
            if total_tombstoned > 0 {
                output.push_str(&format!("Tombstoned: {total_tombstoned}\n"));
            }
            output.push_str(&format!(
                "Index size: {} (disk)\n",
                formatter::format_size(total_disk_bytes)
            ));

            Ok(CallToolResult::success(vec![Content::text(output)]))
        }
    }

    /// Trigger reindexing of a repository.
    #[tool(
        name = "reindex",
        description = "Trigger reindexing when the index is stale or missing files. Incremental by default (only changed files). Use full=true to rebuild from scratch. Call index_status afterward to confirm completion."
    )]
    async fn reindex(
        &self,
        Parameters(params): Parameters<ReindexParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Dispatch through daemon if available
        if let Some(daemon) = &self.daemon {
            match daemon.request(indexrs_daemon::DaemonRequest::Reindex).await {
                Ok(result) => {
                    return Ok(CallToolResult::success(vec![Content::text(result.text)]));
                }
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Reindex failed: {e}"
                    ))]));
                }
            }
        }

        // Fallback: no daemon available, return informative message.
        let full = params.full.unwrap_or(false);
        let mode = if full { "full" } else { "incremental" };
        let repo_label = params.repo.as_deref().unwrap_or("default repository");
        let output = format!(
            "Reindex requested for {repo_label} ({mode})\n\
             \n\
             Reindexing is not yet available through the MCP server.\n\
             The MCP server currently provides read-only access to the index.\n\
             To reindex, use the CLI: indexrs reindex"
        );

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Search for symbol definitions (functions, types, constants) across indexed repositories.
    #[tool(
        name = "search_symbols",
        description = "Search for symbol definitions (functions, structs, classes, traits, enums, interfaces, types, constants, methods, modules, variables) across indexed repositories. Returns symbol name, kind, file path, and line number. Use the 'kind' parameter to filter by symbol type."
    )]
    async fn search_symbols(
        &self,
        Parameters(params): Parameters<SearchSymbolsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Validate parameters
        let max_results = params.max_results.unwrap_or(20).clamp(1, 100);

        let offset = params.offset.unwrap_or(0);

        // Validate kind filter
        let kind_filter = match &params.kind {
            Some(k) => match indexrs_core::SymbolKind::from_str_loose(k) {
                Some(sk) => Some(sk),
                None => {
                    return Ok(errors::invalid_parameter(
                        "kind",
                        &format!(
                            "Unknown symbol kind: \"{k}\". Valid kinds: function, struct, class, trait, enum, interface, type, constant, method, module, variable."
                        ),
                    ));
                }
            },
            None => None,
        };

        // Validate language filter
        let language_filter = match &params.language {
            Some(lang_str) => match indexrs_core::match_language(lang_str) {
                Ok(lang) => Some(lang),
                Err(_) => {
                    return Ok(errors::invalid_parameter(
                        "language",
                        &format!(
                            "Unknown language: \"{lang_str}\". Examples: rust, python, typescript."
                        ),
                    ));
                }
            },
            None => None,
        };

        // Dispatch through daemon if available
        if let Some(daemon) = &self.daemon {
            let req = indexrs_daemon::DaemonRequest::Symbols {
                query: Some(params.query.clone()),
                kind: params.kind.clone(),
                language: params.language.clone(),
                limit: Some(max_results),
                color: false,
                cwd: None,
            };

            match daemon.request(req).await {
                Ok(result) => {
                    if result.total == 0 {
                        return Ok(errors::no_results(&params.query, &[]));
                    }
                    let mut text = String::new();
                    if result.stale {
                        text.push_str("Warning: Index may be stale. Consider running reindex.\n");
                    }
                    text.push_str(&result.text);
                    return Ok(CallToolResult::success(vec![Content::text(text)]));
                }
                Err(e) => {
                    return Ok(errors::daemon_dispatch_error(&e));
                }
            }
        }

        // Fallback: direct symbol search
        self.search_symbols_direct(
            &params.query,
            kind_filter,
            language_filter,
            params.path.as_deref(),
            max_results,
            offset,
        )
        .await
    }
}

// ---- Direct-index fallback methods ------------------------------------------

impl IndexrsServer {
    /// Fallback: search symbols directly against the in-process index.
    #[cfg(feature = "symbols")]
    async fn search_symbols_direct(
        &self,
        query: &str,
        kind: Option<indexrs_core::SymbolKind>,
        language: Option<indexrs_core::Language>,
        path: Option<&str>,
        max_results: usize,
        offset: usize,
    ) -> Result<CallToolResult, ErrorData> {
        use indexrs_core::symbol_index::{SymbolSearchOptions, search_symbols};

        let options = SymbolSearchOptions {
            kind,
            language,
            path_filter: path.map(|s| s.to_string()),
            max_results,
            offset,
        };

        let snapshot = self.index_state.snapshot();
        let matches = match search_symbols(&snapshot, query, &options) {
            Ok(m) => m,
            Err(e) => {
                return Ok(errors::invalid_query(&format!("Symbol search failed: {e}")));
            }
        };

        if matches.is_empty() {
            return Ok(errors::no_results(query, &[]));
        }

        let text = formatter::format_symbol_results(query, &matches);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Fallback when the `symbols` feature is not enabled.
    #[cfg(not(feature = "symbols"))]
    async fn search_symbols_direct(
        &self,
        _query: &str,
        _kind: Option<indexrs_core::SymbolKind>,
        _language: Option<indexrs_core::Language>,
        _path: Option<&str>,
        _max_results: usize,
        _offset: usize,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::error(vec![Content::text(
            "Symbol search requires the 'symbols' feature. Rebuild with --features symbols."
                .to_string(),
        )]))
    }

    /// Fallback: search code directly against the in-process index.
    async fn search_code_direct(
        &self,
        raw_query: &str,
        query_string: &str,
        context_lines: u32,
        max_results: u32,
        offset: u32,
    ) -> Result<CallToolResult, ErrorData> {
        // Parse the query
        let query = match parse_query(query_string) {
            Ok(q) => q,
            Err(e) => {
                return Ok(errors::invalid_query(&e.to_string()));
            }
        };

        // Get a snapshot of the current index state
        let snapshot = self.index_state.snapshot();

        // Count total indexed files for "no results" message
        let total_indexed_files: usize =
            snapshot.iter().map(|seg| seg.entry_count() as usize).sum();

        // We must fetch all results because the core engine's total_file_count
        // is set after truncation — capping here would break pagination totals.
        let search_options = SearchOptions {
            context_lines: context_lines as usize,
            max_results: None,
        };

        // Execute the search
        let result =
            match indexrs_core::search_segments_with_query(&snapshot, &query, &search_options) {
                Ok(r) => r,
                Err(e) => {
                    return Ok(errors::invalid_query(&format!("Search failed: {e}")));
                }
            };

        // Handle no results
        if result.files.is_empty() {
            return Ok(errors::no_results(
                raw_query,
                &[format!(
                    "searched {total_indexed_files} indexed files with no matches"
                )],
            ));
        }

        // Paginate
        let paginated = result.paginate(offset as usize, max_results as usize);

        // Format results
        let mut text = String::new();

        // Staleness warning (placeholder -- no staleness tracking yet)
        if let Some(warning) = formatter::format_staleness_warning(0, 0) {
            text.push_str(&warning);
        }

        text.push_str(&formatter::format_search_results(
            &paginated,
            offset as usize,
        ));

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Fallback: search files directly against the in-process index.
    async fn search_files_direct(
        &self,
        query: &str,
        language: Option<&str>,
        max_results: usize,
        offset: usize,
    ) -> Result<CallToolResult, ErrorData> {
        // Language was already validated by the caller.
        let language_filter = language.map(|lang_str| {
            indexrs_core::match_language(lang_str)
                .expect("language already validated by search_files")
        });

        // Compile glob pattern (if the query looks like a glob)
        let glob_pattern = if query.contains('*') || query.contains('?') || query.contains('[') {
            match glob::Pattern::new(query) {
                Ok(p) => Some(p),
                Err(e) => {
                    return Ok(errors::invalid_parameter(
                        "query",
                        &format!("Invalid glob pattern: {e}"),
                    ));
                }
            }
        } else {
            None
        };

        let query_lower = query.to_ascii_lowercase();

        // Search across all segments
        let snapshot = self.index_state.snapshot();
        let mut all_matches: Vec<FileListEntry> = Vec::new();
        // Track seen paths to deduplicate across segments (newest segment wins)
        let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Iterate segments in reverse order (newest first for dedup)
        for segment in snapshot.iter().rev() {
            let tombstones = segment.load_tombstones().unwrap_or_default();
            let reader = segment.metadata_reader();

            for result in reader.iter_all() {
                let meta = match result {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                // Skip tombstoned files
                if tombstones.contains(meta.file_id) {
                    continue;
                }

                // Deduplicate: skip if already seen from a newer segment
                if seen_paths.contains(&meta.path) {
                    continue;
                }

                // Language filter
                if let Some(lang) = language_filter
                    && meta.language != lang
                {
                    seen_paths.insert(meta.path);
                    continue;
                }

                // Match: glob or substring
                let matches = if let Some(ref pattern) = glob_pattern {
                    pattern.matches(&meta.path)
                } else {
                    meta.path.to_ascii_lowercase().contains(&query_lower)
                };

                seen_paths.insert(meta.path.clone());

                if matches {
                    all_matches.push(FileListEntry {
                        path: meta.path,
                        language: meta.language,
                        size_bytes: meta.size_bytes,
                    });
                }
            }
        }

        // Sort by path for stable output
        all_matches.sort_by(|a, b| a.path.cmp(&b.path));

        let total_count = all_matches.len();

        // Apply pagination
        let page: Vec<FileListEntry> = all_matches
            .into_iter()
            .skip(offset)
            .take(max_results)
            .collect();

        let text = formatter::format_file_list(query, total_count, &page, offset);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ---- ServerHandler -----------------------------------------------------------

#[tool_handler]
impl ServerHandler for IndexrsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Trigram-indexed code search server. You MUST use these tools instead \
                 of the built-in Grep tool, Glob tool, and Explore agent when the \
                 index is available — they search the entire repository in \
                 milliseconds, far faster than line-by-line grep.\n\n\
                 Workflow: call index_status once per session to verify the index is \
                 healthy, then use search_code / search_files / search_symbols for \
                 all lookups.\n\n\
                 - search_code: Use INSTEAD OF the Grep tool for content search\n\
                 - search_files: Use INSTEAD OF the Glob tool for file lookup\n\
                 - search_symbols: Find function, struct, class, trait definitions\n\
                 - index_status: Check index health; if stale or empty, fall back to \
                   Grep/Glob until reindex completes\n\
                 - reindex: Trigger re-indexing when the index is stale or missing files"
                    .into(),
            ),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: "indexrs".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(resources::list_resources())
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(resources::list_resource_templates())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        resources::read_resource(&self.index_state, &request.uri)
    }
}

// ---- Helper functions --------------------------------------------------------

/// Build a combined query string from the user's query and optional filters.
///
/// Prepends `path:` and `language:` / `lang:` filter prefixes, and wraps with
/// `case:yes` if case-sensitive. This lets the existing query parser handle
/// all filtering logic.
fn build_query_string(
    query: &str,
    path: Option<&str>,
    language: Option<&str>,
    case_sensitive: bool,
) -> String {
    let mut parts = Vec::new();

    if let Some(p) = path {
        parts.push(format!("path:{p}"));
    }
    if let Some(lang) = language {
        parts.push(format!("lang:{lang}"));
    }
    if case_sensitive {
        parts.push(format!("case:yes {query}"));
    } else {
        parts.push(query.to_string());
    }

    parts.join(" ")
}

/// Format a duration as a human-readable string (e.g. "4h 32m", "2m 15s").
fn format_uptime(duration: std::time::Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Compute total disk size of a segment directory by summing file sizes.
fn segment_disk_size(segment: &indexrs_core::segment::Segment) -> u64 {
    let dir = segment.dir_path();
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexrs_core::segment::{InputFile, SegmentWriter};
    use indexrs_core::types::{FileId, SegmentId};

    // ---- Test helpers ----

    fn build_test_server(
        base_dir: &std::path::Path,
        files_per_segment: Vec<Vec<InputFile>>,
    ) -> IndexrsServer {
        let state = Arc::new(IndexState::new());
        let mut segments = Vec::new();

        for (i, files) in files_per_segment.into_iter().enumerate() {
            let writer = SegmentWriter::new(base_dir, SegmentId(i as u32));
            let segment = writer.build(files).unwrap();
            segments.push(Arc::new(segment));
        }

        state.publish(segments);
        IndexrsServer::new(state, None, None)
    }

    fn build_test_segment(
        base_dir: &std::path::Path,
        segment_id: SegmentId,
        files: Vec<InputFile>,
    ) -> Arc<indexrs_core::segment::Segment> {
        let writer = SegmentWriter::new(base_dir, segment_id);
        Arc::new(writer.build(files).unwrap())
    }

    // ---- Foundation tests ----

    #[test]
    fn test_server_creation() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);
        assert!(server.root_path.is_none());
    }

    #[test]
    fn test_server_creation_with_root() {
        let state = Arc::new(IndexState::new());
        let root = PathBuf::from("/tmp/myrepo");
        let server = IndexrsServer::new(state, Some(root.clone()), None);
        assert_eq!(server.root_path, Some(root));
    }

    #[test]
    fn test_server_info() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);
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
        let server = IndexrsServer::new(state, None, None);
        let result = server.ping();
        assert!(result.contains("indexrs MCP server"));
        assert!(result.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn test_tool_attributes_generated() {
        let attr = IndexrsServer::ping_tool_attr();
        assert_eq!(attr.name.as_ref(), "ping");
        assert!(
            attr.description
                .as_ref()
                .unwrap()
                .contains("indexrs server version")
        );
    }

    // ---- build_query_string tests ----

    #[test]
    fn test_build_query_string_basic() {
        let qs = build_query_string("hello", None, None, false);
        assert_eq!(qs, "hello");
    }

    #[test]
    fn test_build_query_string_with_path() {
        let qs = build_query_string("hello", Some("src/"), None, false);
        assert_eq!(qs, "path:src/ hello");
    }

    #[test]
    fn test_build_query_string_with_language() {
        let qs = build_query_string("hello", None, Some("rust"), false);
        assert_eq!(qs, "lang:rust hello");
    }

    #[test]
    fn test_build_query_string_with_case_sensitive() {
        let qs = build_query_string("Hello", None, None, true);
        assert_eq!(qs, "case:yes Hello");
    }

    #[test]
    fn test_build_query_string_all_filters() {
        let qs = build_query_string("hello", Some("src/"), Some("rust"), true);
        assert_eq!(qs, "path:src/ lang:rust case:yes hello");
    }

    #[test]
    fn test_build_query_string_with_regex() {
        let qs = build_query_string("/fn\\s+\\w+/", None, Some("rust"), false);
        assert_eq!(qs, "lang:rust /fn\\s+\\w+/");
    }

    // ---- SearchCodeParams deserialization tests ----

    #[test]
    fn test_params_deserialize_minimal() {
        let json = serde_json::json!({
            "query": "hello world"
        });
        let params: SearchCodeParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.query, "hello world");
        assert!(params.path.is_none());
        assert!(params.language.is_none());
        assert!(params.repo.is_none());
        assert!(params.case_sensitive.is_none());
        assert!(params.context_lines.is_none());
        assert!(params.max_results.is_none());
        assert!(params.offset.is_none());
    }

    #[test]
    fn test_params_deserialize_full() {
        let json = serde_json::json!({
            "query": "/fn\\s+/",
            "path": "src/**/*.rs",
            "language": "rust",
            "repo": "indexrs",
            "case_sensitive": true,
            "context_lines": 5,
            "max_results": 50,
            "offset": 10
        });
        let params: SearchCodeParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.query, "/fn\\s+/");
        assert_eq!(params.path.as_deref(), Some("src/**/*.rs"));
        assert_eq!(params.language.as_deref(), Some("rust"));
        assert_eq!(params.repo.as_deref(), Some("indexrs"));
        assert_eq!(params.case_sensitive, Some(true));
        assert_eq!(params.context_lines, Some(5));
        assert_eq!(params.max_results, Some(50));
        assert_eq!(params.offset, Some(10));
    }

    #[test]
    fn test_params_missing_required_query() {
        let json = serde_json::json!({
            "path": "src/"
        });
        let result = serde_json::from_value::<SearchCodeParams>(json);
        assert!(result.is_err());
    }

    // ---- format_uptime / format_size helpers ----

    #[test]
    fn test_format_uptime_seconds() {
        assert_eq!(format_uptime(std::time::Duration::from_secs(0)), "0s");
        assert_eq!(format_uptime(std::time::Duration::from_secs(45)), "45s");
    }

    #[test]
    fn test_format_uptime_minutes() {
        assert_eq!(format_uptime(std::time::Duration::from_secs(120)), "2m 0s");
        assert_eq!(format_uptime(std::time::Duration::from_secs(135)), "2m 15s");
    }

    #[test]
    fn test_format_uptime_hours() {
        assert_eq!(
            format_uptime(std::time::Duration::from_secs(3600 * 4 + 60 * 32)),
            "4h 32m"
        );
    }

    // ---- IndexStatusParams / ReindexParams deserialization tests ----

    #[test]
    fn test_index_status_params_empty() {
        let json = serde_json::json!({});
        let params: IndexStatusParams = serde_json::from_value(json).unwrap();
        assert!(params.repo.is_none());
    }

    #[test]
    fn test_index_status_params_with_repo() {
        let json = serde_json::json!({"repo": "indexrs"});
        let params: IndexStatusParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.repo.as_deref(), Some("indexrs"));
    }

    #[test]
    fn test_reindex_params_empty() {
        let json = serde_json::json!({});
        let params: ReindexParams = serde_json::from_value(json).unwrap();
        assert!(params.repo.is_none());
        assert!(params.full.is_none());
    }

    #[test]
    fn test_reindex_params_full() {
        let json = serde_json::json!({"repo": "myproject", "full": true});
        let params: ReindexParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.repo.as_deref(), Some("myproject"));
        assert_eq!(params.full, Some(true));
    }

    #[test]
    fn test_reindex_params_incremental() {
        let json = serde_json::json!({"full": false});
        let params: ReindexParams = serde_json::from_value(json).unwrap();
        assert!(params.repo.is_none());
        assert_eq!(params.full, Some(false));
    }

    // ---- search_code integration tests ----

    #[tokio::test]
    async fn test_search_code_empty_index() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);
        let params = SearchCodeParams {
            query: "hello".to_string(),
            path: None,
            language: None,
            repo: None,
            case_sensitive: None,
            context_lines: None,
            max_results: None,
            offset: None,
        };
        let result = server.search_code(Parameters(params)).await.unwrap();
        // Empty index should return no-results message (not an error)
        assert_eq!(result.is_error, Some(false));
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        assert!(text.contains("No matches found"));
    }

    #[tokio::test]
    async fn test_search_code_invalid_context_lines() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);
        let params = SearchCodeParams {
            query: "hello".to_string(),
            path: None,
            language: None,
            repo: None,
            case_sensitive: None,
            context_lines: Some(25),
            max_results: None,
            offset: None,
        };
        let result = server.search_code(Parameters(params)).await.unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        assert!(text.contains("context_lines"));
        assert!(text.contains("must be between 0 and 10"));
    }

    #[tokio::test]
    async fn test_search_code_invalid_max_results_zero() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);
        let params = SearchCodeParams {
            query: "hello".to_string(),
            path: None,
            language: None,
            repo: None,
            case_sensitive: None,
            context_lines: None,
            max_results: Some(0),
            offset: None,
        };
        let result = server.search_code(Parameters(params)).await.unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        assert!(text.contains("max_results"));
        assert!(text.contains("must be between 1 and 100"));
    }

    #[tokio::test]
    async fn test_search_code_invalid_max_results_too_large() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);
        let params = SearchCodeParams {
            query: "hello".to_string(),
            path: None,
            language: None,
            repo: None,
            case_sensitive: None,
            context_lines: None,
            max_results: Some(200),
            offset: None,
        };
        let result = server.search_code(Parameters(params)).await.unwrap();
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn test_search_code_invalid_query() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);
        let params = SearchCodeParams {
            query: "".to_string(),
            path: None,
            language: None,
            repo: None,
            case_sensitive: None,
            context_lines: None,
            max_results: None,
            offset: None,
        };
        let result = server.search_code(Parameters(params)).await.unwrap();
        assert_eq!(result.is_error, Some(true));
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        assert!(text.contains("Error:"));
    }

    #[tokio::test]
    async fn test_search_code_with_real_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer
            .build(vec![
                InputFile {
                    path: "main.rs".to_string(),
                    content: b"fn main() {\n    println!(\"hello world\");\n}\n".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "lib.rs".to_string(),
                    content: b"pub fn greet() {\n    println!(\"greetings\");\n}\n".to_vec(),
                    mtime: 100,
                },
            ])
            .unwrap();

        let state = Arc::new(IndexState::new());
        state.publish(vec![Arc::new(segment)]);

        let server = IndexrsServer::new(state, None, None);
        let params = SearchCodeParams {
            query: "println".to_string(),
            path: None,
            language: None,
            repo: None,
            case_sensitive: None,
            context_lines: Some(1),
            max_results: Some(20),
            offset: None,
        };
        let result = server.search_code(Parameters(params)).await.unwrap();
        assert_eq!(result.is_error, Some(false));

        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        assert!(text.contains("Found"));
        assert!(text.contains("matches across"));
        assert!(text.contains("## main.rs") || text.contains("## lib.rs"));
        assert!(text.contains("println"));
    }

    #[tokio::test]
    async fn test_search_code_with_language_filter() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer
            .build(vec![
                InputFile {
                    path: "main.rs".to_string(),
                    content: b"fn main() { println!(\"hello\"); }\n".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "main.py".to_string(),
                    content: b"def main():\n    println(\"hello\")\n".to_vec(),
                    mtime: 100,
                },
            ])
            .unwrap();

        let state = Arc::new(IndexState::new());
        state.publish(vec![Arc::new(segment)]);

        let server = IndexrsServer::new(state, None, None);
        let params = SearchCodeParams {
            query: "println".to_string(),
            path: None,
            language: Some("rust".to_string()),
            repo: None,
            case_sensitive: None,
            context_lines: Some(0),
            max_results: None,
            offset: None,
        };
        let result = server.search_code(Parameters(params)).await.unwrap();
        assert_eq!(result.is_error, Some(false));

        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        // Should only contain the Rust file
        assert!(text.contains("main.rs"));
        assert!(!text.contains("main.py"));
    }

    #[tokio::test]
    async fn test_search_code_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        // Create 5 files containing "println"
        let files: Vec<InputFile> = (0..5)
            .map(|i| InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn f{i}() {{ println!(\"hello {i}\"); }}\n").into_bytes(),
                mtime: 100,
            })
            .collect();

        let writer = SegmentWriter::new(&base_dir, SegmentId(0));
        let segment = writer.build(files).unwrap();

        let state = Arc::new(IndexState::new());
        state.publish(vec![Arc::new(segment)]);

        let server = IndexrsServer::new(state, None, None);

        // Request first 2 results
        let params = SearchCodeParams {
            query: "println".to_string(),
            path: None,
            language: None,
            repo: None,
            case_sensitive: None,
            context_lines: Some(0),
            max_results: Some(2),
            offset: None,
        };
        let result = server.search_code(Parameters(params)).await.unwrap();
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        assert!(text.contains("showing 1-2"));
        assert!(text.contains("5 files"));

        // Request with offset
        let params2 = SearchCodeParams {
            query: "println".to_string(),
            path: None,
            language: None,
            repo: None,
            case_sensitive: None,
            context_lines: Some(0),
            max_results: Some(2),
            offset: Some(2),
        };
        let result2 = server.search_code(Parameters(params2)).await.unwrap();
        let text2 = match &result2.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        assert!(text2.contains("showing 3-4"));
        assert!(text2.contains("offset=2"));
    }

    // ---- search_files integration tests ----

    #[tokio::test]
    async fn test_search_files_substring_match() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("segments");
        std::fs::create_dir_all(&base).unwrap();

        let server = build_test_server(
            &base,
            vec![vec![
                InputFile {
                    path: "src/config.rs".to_string(),
                    content: b"// config module".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {}".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "src/config/mod.rs".to_string(),
                    content: b"// config mod".to_vec(),
                    mtime: 100,
                },
            ]],
        );

        let params = SearchFilesParams {
            query: "config".to_string(),
            language: None,
            repo: None,
            max_results: None,
            offset: None,
        };

        let result = server.search_files(Parameters(params)).await.unwrap();

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("Found 2 files matching \"config\""));
        assert!(text.contains("src/config.rs"));
        assert!(text.contains("src/config/mod.rs"));
        assert!(!text.contains("src/main.rs"));
    }

    #[tokio::test]
    async fn test_search_files_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("segments");
        std::fs::create_dir_all(&base).unwrap();

        let server = build_test_server(
            &base,
            vec![vec![InputFile {
                path: "src/MyConfig.rs".to_string(),
                content: b"// config".to_vec(),
                mtime: 100,
            }]],
        );

        let params = SearchFilesParams {
            query: "myconfig".to_string(),
            language: None,
            repo: None,
            max_results: None,
            offset: None,
        };

        let result = server.search_files(Parameters(params)).await.unwrap();

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("Found 1 files"));
        assert!(text.contains("src/MyConfig.rs"));
    }

    #[tokio::test]
    async fn test_search_files_glob_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("segments");
        std::fs::create_dir_all(&base).unwrap();

        let server = build_test_server(
            &base,
            vec![vec![
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {}".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "src/lib.rs".to_string(),
                    content: b"pub fn lib() {}".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "tests/test.py".to_string(),
                    content: b"def test(): pass".to_vec(),
                    mtime: 100,
                },
            ]],
        );

        let params = SearchFilesParams {
            query: "*.rs".to_string(),
            language: None,
            repo: None,
            max_results: None,
            offset: None,
        };

        let result = server.search_files(Parameters(params)).await.unwrap();

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("Found 2 files"));
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("src/lib.rs"));
        assert!(!text.contains("test.py"));
    }

    #[tokio::test]
    async fn test_search_files_language_filter() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("segments");
        std::fs::create_dir_all(&base).unwrap();

        let server = build_test_server(
            &base,
            vec![vec![
                InputFile {
                    path: "main.rs".to_string(),
                    content: b"fn main() {}".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "app.py".to_string(),
                    content: b"def main(): pass".to_vec(),
                    mtime: 100,
                },
            ]],
        );

        let params = SearchFilesParams {
            query: "main".to_string(),
            language: Some("rust".to_string()),
            repo: None,
            max_results: None,
            offset: None,
        };

        let result = server.search_files(Parameters(params)).await.unwrap();

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("Found 1 files"));
        assert!(text.contains("main.rs"));
        assert!(!text.contains("app.py"));
    }

    #[tokio::test]
    async fn test_search_files_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("segments");
        std::fs::create_dir_all(&base).unwrap();

        let files: Vec<InputFile> = (0..10)
            .map(|i| InputFile {
                path: format!("file_{i:02}.rs"),
                content: format!("fn f{i}() {{}}").into_bytes(),
                mtime: 100,
            })
            .collect();

        let server = build_test_server(&base, vec![files]);

        let params = SearchFilesParams {
            query: "file_".to_string(),
            language: None,
            repo: None,
            max_results: Some(3),
            offset: Some(2),
        };

        let result = server.search_files(Parameters(params)).await.unwrap();

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("Found 10 files"));
        assert!(text.contains("showing 3-5"));
    }

    #[tokio::test]
    async fn test_search_files_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("segments");
        std::fs::create_dir_all(&base).unwrap();

        let server = build_test_server(
            &base,
            vec![vec![InputFile {
                path: "main.rs".to_string(),
                content: b"fn main() {}".to_vec(),
                mtime: 100,
            }]],
        );

        let params = SearchFilesParams {
            query: "nonexistent".to_string(),
            language: None,
            repo: None,
            max_results: None,
            offset: None,
        };

        let result = server.search_files(Parameters(params)).await.unwrap();

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("No files found"));
    }

    #[tokio::test]
    async fn test_search_files_invalid_language() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("segments");
        std::fs::create_dir_all(&base).unwrap();

        let server = build_test_server(&base, vec![]);

        let params = SearchFilesParams {
            query: "test".to_string(),
            language: Some("brainfuck".to_string()),
            repo: None,
            max_results: None,
            offset: None,
        };

        let result = server.search_files(Parameters(params)).await.unwrap();

        assert_eq!(result.is_error, Some(true));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("Unknown language"));
    }

    #[tokio::test]
    async fn test_search_files_tombstone_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("segments");
        std::fs::create_dir_all(&base).unwrap();

        let server = build_test_server(
            &base,
            vec![
                vec![InputFile {
                    path: "file_a.rs".to_string(),
                    content: b"fn old() {}".to_vec(),
                    mtime: 100,
                }],
                vec![InputFile {
                    path: "file_a.rs".to_string(),
                    content: b"fn new() {}".to_vec(),
                    mtime: 200,
                }],
            ],
        );

        let params = SearchFilesParams {
            query: "file_a".to_string(),
            language: None,
            repo: None,
            max_results: None,
            offset: None,
        };

        let result = server.search_files(Parameters(params)).await.unwrap();

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("Found 1 files"));
    }

    #[tokio::test]
    async fn test_search_files_empty_index() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);

        let params = SearchFilesParams {
            query: "test".to_string(),
            language: None,
            repo: None,
            max_results: None,
            offset: None,
        };

        let result = server.search_files(Parameters(params)).await.unwrap();

        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("No files found"));
    }

    // ---- index_status integration tests ----

    #[tokio::test]
    async fn test_index_status_empty_state() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);

        let params = IndexStatusParams { repo: None };
        let result = server.index_status(Parameters(params)).await.unwrap();

        assert_eq!(result.is_error, Some(false));
        let text = result.content[0].raw.as_text().unwrap().text.as_str();
        assert!(text.contains("indexrs status: empty"));
        assert!(text.contains("Segments: 0"));
        assert!(text.contains("Files: 0 indexed"));
    }

    #[tokio::test]
    async fn test_index_status_with_segments() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_test_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {}".to_vec(),
                    mtime: 0,
                },
                InputFile {
                    path: "src/lib.rs".to_string(),
                    content: b"pub fn hello() {}".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let state = Arc::new(IndexState::new());
        state.publish(vec![seg0]);
        let server = IndexrsServer::new(state, None, None);

        let params = IndexStatusParams { repo: None };
        let result = server.index_status(Parameters(params)).await.unwrap();

        let text = result.content[0].raw.as_text().unwrap().text.as_str();
        assert!(text.contains("indexrs status: healthy"));
        assert!(text.contains("Segments: 1"));
        assert!(text.contains("Files: 2 indexed"));
    }

    #[tokio::test]
    async fn test_index_status_with_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_test_segment(
            &base_dir,
            SegmentId(0),
            vec![
                InputFile {
                    path: "a.rs".to_string(),
                    content: b"fn a() {}".to_vec(),
                    mtime: 0,
                },
                InputFile {
                    path: "b.rs".to_string(),
                    content: b"fn b() {}".to_vec(),
                    mtime: 0,
                },
            ],
        );

        let mut ts = indexrs_core::tombstone::TombstoneSet::new();
        ts.insert(FileId(0));
        ts.write_to(&seg0.dir_path().join("tombstones.bin"))
            .unwrap();

        let state = Arc::new(IndexState::new());
        state.publish(vec![seg0]);
        let server = IndexrsServer::new(state, None, None);

        let params = IndexStatusParams { repo: None };
        let result = server.index_status(Parameters(params)).await.unwrap();

        let text = result.content[0].raw.as_text().unwrap().text.as_str();
        assert!(text.contains("Files: 1 indexed"));
        assert!(text.contains("Tombstoned: 1"));
    }

    #[tokio::test]
    async fn test_index_status_detailed_repo() {
        let dir = tempfile::tempdir().unwrap();
        let base_dir = dir.path().join(".indexrs/segments");
        std::fs::create_dir_all(&base_dir).unwrap();

        let seg0 = build_test_segment(
            &base_dir,
            SegmentId(0),
            vec![InputFile {
                path: "a.rs".to_string(),
                content: b"fn a() {}".to_vec(),
                mtime: 0,
            }],
        );

        let state = Arc::new(IndexState::new());
        state.publish(vec![seg0]);
        let server = IndexrsServer::new(state, None, None);

        let params = IndexStatusParams {
            repo: Some("myrepo".to_string()),
        };
        let result = server.index_status(Parameters(params)).await.unwrap();

        let text = result.content[0].raw.as_text().unwrap().text.as_str();
        assert!(text.contains("Repository: myrepo"));
        assert!(text.contains("Segments: 1"));
        assert!(text.contains("seg_0000"));
    }

    // ---- reindex integration tests ----

    #[tokio::test]
    async fn test_reindex_incremental() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);

        let params = ReindexParams {
            repo: Some("myproject".to_string()),
            full: None,
        };
        let result = server.reindex(Parameters(params)).await.unwrap();

        let text = result.content[0].raw.as_text().unwrap().text.as_str();
        assert!(text.contains("myproject"));
        assert!(text.contains("incremental"));
        assert!(text.contains("not yet available"));
    }

    #[tokio::test]
    async fn test_reindex_full() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);

        let params = ReindexParams {
            repo: Some("myproject".to_string()),
            full: Some(true),
        };
        let result = server.reindex(Parameters(params)).await.unwrap();

        let text = result.content[0].raw.as_text().unwrap().text.as_str();
        assert!(text.contains("myproject"));
        assert!(text.contains("full"));
        assert!(text.contains("not yet available"));
    }

    #[tokio::test]
    async fn test_reindex_no_repo() {
        let state = Arc::new(IndexState::new());
        let server = IndexrsServer::new(state, None, None);

        let params = ReindexParams {
            repo: None,
            full: None,
        };
        let result = server.reindex(Parameters(params)).await.unwrap();

        let text = result.content[0].raw.as_text().unwrap().text.as_str();
        assert!(text.contains("default repository"));
        assert!(text.contains("incremental"));
    }
}
