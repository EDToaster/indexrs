# Multi-Repo Support Design

ferret currently operates on a single repository at a time. Multi-repo support adds a lightweight registry so the web UI can switch between repos, while the CLI continues to infer the current repo from the working directory.

**Key constraint:** no cross-repo search. Each search targets exactly one repo. The CLI, MCP server, and web UI all operate on one repo at a time.

---

## 1. Repo Registry

### Config file: `~/.config/ferret/repos.toml`

A simple list of known repositories:

```toml
[[repo]]
name = "ferret"
path = "/Users/howard/src/ferret"

[[repo]]
name = "frontend"
path = "/Users/howard/src/frontend"

[[repo]]
name = "api"
path = "/Users/howard/work/api"
```

Each entry has:

| Field  | Required | Description |
|--------|----------|-------------|
| `name` | no       | Human-readable identifier. Defaults to the directory name if omitted. |
| `path` | yes      | Absolute path to the repo root (must contain `.ferret_index/`). |

Names must be unique. If two repos would have the same auto-derived name (e.g., `~/work/api` and `~/personal/api`), the second `ferret init` prints a warning and requires an explicit `--name` override.

### Config parsing

```rust
#[derive(Debug, Deserialize, Serialize)]
struct RepoConfig {
    repo: Vec<RepoEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RepoEntry {
    name: Option<String>,
    path: PathBuf,
}
```

The config directory is created on first write. If the file doesn't exist, the registry is empty (not an error). The file is read on startup and not watched for live changes -- restart the web server to pick up config changes.

---

## 2. Registration

### Auto-registration on `ferret init`

When `ferret init` builds the index for a repo, it also registers the repo in the config file:

1. Derive name from directory: `/Users/howard/src/ferret` → `"ferret"`
2. Check for name collision in existing config
3. Append `[[repo]]` entry to `repos.toml` (create file if needed)
4. Print: `Registered repo "ferret" in ~/.config/ferret/repos.toml`

If a repo with the same path already exists in the config, skip registration silently (idempotent).

### Manual management commands

```
ferret repos list              # List registered repos (name, path, status)
ferret repos add <path>        # Register a repo (--name override optional)
ferret repos remove <name>     # Unregister (does not delete .ferret_index/)
```

`repos add` validates that the path exists and contains `.ferret_index/` (i.e., the repo has been initialized). `repos remove` only edits the config -- it does not delete the index or stop a running daemon.

### CLI args

```rust
#[derive(Debug, Subcommand)]
pub enum Command {
    // ... existing commands ...

    /// Manage registered repositories
    Repos {
        #[command(subcommand)]
        action: ReposAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum ReposAction {
    /// List registered repositories
    List,
    /// Register a repository
    Add {
        /// Path to the repository root
        path: PathBuf,
        /// Override the auto-derived name
        #[arg(long)]
        name: Option<String>,
    },
    /// Unregister a repository (does not delete index data)
    Remove {
        /// Repository name
        name: String,
    },
}
```

---

## 3. Architecture

### Nothing changes about per-repo indexing

Each repo continues to have its own independent:

- `.ferret_index/` directory with segments, lock file, checkpoint
- `SegmentManager` (owned by the daemon)
- `HybridDetector` (file watcher + git diff)
- Unix socket at `.ferret_index/sock`

The registry is purely a discovery mechanism -- it tells the web server which repos exist. It has no effect on how indexing works.

### Web server: proxy to per-repo daemons

The web server reads the registry on startup and holds a `RepoRegistry`:

```rust
struct RepoRegistry {
    repos: HashMap<String, RepoInfo>,
}

struct RepoInfo {
    name: String,
    root: PathBuf,
}
```

When a request comes in for repo `"ferret"`, the web server:

1. Looks up `root` in the registry
2. Calls `ensure_daemon(root)` to get a Unix socket connection
3. Sends the appropriate `DaemonRequest`
4. Streams the response back as HTTP (JSON or HTML)

The web server is stateless with respect to index data. All search, file retrieval, and index management is delegated to the per-repo daemon.

### CLI: `--repo` resolves names from the registry

The `--repo` flag gains name-based resolution. When a value is passed:

1. Look up the value as a **repo name** in `~/.config/ferret/repos.toml`
2. If found, use the config's `path` as the repo root
3. If not found, treat the value as a **filesystem path** (existing behavior -- walk it looking for `.ferret_index/`)
4. If no `--repo` is passed, infer from CWD as today

This means `ferret search --repo ferret "fn main"` works without knowing the full path, while `ferret search --repo /tmp/some-project "fn main"` still works for unregistered repos.

```rust
fn resolve_repo(flag: Option<&str>) -> Result<PathBuf, IndexError> {
    match flag {
        Some(value) => {
            // Try name lookup first.
            if let Some(entry) = load_config()?.find_by_name(value) {
                return Ok(entry.path.clone());
            }
            // Fall back to path.
            let path = PathBuf::from(value);
            if path.join(".ferret_index").exists() {
                Ok(path)
            } else {
                Err(IndexError::RepoNotFound(value.to_string()))
            }
        }
        None => find_repo_root_from_cwd(),
    }
}
```

The `--repo` flag type changes from `PathBuf` to `String` to support both names and paths.

### MCP server: unchanged for now

The MCP server continues to operate on a single repo (passed via `--repo` or inferred from CWD). The existing `repo` parameter on MCP tools remains reserved for future use. Multi-repo MCP support can be added later by having the MCP server read the registry the same way the web server does.

---

## 4. Daemon Protocol Extensions

The existing daemon protocol returns pre-formatted text (`DaemonResponse::Line { content: String }`), which works for the CLI but not for the web server. The web server needs structured data (JSON-serializable `FileMatch` objects) to render HTML templates and serve the JSON API.

**Approach:** add new request/response variants alongside the existing CLI-oriented ones. The existing protocol is untouched -- no breaking changes.

### New request variants

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRequest {
    // ... existing variants unchanged ...

    /// Structured search returning JSON FileMatch objects.
    JsonSearch {
        query: String,
        /// 1-indexed page number.
        page: usize,
        /// Results per page (max 100).
        per_page: usize,
        /// Lines of context above/below each match.
        context_lines: usize,
        /// Optional language filter.
        language: Option<String>,
        /// Optional path glob filter.
        path_glob: Option<String>,
    },

    /// Retrieve file contents with metadata.
    GetFile {
        /// Relative path from repo root.
        path: String,
        /// First line to return (1-indexed, default 1).
        line_start: Option<usize>,
        /// Last line to return (default: EOF).
        line_end: Option<usize>,
    },

    /// Structured index status.
    Status,

    /// Health check with metadata.
    Health,
}
```

### New response variant

A single new TLV tag for JSON payloads:

```rust
pub enum DaemonResponse {
    // ... existing variants unchanged ...

    /// JSON-serialized structured data.
    Json { payload: String },  // TLV tag: 0x06
}
```

### Response format for each new request

**`JsonSearch`** returns:

```
[Json]  {"type":"result", "file": <FileMatch>}     // one per matched file
[Json]  {"type":"result", "file": <FileMatch>}
[Json]  {"type":"stats", "stats": <SearchStats>}    // aggregate stats
[Done]  {total, duration_ms, stale}                 // end marker
```

Where `FileMatch` and `SearchStats` are the existing serde-serializable types from `ferret-indexer-core::search`, extended with pagination info:

```rust
/// Wrapper for JSON search response frames.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum JsonSearchFrame {
    #[serde(rename = "result")]
    Result { file: FileMatch },
    #[serde(rename = "stats")]
    Stats { stats: SearchStats },
}

#[derive(Serialize, Deserialize)]
struct SearchStats {
    total_matches: usize,
    files_matched: usize,
    duration_ms: u64,
    page: usize,
    per_page: usize,
    total_pages: usize,
    has_next: bool,
}
```

**`GetFile`** returns:

```
[Json]  {"path":"src/main.rs", "language":"Rust", "total_lines":45, "lines":[...]}
[Done]  ...
```

**`Status`** returns:

```
[Json]  {"status":"ready", "files_indexed":156, "segments":3, "languages":{...}}
[Done]  ...
```

**`Health`** returns:

```
[Json]  {"status":"ok", "version":"0.1.0", "uptime_seconds":3600}
[Done]  ...
```

### Daemon handler changes

The daemon's `handle_connection` match arm gains new cases for each new request type. The handler logic is similar to the existing `Search` handler but skips the formatting step -- it serializes `FileMatch` directly to JSON instead of rendering vimgrep lines:

```rust
DaemonRequest::JsonSearch { query, page, per_page, context_lines, language, path_glob } => {
    let snapshot = manager.snapshot();
    let search_opts = SearchOptions { context_lines, max_results: Some(page * per_page) };

    // Run search (same as existing Search handler)
    let result = search_segments_with_query_and_options(&snapshot, &parsed_query, &search_opts)?;

    // Paginate
    let offset = (page - 1) * per_page;
    let page_results = result.paginate(offset, per_page);

    // Send structured results
    for file_match in &page_results.files {
        let frame = JsonSearchFrame::Result { file: file_match.clone() };
        let json = serde_json::to_string(&frame)?;
        write_response(&mut writer, &DaemonResponse::Json { payload: json }).await?;
    }

    // Send stats
    let stats = SearchStats { total_matches: result.total_match_count, ... };
    let frame = JsonSearchFrame::Stats { stats };
    let json = serde_json::to_string(&frame)?;
    write_response(&mut writer, &DaemonResponse::Json { payload: json }).await?;

    // Send done
    write_response(&mut writer, &DaemonResponse::Done { total, duration_ms, stale }).await?;
}
```

---

## 5. Web Server Integration

The web server (`ferret web`) uses the registry and extended daemon protocol:

```rust
struct AppState {
    registry: RepoRegistry,
}
```

### Request flow

```
GET /api/v1/repos/ferret/search?q=handleRequest&page=1&per_page=25

1. Extract repo name "ferret" from path
2. Look up root path in registry → /Users/howard/src/ferret
3. Connect to daemon: ensure_daemon("/Users/howard/src/ferret")
4. Send: JsonSearch { query: "handleRequest", page: 1, per_page: 25, ... }
5. Receive: Json frames with FileMatch data + Done
6. Serialize as JSON HTTP response (or render as HTML fragment for htmx)
```

### Repo switcher

The UI header includes a dropdown populated from `GET /api/v1/repos`. Selecting a repo reloads the page (or updates via htmx) with the new repo context. All search/file URLs include the repo name, so the current repo is always in the URL.

---

## 6. What Doesn't Change

| Component | Change? | Notes |
|-----------|---------|-------|
| Binary format (meta.bin, trigrams.bin, etc.) | No | Segments are repo-local |
| SegmentManager | No | One per repo, owned by daemon |
| Trigram extraction, posting lists, codec | No | Core indexing unchanged |
| Content verification, ranking | No | Core search unchanged |
| Segment writer/reader | No | Per-repo as today |
| Walker, binary detection | No | Per-repo as today |
| Git change detection | No | Per-repo as today |
| HybridDetector | No | Per-repo as today |
| Recovery | No | Per-repo as today |
| Existing daemon wire protocol | No | New variants added alongside |
| CLI search/files/symbols commands | No | Still infer repo from CWD |

---

## 7. Implementation Sequencing

### Issue 1: Repo registry config + CLI commands

- Config file format, parsing, writing (`~/.config/ferret/repos.toml`)
- `ferret repos list|add|remove` subcommands
- Auto-registration in `ferret init`
- ~300 lines + tests

### Issue 2: Daemon protocol extensions

- New `DaemonRequest` variants: `JsonSearch`, `GetFile`, `Status`, `Health`
- New `DaemonResponse::Json` variant + TLV tag `0x06`
- Daemon handler code for each new request type
- Wire format encode/decode for `Json` frames
- ~400 lines + tests

### Issue 3: Web server skeleton with repo registry

- axum server startup, `AppState` with `RepoRegistry`
- `ensure_daemon()` per registered repo
- Repo list endpoint (`GET /api/v1/repos`)
- Health endpoint (`GET /api/v1/health`)
- Static file serving via `rust-embed`
- ~300 lines + tests

### Issue 4: Web server search + file endpoints

- `GET /api/v1/repos/{name}/search` (JSON, proxied via `JsonSearch`)
- `GET /api/v1/repos/{name}/search/stream` (SSE)
- `GET /api/v1/repos/{name}/files/{*path}` (proxied via `GetFile`)
- `GET /api/v1/repos/{name}/status` (proxied via `Status`)
- `POST /api/v1/repos/{name}/refresh` (proxied via `Reindex`)
- ~400 lines + tests

### Issue 5: htmx frontend with repo switcher

- HTML templates (askama), CSS, JS, htmx
- Search view with debounced search, filter chips, paginated results
- File preview view with match highlighting
- Repo switcher dropdown in header
- Keyboard shortcuts
- ~600 lines (templates + static assets)

Issues 1 and 2 can be done in parallel. Issue 3 depends on both. Issues 4 and 5 depend on 3.

```
[1: Registry config]  ──┐
                        ├──→ [3: Web skeleton] → [4: API endpoints] → [5: Frontend]
[2: Protocol extensions]┘
```
