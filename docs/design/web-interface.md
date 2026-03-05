# Web Interface Design

This document covers the REST API and web UI for ferret. The web interface is designed for local use by a single developer -- it should feel fast, lightweight, and keyboard-driven.

IMPORTANT: make sure every dependency added here is up to date!

## Technology Choices

### Backend: axum

axum is the right choice for a local dev tool:

- **Lightweight** -- no macros, no ORM, just functions. Compiles fast relative to alternatives like actix-web.
- **Tower middleware ecosystem** -- compression, CORS, tracing come free via tower-http.
- **First-class SSE support** -- `axum::response::Sse` for streaming search results.
- **Shared state** -- `State` extractor makes it trivial to share the repo registry across handlers.
- **tokio-native** -- same runtime we use for file watching and MCP server.

We don't need actix-web's actor system or its performance edge (irrelevant for a local tool with one user).

### Frontend: vanilla HTML + htmx + a small amount of JS

No build step. No node_modules. The entire frontend ships as static files embedded in the binary via `rust-embed` or `include_dir`.

- **htmx (~16kb gzipped)** -- handles search-as-you-type via `hx-get` with `hx-trigger="keyup changed delay:150ms"`, partial page updates, SSE for index status.
- **Vanilla JS** -- only for syntax highlighting (highlight.js or a lighter alternative) and keyboard shortcuts.
- **CSS** -- a single stylesheet, no framework. Use system fonts, minimal color palette with light/dark mode via `prefers-color-scheme`.

This keeps the binary self-contained. No external CDN dependencies -- everything works offline.

### Templates: askama

Server-side HTML fragment rendering uses askama (Jinja2-like templates). Template files live alongside the static assets, separate from Rust code. This makes it easy to iterate on the UI without recompiling handler logic. askama templates are compile-time checked, so broken templates fail the build.

### Why not GraphQL?

GraphQL adds complexity (schema definitions, resolver boilerplate, client library) for no benefit here. We have a small number of well-defined endpoints. REST with JSON is simpler and sufficient. The search endpoint is the only "complex" query, and it maps naturally to query parameters.

---

## Architecture

### Proxy model: web server talks to per-repo daemons

The web server does **not** own `SegmentManager` instances. Instead, it proxies all search, file, and index operations to the existing per-repo daemons over Unix sockets. This avoids divergence between the daemon and web server (two `SegmentManager` instances fighting over the same `.ferret_index/` directory) and gets mutual exclusion for free via the daemon's internal writer mutex.

```
Browser → HTTP → Web Server → Unix Socket → Daemon → SegmentManager
                  (axum)        (per repo)    (owns index)

CLI ─────────────────────────→ Unix Socket → Daemon → SegmentManager
                                (unchanged)
```

On startup, the web server reads the repo registry config (`~/.config/ferret/repos.toml`, see `docs/design/multi-repo.md`) and calls `ensure_daemon()` for each registered repo. The web server holds a map of `repo_name → repo_root` and opens a new Unix socket connection per request.

The CLI is unaffected -- it continues to auto-start its own per-repo daemon via `ensure_daemon()` as it does today.

### Daemon protocol extensions

The existing daemon protocol returns pre-formatted text lines (`DaemonResponse::Line`), which works for the CLI but not for the web server. New structured request/response variants are added alongside the existing ones (see `docs/design/multi-repo.md` § Daemon Protocol Extensions for the full spec):

- `JsonSearch` → returns `Json` frames with serialized `FileMatch` objects
- `GetFile` → returns file content with metadata
- `Status` → returns structured index status (file count, languages, segments)
- `Health` → returns version, uptime

The existing CLI-oriented `Search`/`QuerySearch`/`Files` variants are unchanged.

---

## API Design

All endpoints are prefixed with `/api/v1`. The API returns JSON with `Content-Type: application/json`. Errors use standard HTTP status codes with a consistent error body.

### Error Format

```json
{
  "error": {
    "code": "invalid_query",
    "message": "Unclosed regex group at position 12"
  }
}
```

### Search

#### `GET /api/v1/repos/{name}/search`

The primary endpoint. Supports the same query syntax as the CLI and MCP interfaces.

**Query Parameters:**

| Parameter   | Type    | Default | Description |
|-------------|---------|---------|-------------|
| `q`         | string  | required | Search query (see Query Syntax below) |
| `page`      | integer | 1       | Page number (1-indexed) |
| `per_page`  | integer | 25      | Results per page (max 100) |
| `context`   | integer | 2       | Lines of context above/below match |
| `stats_only`| boolean | false   | Return only match count, no results |

The `repo` is determined by the path prefix: `/api/v1/repos/{name}/search`. There is no cross-repo search -- each search targets exactly one repo.

**Query Syntax:**

The `q` parameter supports inline filters, following GitHub code search conventions:

| Filter            | Example                   | Description |
|-------------------|---------------------------|-------------|
| (bare text)       | `handleRequest`           | Literal substring match |
| `regex:`          | `regex:fn\s+\w+`         | RE2 regex pattern |
| `/pattern/`       | `/fn\s+\w+/`             | Shorthand for regex |
| `language:`       | `language:rust`           | Filter by language |
| `lang:`           | `lang:rs`                 | Alias (also accepts extensions) |
| `path:`           | `path:src/server/`        | Path prefix filter |
| `file:`           | `file:*.test.ts`          | Glob pattern on filename |
| `symbol:`         | `symbol:handleRequest`    | Symbol definitions only |
| `case:yes`        | `case:yes TODO`           | Case-sensitive search |
| `NOT`             | `NOT test`                | Exclude matches |
| `OR`              | `lang:rust OR lang:go`    | Union |

Multiple filters are ANDed. Bare text without a filter prefix is treated as a literal substring match (case-insensitive by default).

**Response: `200 OK`**

```json
{
  "query": {
    "raw": "language:rust fn main",
    "parsed": {
      "pattern": "fn main",
      "filters": {
        "language": ["rust"]
      },
      "is_regex": false,
      "case_sensitive": false
    }
  },
  "stats": {
    "total_matches": 142,
    "files_searched": 8923,
    "files_matched": 87,
    "duration_ms": 12,
    "index_revision": "a1b2c3d4"
  },
  "results": [
    {
      "repo": "ferret",
      "path": "src/main.rs",
      "language": "Rust",
      "matches": [
        {
          "line_number": 15,
          "line_content": "fn main() {",
          "match_offsets": [[0, 7]],
          "context_before": [
            {"line_number": 13, "content": "use std::env;"},
            {"line_number": 14, "content": ""}
          ],
          "context_after": [
            {"line_number": 16, "content": "    let args = Args::parse();"},
            {"line_number": 17, "content": "    run(args).await;"}
          ]
        }
      ]
    }
  ],
  "pagination": {
    "page": 1,
    "per_page": 25,
    "total_pages": 6,
    "has_next": true
  }
}
```

Results are grouped by file. Within each file, matches are sorted by line number. Files are ranked by number of matches (most matches first), with ties broken by path (alphabetical).

**Error Responses:**

| Status | Condition |
|--------|-----------|
| `400`  | Invalid query syntax, invalid parameter values |
| `503`  | Index not ready (still building) |

#### `GET /api/v1/repos/{name}/search/stream`

Same parameters as the search endpoint, but returns results as Server-Sent Events. This is used by the UI for live-as-you-type search, so the user sees results appearing as the backend finds them rather than waiting for the full result set.

Each SSE event:

```
event: result
data: {"repo":"ferret","path":"src/main.rs","matches":[...]}

event: result
data: {"repo":"ferret","path":"src/server.rs","matches":[...]}

event: stats
data: {"total_matches":142,"files_searched":8923,"duration_ms":12}

event: done
data: {}
```

The stream emits `result` events as files are matched, then a final `stats` event with totals, followed by `done`. The client can close the connection early if the user changes their query (debounce handles this).

### File Retrieval

#### `GET /api/v1/repos/{name}/files/{path...}`

Retrieve a single file's contents with syntax highlighting metadata.

**Query Parameters:**

| Parameter    | Type    | Default | Description |
|--------------|---------|---------|-------------|
| `highlight`  | boolean | true    | Include syntax highlighting tokens |
| `line_start` | integer | 1       | First line to return (for large files) |
| `line_end`   | integer | EOF     | Last line to return |

**Response: `200 OK`**

```json
{
  "repo": "ferret",
  "path": "src/main.rs",
  "language": "Rust",
  "size_bytes": 1234,
  "total_lines": 45,
  "lines": [
    {
      "number": 1,
      "content": "use axum::Router;",
      "tokens": [
        {"start": 0, "end": 3, "kind": "keyword"},
        {"start": 4, "end": 17, "kind": "namespace"}
      ]
    }
  ]
}
```

The `tokens` array provides character offsets for syntax highlighting. The UI renders these as `<span>` elements with CSS classes. Server-side tokenization means the UI doesn't need a heavy JS syntax highlighter for the file preview.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| `404`  | File or repo not found |
| `416`  | Requested line range out of bounds |

### Index Management

#### `GET /api/v1/repos/{name}/status`

Returns the current state of the index for a specific repo.

**Response: `200 OK`**

```json
{
  "status": "ready",
  "repos": [
    {
      "name": "ferret",
      "path": "/Users/howard/src/ferret",
      "files_indexed": 156,
      "last_indexed_at": "2026-02-27T10:30:00Z",
      "index_duration_ms": 450
    }
  ],
  "total_files": 156,
  "total_size_bytes": 2345678,
  "languages": {
    "Rust": 89,
    "TOML": 4,
    "Markdown": 12
  }
}
```

The `status` field is one of: `ready`, `indexing`, `error`.

#### `GET /api/v1/repos/{name}/status/stream`

SSE stream for live index status updates for a specific repo. The UI uses this to show a progress indicator during reindexing.

```
event: status
data: {"status":"indexing","progress":{"files_done":80,"files_total":156,"current_file":"src/search.rs"}}

event: status
data: {"status":"ready","repos":[...]}
```

#### `POST /api/v1/repos/{name}/refresh`

Trigger a manual reindex for a specific repo. Normally the file watcher handles this, but this endpoint exists for when you want to force it.

**Request Body:** none required.

**Response: `202 Accepted`**

```json
{
  "message": "Reindex started",
  "repo": "ferret"
}
```

#### `GET /api/v1/repos`

List registered repositories (from `~/.config/ferret/repos.toml`). Includes live status from each repo's daemon.

**Response: `200 OK`**

```json
{
  "repos": [
    {
      "name": "ferret",
      "path": "/Users/howard/src/ferret",
      "status": "ready",
      "files_indexed": 156
    },
    {
      "name": "frontend",
      "path": "/Users/howard/src/frontend",
      "status": "indexing",
      "files_indexed": 0
    }
  ]
}
```

The `status` field is derived by sending a `Status` request to the repo's daemon. If the daemon is unreachable, status is `"offline"`.

#### `POST /api/v1/repos`

Register a repository. Writes to the config file and starts a daemon for it.

**Request Body:**

```json
{
  "path": "/Users/howard/src/other-project",
  "name": "other-project"
}
```

If `name` is omitted, the directory name is used. The repo must already be initialized (`ferret init`).

**Response: `201 Created`**

Returns the repo object.

#### `DELETE /api/v1/repos/{name}`

Unregister a repository. Removes from the config file. Does not delete index data.

**Response: `204 No Content`**

### Health

#### `GET /api/v1/health`

Simple liveness check.

**Response: `200 OK`**

```json
{
  "status": "ok",
  "version": "0.1.0",
  "uptime_seconds": 3600
}
```

---

## UI Design

### Design Principles

1. **Speed over polish** -- perceived latency matters most. Results should appear within the first keystroke pause.
2. **Keyboard-first** -- every action reachable without a mouse. The UI should feel like a terminal tool that happens to run in a browser.
3. **Information density** -- show more results, less chrome. Developers want to scan code, not admire whitespace.
4. **Zero configuration** -- works immediately at `http://localhost:PORT` with sensible defaults.

### Layout

The UI has two views: Search (default) and File Preview.

#### Search View

```
+----------------------------------------------------------------------+
| ferret                           [v ferret ▾] [ready] [2 repos]    |
+----------------------------------------------------------------------+
| [/ ] Search: language:rust fn handle_______________|                 |
|      [Rust] [x]  [path:src/] [x]                                    |
+----------------------------------------------------------------------+
| 142 matches in 87 files (12ms)                          page 1 of 6 |
+----------------------------------------------------------------------+
|                                                                      |
| src/server/handler.rs                                    Rust   23   |
| -------------------------------------------------------------------- |
|  41 |  use super::middleware;                                        |
|  42 |                                                                |
|  43 |  pub async fn [handle_request](req: Request) -> Response {     |
|  44 |      let query = req.query();                                  |
|  45 |      middleware::log(&req);                                    |
|  ..                                                                  |
| 102 |  fn [handle_error](err: Error) -> Response {                   |
| 103 |      eprintln!("error: {err}");                                |
| 104 |      Response::internal_server_error()                         |
|                                                                      |
| src/server/router.rs                                     Rust    5   |
| -------------------------------------------------------------------- |
|  18 |  use crate::server::handler::[handle_request];                 |
|  19 |  use crate::server::handler::[handle_error];                   |
|                                                                      |
| src/main.rs                                              Rust    1   |
| -------------------------------------------------------------------- |
|  30 |  let app = Router::new().route("/", get([handle_request]));    |
|  31 |      .route("/error", get(handle_error));                      |
|                                                                      |
|                                                                      |
| [1] 2 3 4 5 6  >                                                    |
+----------------------------------------------------------------------+
```

Key elements:

- **Header bar** -- app name, **repo switcher dropdown** (switches all search/file operations to the selected repo), index status badge (green dot = ready, yellow = indexing, red = error), repo count.
- **Search bar** -- full width, always focused on page load. Accepts the full query syntax. `/` to focus from anywhere.
- **Active filters** -- parsed out of the query and shown as removable chips below the search bar. Clicking a chip removes it from the query. Clicking a language name in results adds it as a filter.
- **Stats line** -- match count, file count, search duration. Updates live during SSE streaming.
- **Result list** -- grouped by file. Each file header shows the path (clickable to open file preview), language, and match count. Match lines show line numbers and highlighted matches in `[brackets]` (rendered as `<mark>` elements). Context lines are dimmed.
- **Pagination** -- simple numbered pages. Keyboard: `n` next page, `p` previous page.

#### File Preview View

Navigated to by clicking a file path in search results, or via `/api/v1/files/...`.

```
+----------------------------------------------------------------------+
| ferret                                          [ready] [2 repos]   |
+----------------------------------------------------------------------+
| < Back to results    src/server/handler.rs               Rust  106L  |
+----------------------------------------------------------------------+
|                                                                      |
|   1 | use axum::{Request, Response};                                 |
|   2 | use super::middleware;                                         |
|   3 |                                                                |
|  .. | (collapsed: lines 4-40)                                       |
|     |                                                                |
|  41 | use super::middleware;                                         |
|  42 |                                                                |
|  43 | pub async fn [handle_request](req: Request) -> Response {      |
|  44 |     let query = req.query();                                   |
|  45 |     middleware::log(&req);                                     |
|  46 |     // process the request                                     |
|  47 |     let result = query.execute().await;                        |
|  48 |     Response::ok(result)                                       |
|  49 | }                                                              |
|  50 |                                                                |
|  .. | (collapsed: lines 51-100)                                     |
|     |                                                                |
| 101 |                                                                |
| 102 | fn [handle_error](err: Error) -> Response {                    |
| 103 |     eprintln!("error: {err}");                                 |
| 104 |     Response::internal_server_error()                          |
| 105 | }                                                              |
| 106 |                                                                |
|                                                                      |
+----------------------------------------------------------------------+
```

Key elements:

- **Breadcrumb** -- back link to search results, file path, language, line count.
- **Syntax highlighting** -- server-rendered tokens, styled with CSS.
- **Match highlighting** -- search matches are highlighted within the file.
- **Collapsible regions** -- non-matching regions are collapsed by default. Click to expand. This focuses attention on the matches while keeping them in the context of the full file.
- **Line numbers** -- clickable. Clicking a line number copies a deep link (`?file=path&line=43`) to clipboard.

### Keyboard Shortcuts

All shortcuts work without modifier keys (outside of text inputs).

| Key       | Action |
|-----------|--------|
| `/`       | Focus search bar |
| `Escape`  | Clear search / close preview / blur input |
| `j` / `k` | Next / previous result file |
| `Enter`   | Open selected file preview |
| `Backspace` or `q` | Back to search results (from file preview) |
| `n` / `p` | Next / previous page |
| `?`       | Show keyboard shortcut help overlay |

Inside the search bar:

| Key       | Action |
|-----------|--------|
| `Enter`   | Submit search (also happens automatically with debounce) |
| `Escape`  | Blur search bar |
| `Ctrl+L`  | Clear search bar |

### Dark/Light Mode

The UI respects `prefers-color-scheme` from the OS. No manual toggle -- this is a local tool, it should just match the system. CSS custom properties make this a few lines:

```css
:root {
  --bg: #ffffff;
  --fg: #1a1a1a;
  --match-bg: #fff3cd;
  --border: #e0e0e0;
  --dim: #6b7280;
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d1117;
    --fg: #e6edf3;
    --match-bg: #3b2e00;
    --border: #30363d;
    --dim: #8b949e;
  }
}
```

Color values drawn from GitHub's code view palette for familiarity.

---

## Performance

### Target: <100ms for most searches

The web layer itself should add minimal overhead on top of the index query time. Key strategies:

**Search path:**
1. Query arrives at axum handler (~0ms overhead).
2. Parse query string into structured query (~<1ms).
3. Execute against trigram index (~5-50ms for most queries, depending on selectivity).
4. Collect results, apply pagination (~<1ms).
5. Serialize to JSON (~<1ms).

Total API latency budget: <100ms for the 95th percentile.

**Streaming for long searches:**
For queries that match many files (e.g., common words, broad regex), the SSE endpoint streams results as they are found. The UI renders the first results while the search continues. This makes even slow searches feel fast -- the user sees results in <50ms even if the full search takes 500ms.

**Debounce:**
The UI debounces search input with a 150ms delay. This means:
- Typing "fn main" triggers one search for "fn main", not searches for "f", "fn", "fn ", "fn m", etc.
- If the user is still typing when results arrive, the results are discarded (superseded by the pending debounced request).
- The SSE connection from the previous search is closed before starting a new one, so at most one search is in flight.

**htmx implementation:**
```html
<input type="search"
       name="q"
       hx-get="/search-results"
       hx-trigger="keyup changed delay:150ms, search"
       hx-target="#results"
       hx-indicator="#search-spinner"
       hx-swap="innerHTML" />
```

The `/search-results` endpoint returns an HTML fragment (not JSON) containing the rendered result list. This avoids client-side rendering entirely. The same search logic backs both the JSON API and the HTML fragment endpoint.

**Response size control:**
- Default 25 results per page keeps response payloads small (~10-50kb).
- Context lines limited to 2 above/below by default (configurable via `context` param).
- File preview loads only the relevant sections by default, with lazy expansion.

**Caching:**
No HTTP caching of search results (they can go stale when files change). But we do cache:
- Syntax highlighting tokens for recently viewed files (LRU, ~100 files).
- Parsed query structures (avoid re-parsing on pagination).

**Static asset caching:**
Static files (CSS, JS, htmx library) are served with `Cache-Control: public, max-age=31536000, immutable` and content-hashed filenames so they're cached forever and busted on update.

---

## Implementation Notes

### Dual rendering: HTML fragments and JSON API

The web server has two response modes for search:

1. **JSON API** (`/api/v1/repos/{name}/search`) -- used by external consumers.
2. **HTML fragments** (`/search-results`) -- used by htmx for the web UI. These are internal endpoints, not part of the public API.

Both send a `JsonSearch` request to the per-repo daemon over Unix socket and receive structured `FileMatch` data. The JSON API serializes it directly; the HTML path renders it through askama templates. Search logic lives entirely in the daemon -- the web server is a stateless proxy.

### Embedded static files

All frontend assets are embedded in the binary at compile time:

```rust
#[derive(RustEmbed)]
#[folder = "static/"]
struct Assets;

// Serve with axum
async fn static_handler(Path(path): Path<String>) -> impl IntoResponse {
    Assets::get(&path)
        .map(|file| {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.to_string())], file.data)
        })
        .ok_or(StatusCode::NOT_FOUND)
}
```

This keeps deployment to a single binary. No separate static file directory to manage.

### Server startup

The web server binds to `127.0.0.1` only (not `0.0.0.0`) since this is a local tool. Default port is `4040` (configurable). On startup it:

1. Reads the repo registry from `~/.config/ferret/repos.toml`
2. Calls `ensure_daemon()` for each registered repo (auto-starts daemons as needed)
3. Prints startup info:

```
ferret web interface: http://localhost:4040
  repos: ferret, frontend (2 repos)
```

### axum router sketch

```rust
use axum::{Router, routing::{get, post, delete}};

fn app(state: AppState) -> Router {
    Router::new()
        // Web UI
        .route("/", get(ui::index))
        .route("/search-results", get(ui::search_results_fragment))
        .route("/file/{repo}/{*path}", get(ui::file_preview))
        .route("/static/{*path}", get(static_handler))

        // JSON API
        .nest("/api/v1", api_router())

        .with_state(state)
}

fn api_router() -> Router<AppState> {
    Router::new()
        // Repo-scoped endpoints (all search/file/index ops target one repo)
        .route("/repos/{name}/search", get(api::search))
        .route("/repos/{name}/search/stream", get(api::search_stream))
        .route("/repos/{name}/files/{*path}", get(api::get_file))
        .route("/repos/{name}/status", get(api::index_status))
        .route("/repos/{name}/status/stream", get(api::index_status_stream))
        .route("/repos/{name}/refresh", post(api::refresh_index))

        // Repo management
        .route("/repos", get(api::list_repos))
        .route("/repos", post(api::add_repo))
        .route("/repos/{name}", delete(api::remove_repo))

        // Global
        .route("/health", get(api::health))
}
```

### Frontend file structure

```
static/
  index.html          -- main page, search view
  style.css           -- all styles (~200 lines)
  app.js              -- keyboard shortcuts, minor interactions (~100 lines)
  htmx.min.js         -- htmx library (vendored, ~16kb gzipped)
  highlight-worker.js -- optional: web worker for client-side highlighting fallback
```

Total frontend: ~4 files, <50kb uncompressed.
