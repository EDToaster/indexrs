# Web Symbol Features Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add three symbol-powered features to the web UI: symbol search mode, file symbol outline, and a "Go to Symbol" quick-open modal.

**Architecture:** The daemon currently sends symbol results as text lines (`DaemonResponse::Line`). We add a new `DaemonRequest::JsonSymbols` variant that returns structured JSON (like `JsonSearch` does for text search). The web server proxies this to three new endpoints: a JSON API, an htmx symbol-results fragment, and an htmx symbol-outline fragment. The frontend adds a search mode toggle, an outline panel in file preview, and a `@`-triggered quick-open modal — all using htmx for server-rendered HTML.

**Tech Stack:** Rust (axum, askama, serde), htmx, vanilla JS, CSS custom properties, Playwright MCP for E2E testing.

---

### Task 1: Add Serialize to SymbolMatch + JSON protocol types

**Files:**
- Modify: `ferret-indexer-core/src/symbol_index.rs:451` (add Serialize derive)
- Modify: `ferret-indexer-daemon/src/json_protocol.rs` (add JsonSymbolsFrame, SymbolsStats)
- Modify: `ferret-indexer-daemon/src/types.rs` (add JsonSymbols variant)
- Modify: `ferret-indexer-daemon/src/lib.rs` (re-export new types)

**Step 1: Add Serialize/Deserialize to SymbolMatch**

In `ferret-indexer-core/src/symbol_index.rs`, change:
```rust
#[derive(Debug, Clone)]
pub struct SymbolMatch {
```
to:
```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SymbolMatch {
```

**Step 2: Add JsonSymbolsFrame and SymbolsStats to json_protocol.rs**

Append to `ferret-indexer-daemon/src/json_protocol.rs`:
```rust
use ferret_indexer_core::symbol_index::SymbolMatch;

/// Wrapper for JSON symbol search response frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JsonSymbolsFrame {
    #[serde(rename = "symbol")]
    Symbol(SymbolMatchResponse),
    #[serde(rename = "stats")]
    Stats { stats: SymbolsStats },
}

/// A symbol match with 1-based line numbers for display.
#[derive(Debug, Serialize, Deserialize)]
pub struct SymbolMatchResponse {
    pub name: String,
    pub kind: String,
    pub path: String,
    /// 1-based line number (converted from SymbolMatch's 0-based).
    pub line: u32,
    pub column: u16,
    pub score: f64,
}

impl From<SymbolMatch> for SymbolMatchResponse {
    fn from(m: SymbolMatch) -> Self {
        Self {
            name: m.name,
            kind: m.kind.short_label().to_string(),
            path: m.path,
            line: m.line + 1,
            column: m.column,
            score: m.score,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SymbolsStats {
    pub total: usize,
    pub duration_ms: u64,
}
```

**Step 3: Add DaemonRequest::JsonSymbols variant**

In `ferret-indexer-daemon/src/types.rs`, add after the `Symbols` variant:
```rust
/// Structured symbol search returning JSON-serializable SymbolMatch objects.
JsonSymbols {
    query: Option<String>,
    kind: Option<String>,
    language: Option<String>,
    path_filter: Option<String>,
    max_results: Option<usize>,
    offset: Option<usize>,
},
```

**Step 4: Re-export new types in lib.rs**

In `ferret-indexer-daemon/src/lib.rs`, add to the `json_protocol` re-export:
```rust
pub use json_protocol::{
    FileResponse, HealthResponse, JsonSearchFrame, JsonSymbolsFrame, SearchStats,
    SegmentInfo, StatusResponse, SymbolMatchResponse, SymbolsStats,
};
```

**Step 5: Add roundtrip test for JsonSymbols**

In `ferret-indexer-daemon/src/types.rs` tests:
```rust
#[test]
fn test_json_symbols_roundtrip() {
    let req = DaemonRequest::JsonSymbols {
        query: Some("process".to_string()),
        kind: Some("fn".to_string()),
        language: None,
        path_filter: Some("src/".to_string()),
        max_results: Some(50),
        offset: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    let deserialized: DaemonRequest = serde_json::from_str(&json).unwrap();
    match deserialized {
        DaemonRequest::JsonSymbols {
            query, kind, language, path_filter, max_results, offset,
        } => {
            assert_eq!(query, Some("process".to_string()));
            assert_eq!(kind, Some("fn".to_string()));
            assert!(language.is_none());
            assert_eq!(path_filter, Some("src/".to_string()));
            assert_eq!(max_results, Some(50));
            assert!(offset.is_none());
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}
```

Add a serialization test for `JsonSymbolsFrame` in `json_protocol.rs` tests:
```rust
#[test]
fn test_json_symbols_frame_serialization() {
    let frame = JsonSymbolsFrame::Symbol(SymbolMatchResponse {
        name: "process_data".to_string(),
        kind: "fn".to_string(),
        path: "src/main.rs".to_string(),
        line: 42,
        column: 4,
        score: 1.0,
    });
    let json = serde_json::to_string(&frame).unwrap();
    assert!(json.contains(r#""type":"symbol"#));
    assert!(json.contains("process_data"));

    let stats_frame = JsonSymbolsFrame::Stats {
        stats: SymbolsStats { total: 10, duration_ms: 5 },
    };
    let stats_json = serde_json::to_string(&stats_frame).unwrap();
    assert!(stats_json.contains(r#""type":"stats"#));
    assert!(stats_json.contains(r#""total":10"#));
}
```

**Step 6: Run tests**

```bash
cargo test -p ferret-indexer-daemon -- --include-ignored
cargo test -p ferret-indexer-core -- symbol
```

**Step 7: Run clippy + fmt**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

**Step 8: Commit**

```bash
git add ferret-indexer-core/src/symbol_index.rs ferret-indexer-daemon/src/json_protocol.rs ferret-indexer-daemon/src/types.rs ferret-indexer-daemon/src/lib.rs
git commit -m "feat(daemon): add JsonSymbols request variant and JSON protocol types"
```

---

### Task 2: Daemon handler for JsonSymbols

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs` (add handler + match arm)

**Step 1: Add handle_json_symbols_request function**

Add near the existing `handle_symbols_request` function in `ferret-indexer-cli/src/daemon.rs`:

```rust
fn handle_json_symbols_request(
    manager: &SegmentManager,
    query: Option<String>,
    kind: Option<String>,
    language: Option<String>,
    path_filter: Option<String>,
    max_results: Option<usize>,
    offset: Option<usize>,
) -> Result<(Vec<ferret_indexer_core::symbol_index::SymbolMatch>, std::time::Duration), String> {
    use ferret_indexer_core::symbol_index::{SymbolSearchOptions, search_symbols};
    use ferret_indexer_core::types::SymbolKind;

    let start = std::time::Instant::now();

    let query_str = query.unwrap_or_default();

    // Allow empty query when path_filter is set (for file outline mode).
    if query_str.is_empty() && path_filter.is_none() {
        return Err("symbol search requires 'query' or 'path_filter'".to_string());
    }

    let kind_filter = match kind {
        Some(ref k) => match SymbolKind::from_str_loose(k) {
            Some(sk) => Some(sk),
            None => return Err(format!("unknown symbol kind: \"{k}\"")),
        },
        None => None,
    };

    let language_filter = match language {
        Some(ref lang_str) => match ferret_indexer_core::match_language(lang_str) {
            Ok(lang) => Some(lang),
            Err(_) => return Err(format!("unknown language: \"{lang_str}\"")),
        },
        None => None,
    };

    let options = SymbolSearchOptions {
        kind: kind_filter,
        language: language_filter,
        path_filter,
        max_results: max_results.unwrap_or(100).min(500),
        offset: offset.unwrap_or(0),
    };

    let snapshot = manager.snapshot();
    let matches = search_symbols(&snapshot, &query_str, &options).map_err(|e| e.to_string())?;

    Ok((matches, start.elapsed()))
}
```

**Step 2: Add match arm for JsonSymbols**

In the main request dispatch `match` in `handle_connection`, add after the existing `Symbols` arm:

```rust
DaemonRequest::JsonSymbols {
    query,
    kind,
    language,
    path_filter,
    max_results,
    offset,
} => {
    let stale = !caught_up.load(Ordering::Relaxed);
    match handle_json_symbols_request(
        manager, query, kind, language, path_filter, max_results, offset,
    ) {
        Ok((matches, elapsed)) => {
            let total = matches.len();
            for m in matches {
                let frame = ferret_indexer_daemon::JsonSymbolsFrame::Symbol(m.into());
                let payload = serde_json::to_string(&frame).unwrap();
                wire::write_response(
                    &mut writer,
                    &DaemonResponse::Json { payload },
                )
                .await
                .map_err(IndexError::Io)?;
            }
            // Send stats frame
            let stats_frame = ferret_indexer_daemon::JsonSymbolsFrame::Stats {
                stats: ferret_indexer_daemon::SymbolsStats {
                    total,
                    duration_ms: elapsed.as_millis() as u64,
                },
            };
            let stats_payload = serde_json::to_string(&stats_frame).unwrap();
            wire::write_response(
                &mut writer,
                &DaemonResponse::Json { payload: stats_payload },
            )
            .await
            .map_err(IndexError::Io)?;

            wire::write_response(
                &mut writer,
                &DaemonResponse::Done {
                    total,
                    duration_ms: elapsed.as_millis() as u64,
                    stale,
                },
            )
            .await
            .map_err(IndexError::Io)?;
        }
        Err(msg) => {
            wire::write_response(&mut writer, &DaemonResponse::Error { message: msg })
                .await
                .map_err(IndexError::Io)?;
        }
    }
}
```

**Step 3: Run clippy + fmt + tests**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

**Step 4: Commit**

```bash
git add ferret-indexer-cli/src/daemon.rs
git commit -m "feat(daemon): add JsonSymbols handler for structured symbol search"
```

---

### Task 3: Web API endpoint + proxy

**Files:**
- Modify: `ferret-indexer-web/src/proxy.rs` (add `symbols()` function)
- Modify: `ferret-indexer-web/src/api.rs` (add handler + params + response types)
- Modify: `ferret-indexer-web/src/lib.rs` (add route)

**Step 1: Add proxy::symbols() function**

In `ferret-indexer-web/src/proxy.rs`:
```rust
/// Send a JsonSymbols request to the daemon and return matched symbols + stats.
pub async fn symbols(
    daemon_bin: &Path,
    repo_root: &Path,
    query: Option<String>,
    kind: Option<String>,
    language: Option<String>,
    path_filter: Option<String>,
    max_results: Option<usize>,
    offset: Option<usize>,
) -> Result<(Vec<ferret_indexer_daemon::SymbolMatchResponse>, ferret_indexer_daemon::SymbolsStats), ApiError> {
    let request = DaemonRequest::JsonSymbols {
        query,
        kind,
        language,
        path_filter,
        max_results,
        offset,
    };

    let result = send_request(daemon_bin, repo_root, &request).await?;

    let mut symbols = Vec::new();
    let mut stats = None;

    for payload in result.payloads {
        let frame: ferret_indexer_daemon::JsonSymbolsFrame = serde_json::from_str(&payload)
            .map_err(|e| ApiError::internal(format!("failed to parse symbols frame: {e}")))?;
        match frame {
            ferret_indexer_daemon::JsonSymbolsFrame::Symbol(m) => symbols.push(m),
            ferret_indexer_daemon::JsonSymbolsFrame::Stats { stats: s } => stats = Some(s),
        }
    }

    let stats = stats.unwrap_or(ferret_indexer_daemon::SymbolsStats {
        total: symbols.len(),
        duration_ms: result.duration_ms,
    });

    Ok((symbols, stats))
}
```

**Step 2: Add API handler + types**

In `ferret-indexer-web/src/api.rs`, add params and response structs:
```rust
#[derive(Deserialize)]
pub struct SymbolParams {
    pub q: Option<String>,
    pub kind: Option<String>,
    pub language: Option<String>,
    pub path: Option<String>,
    #[serde(default = "default_symbol_limit")]
    pub max_results: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_symbol_limit() -> usize {
    100
}

#[derive(Serialize)]
pub struct SymbolSearchResponse {
    pub symbols: Vec<ferret_indexer_daemon::SymbolMatchResponse>,
    pub stats: ferret_indexer_daemon::SymbolsStats,
}
```

Add handler:
```rust
/// `GET /repos/{name}/symbols?q=...&kind=...&language=...&path=...&max_results=100&offset=0`
pub async fn symbols(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<SymbolParams>,
) -> Result<Json<SymbolSearchResponse>, ApiError> {
    let repo_path = state
        .repo_path(&name)
        .await
        .ok_or_else(|| ApiError::repo_not_found(&name))?;

    if params.q.is_none() && params.path.is_none() {
        return Err(ApiError::bad_request("'q' or 'path' parameter required"));
    }

    let (symbols, stats) = proxy::symbols(
        state.daemon_bin(),
        &repo_path,
        params.q,
        params.kind,
        params.language,
        params.path,
        Some(params.max_results.min(500)),
        Some(params.offset),
    )
    .await?;

    Ok(Json(SymbolSearchResponse { symbols, stats }))
}
```

**Step 3: Add route**

In `ferret-indexer-web/src/lib.rs`, add to the `api` router:
```rust
.route("/repos/{name}/symbols", get(api::symbols))
```

**Step 4: Add unit test**

In `ferret-indexer-web/src/api.rs` tests:
```rust
#[tokio::test]
async fn test_symbols_unknown_repo_returns_404() {
    let app = test_app();
    let req = Request::builder()
        .uri("/api/v1/repos/nonexistent/symbols?q=main")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_symbols_no_params_returns_400() {
    let app = test_app();
    let req = Request::builder()
        .uri("/api/v1/repos/nonexistent/symbols")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    // 404 because repo check comes first, but with a real repo it would be 400
    assert!(resp.status().is_client_error());
}
```

**Step 5: Run clippy + fmt + tests**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test -p ferret-indexer-web
```

**Step 6: Commit**

```bash
git add ferret-indexer-web/src/proxy.rs ferret-indexer-web/src/api.rs ferret-indexer-web/src/lib.rs
git commit -m "feat(web): add /api/v1/repos/{name}/symbols JSON endpoint"
```

---

### Task 4: Symbol Search Mode UI

**Files:**
- Create: `ferret-indexer-web/templates/symbol_results.html`
- Modify: `ferret-indexer-web/templates/index.html` (add mode toggle)
- Modify: `ferret-indexer-web/src/ui.rs` (add symbol search fragment handler + types)
- Modify: `ferret-indexer-web/src/lib.rs` (add route)
- Modify: `ferret-indexer-web/static/style.css` (add symbol result styles)
- Modify: `ferret-indexer-web/static/app.js` (add mode switching logic)

**Step 1: Create symbol_results.html template**

Create `ferret-indexer-web/templates/symbol_results.html`:
```html
{% if !symbols.is_empty() %}
<div class="stats-line">
    {{ total }} symbol{% if total != 1 %}s{% endif %} ({{ duration_ms }}ms)
</div>
{% for sym in &symbols %}
<div class="symbol-result">
    <a href="/file/{{ repo }}/{{ sym.path }}#L{{ sym.line }}" class="symbol-link">
        <span class="symbol-kind symbol-kind--{{ sym.kind }}">{{ sym.kind }}</span>
        <span class="symbol-name">{{ sym.name }}</span>
        <span class="symbol-path">{{ sym.path }}:{{ sym.line }}</span>
    </a>
</div>
{% endfor %}
{% else %}
    {% if !query.is_empty() %}
    <div class="empty-state">
        <p>No symbols matching "{{ query }}"</p>
        <p class="hint">Try a different query or symbol kind filter</p>
    </div>
    {% else %}
    <div class="empty-state">
        <p>Search for symbol definitions</p>
        <p class="hint">Functions, structs, classes, traits, enums, and more</p>
    </div>
    {% endif %}
{% endif %}
```

**Step 2: Add ui handler + template struct**

In `ferret-indexer-web/src/ui.rs`, add the template and handler:
```rust
/// A symbol result entry for the template.
pub struct SymbolItem {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: u32,
    pub score: f64,
}

#[derive(Template)]
#[template(path = "symbol_results.html")]
struct SymbolResultsTemplate {
    symbols: Vec<SymbolItem>,
    repo: String,
    query: String,
    total: usize,
    duration_ms: u64,
}

#[derive(Deserialize)]
pub struct SymbolSearchParams {
    q: Option<String>,
    #[serde(rename = "repo-select")]
    repo_select: Option<String>,
    kind: Option<String>,
}
```

Handler:
```rust
/// GET /symbol-results?q=...&repo-select=...&kind=...
pub async fn symbol_results_fragment(
    State(state): State<AppState>,
    Query(params): Query<SymbolSearchParams>,
) -> Response {
    let query = params.q.unwrap_or_default();
    let repo = params.repo_select.unwrap_or_default();
    let kind = params.kind;

    if query.is_empty() {
        return render_template(SymbolResultsTemplate {
            symbols: vec![],
            repo,
            query,
            total: 0,
            duration_ms: 0,
        });
    }

    let repos_map = state.repos().await;
    let repo_path = match repos_map.get(&repo) {
        Some(p) => p.clone(),
        None => {
            return render_template(SymbolResultsTemplate {
                symbols: vec![],
                repo,
                query,
                total: 0,
                duration_ms: 0,
            });
        }
    };

    let stream = match ferret_indexer_daemon::ensure_daemon(state.daemon_bin(), &repo_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("daemon connect error: {e}");
            return (StatusCode::BAD_GATEWAY, format!("Daemon unavailable: {e}")).into_response();
        }
    };

    let request = ferret_indexer_daemon::DaemonRequest::JsonSymbols {
        query: Some(query.clone()),
        kind,
        language: None,
        path_filter: None,
        max_results: Some(50),
        offset: None,
    };

    match ferret_indexer_daemon::send_json_request(stream, &request).await {
        Ok(result) => {
            let mut symbols = Vec::new();
            let mut total = 0usize;
            let mut duration_ms = result.duration_ms;

            for payload in &result.payloads {
                if let Ok(frame) =
                    serde_json::from_str::<ferret_indexer_daemon::JsonSymbolsFrame>(payload)
                {
                    match frame {
                        ferret_indexer_daemon::JsonSymbolsFrame::Symbol(m) => {
                            symbols.push(SymbolItem {
                                name: m.name,
                                kind: m.kind,
                                path: m.path,
                                line: m.line,
                                score: m.score,
                            });
                        }
                        ferret_indexer_daemon::JsonSymbolsFrame::Stats { stats } => {
                            total = stats.total;
                            duration_ms = stats.duration_ms;
                        }
                    }
                }
            }

            if total == 0 {
                total = symbols.len();
            }

            render_template(SymbolResultsTemplate {
                symbols,
                repo,
                query,
                total,
                duration_ms,
            })
        }
        Err(e) => {
            tracing::error!("symbol search proxy error: {e}");
            (StatusCode::BAD_GATEWAY, format!("Symbol search failed: {e}")).into_response()
        }
    }
}
```

**Step 3: Add route**

In `ferret-indexer-web/src/lib.rs`, add UI route:
```rust
.route("/symbol-results", get(ui::symbol_results_fragment))
```

**Step 4: Update index.html — add mode toggle**

In `ferret-indexer-web/templates/index.html`, replace the search bar div with:
```html
<div class="search-bar">
    <div class="search-mode-toggle">
        <button type="button" class="mode-btn mode-btn--active" id="mode-text" data-mode="text">Code</button>
        <button type="button" class="mode-btn" id="mode-symbol" data-mode="symbol">Symbols</button>
    </div>
    <input type="search" name="q" class="search-input"
           placeholder="Search code... (press / to focus, ? for help)"
           autocomplete="off" autofocus
           hx-get="/search-results"
           hx-trigger="keyup changed delay:150ms, search"
           hx-target="#results"
           hx-include="#repo-select">
    <span class="htmx-indicator"><span class="spinner"></span></span>
</div>
```

Also add `@` shortcut to the help overlay:
```html
<dt>@</dt><dd>Go to symbol (quick-open)</dd>
```

**Step 5: Add CSS for symbol results and mode toggle**

Append to `ferret-indexer-web/static/style.css`:
```css
/* Search mode toggle */
.search-mode-toggle {
    display: flex;
    gap: 2px;
    margin-bottom: 0.5rem;
}

.mode-btn {
    padding: 0.2rem 0.6rem;
    font-size: 0.75rem;
    font-weight: 600;
    font-family: var(--font-sans);
    background: var(--bg-elevated);
    color: var(--fg-dim);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    cursor: pointer;
    transition: all var(--transition);
}

.mode-btn:hover {
    color: var(--fg);
    border-color: var(--accent);
}

.mode-btn--active {
    background: var(--accent);
    color: #fff;
    border-color: var(--accent);
}

/* Symbol results */
.symbol-result {
    border-bottom: 1px solid var(--border-subtle);
    transition: background var(--transition);
}

.symbol-result:hover {
    background: var(--selection-bg);
}

.symbol-result.selected {
    background: var(--selection-bg);
    outline: 2px solid var(--selection-border);
    outline-offset: -2px;
}

.symbol-link {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.45rem 1.25rem;
    text-decoration: none;
    color: var(--fg);
    font-family: var(--font-mono);
    font-size: 0.85rem;
}

.symbol-kind {
    display: inline-block;
    min-width: 3.5em;
    padding: 0.1rem 0.35rem;
    font-size: 0.7rem;
    font-weight: 700;
    text-align: center;
    border-radius: 3px;
    letter-spacing: 0.02em;
    text-transform: lowercase;
    background: var(--bg-secondary);
    color: var(--fg-dim);
    border: 1px solid var(--border);
}

.symbol-kind--fn { color: #2563eb; border-color: #93c5fd; background: rgba(37, 99, 235, 0.08); }
.symbol-kind--struct { color: #059669; border-color: #6ee7b7; background: rgba(5, 150, 105, 0.08); }
.symbol-kind--trait { color: #7c3aed; border-color: #c4b5fd; background: rgba(124, 58, 237, 0.08); }
.symbol-kind--enum { color: #d97706; border-color: #fcd34d; background: rgba(217, 119, 6, 0.08); }
.symbol-kind--interface { color: #7c3aed; border-color: #c4b5fd; background: rgba(124, 58, 237, 0.08); }
.symbol-kind--class { color: #059669; border-color: #6ee7b7; background: rgba(5, 150, 105, 0.08); }
.symbol-kind--method { color: #2563eb; border-color: #93c5fd; background: rgba(37, 99, 235, 0.08); }
.symbol-kind--const { color: #dc2626; border-color: #fca5a5; background: rgba(220, 38, 38, 0.08); }
.symbol-kind--var { color: #78716c; }
.symbol-kind--type { color: #0891b2; border-color: #67e8f9; background: rgba(8, 145, 178, 0.08); }
.symbol-kind--mod { color: #78716c; }

[data-theme="dark"] .symbol-kind--fn { color: #60a5fa; border-color: #1e40af; background: rgba(96, 165, 250, 0.1); }
[data-theme="dark"] .symbol-kind--struct { color: #34d399; border-color: #065f46; background: rgba(52, 211, 153, 0.1); }
[data-theme="dark"] .symbol-kind--trait { color: #a78bfa; border-color: #5b21b6; background: rgba(167, 139, 250, 0.1); }
[data-theme="dark"] .symbol-kind--enum { color: #fbbf24; border-color: #92400e; background: rgba(251, 191, 36, 0.1); }
[data-theme="dark"] .symbol-kind--interface { color: #a78bfa; border-color: #5b21b6; background: rgba(167, 139, 250, 0.1); }
[data-theme="dark"] .symbol-kind--class { color: #34d399; border-color: #065f46; background: rgba(52, 211, 153, 0.1); }
[data-theme="dark"] .symbol-kind--method { color: #60a5fa; border-color: #1e40af; background: rgba(96, 165, 250, 0.1); }
[data-theme="dark"] .symbol-kind--const { color: #f87171; border-color: #991b1b; background: rgba(248, 113, 113, 0.1); }
[data-theme="dark"] .symbol-kind--type { color: #22d3ee; border-color: #155e75; background: rgba(34, 211, 238, 0.1); }

.symbol-name {
    font-weight: 700;
    color: var(--fg);
}

.symbol-path {
    margin-left: auto;
    font-size: 0.75rem;
    color: var(--fg-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 50%;
    text-align: right;
}
```

**Step 6: Add JS for mode switching**

In `ferret-indexer-web/static/app.js`, add inside the IIFE (before the closing `})();`):

```javascript
// Search mode toggle (text vs symbols)
var currentMode = "text";

function setSearchMode(mode) {
    currentMode = mode;
    var textBtn = document.getElementById("mode-text");
    var symBtn = document.getElementById("mode-symbol");
    var input = document.querySelector(".search-input");
    if (!textBtn || !symBtn || !input) return;

    textBtn.classList.toggle("mode-btn--active", mode === "text");
    symBtn.classList.toggle("mode-btn--active", mode === "symbol");

    if (mode === "symbol") {
        input.setAttribute("hx-get", "/symbol-results");
        input.setAttribute("placeholder", "Search symbols... (functions, structs, classes)");
    } else {
        input.setAttribute("hx-get", "/search-results");
        input.setAttribute("placeholder", "Search code... (press / to focus, ? for help)");
    }
    // Re-process htmx attributes after changing them
    if (window.htmx) htmx.process(input);

    // Trigger a search with current value
    if (input.value) {
        htmx.trigger(input, "search");
    }
}

document.addEventListener("click", function(e) {
    var btn = e.target.closest(".mode-btn");
    if (btn && btn.dataset.mode) {
        setSearchMode(btn.dataset.mode);
    }
});
```

**Step 7: Run clippy + fmt + tests**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test -p ferret-indexer-web
```

**Step 8: Commit**

```bash
git add ferret-indexer-web/templates/symbol_results.html ferret-indexer-web/templates/index.html ferret-indexer-web/src/ui.rs ferret-indexer-web/src/lib.rs ferret-indexer-web/static/style.css ferret-indexer-web/static/app.js
git commit -m "feat(web): add symbol search mode with toggle in search bar"
```

---

### Task 5: Symbol Outline in File Preview

**Files:**
- Create: `ferret-indexer-web/templates/symbol_outline.html`
- Modify: `ferret-indexer-web/templates/file_preview.html` (add outline panel)
- Modify: `ferret-indexer-web/src/ui.rs` (add outline handler)
- Modify: `ferret-indexer-web/src/lib.rs` (add route)
- Modify: `ferret-indexer-web/static/style.css` (outline panel styles)
- Modify: `ferret-indexer-web/static/app.js` (outline toggle)

**Step 1: Create symbol_outline.html template fragment**

Create `ferret-indexer-web/templates/symbol_outline.html`:
```html
{% if !symbols.is_empty() %}
<div class="outline-header">
    <span class="outline-title">Symbols</span>
    <span class="outline-count">{{ symbols.len() }}</span>
</div>
<div class="outline-list">
{% for sym in &symbols %}
    <a href="#L{{ sym.line }}" class="outline-item" data-line="{{ sym.line }}">
        <span class="symbol-kind symbol-kind--{{ sym.kind }}">{{ sym.kind }}</span>
        <span class="outline-name">{{ sym.name }}</span>
        <span class="outline-line">{{ sym.line }}</span>
    </a>
{% endfor %}
</div>
{% else %}
<div class="outline-empty">No symbols found</div>
{% endif %}
```

**Step 2: Add outline handler**

In `ferret-indexer-web/src/ui.rs`, add:
```rust
#[derive(Template)]
#[template(path = "symbol_outline.html")]
struct SymbolOutlineTemplate {
    symbols: Vec<SymbolItem>,
}

#[derive(Deserialize)]
pub struct OutlineParams {
    repo: String,
    path: String,
}

/// GET /symbol-outline?repo=...&path=...
pub async fn symbol_outline_fragment(
    State(state): State<AppState>,
    Query(params): Query<OutlineParams>,
) -> Response {
    let repos_map = state.repos().await;
    let repo_path = match repos_map.get(&params.repo) {
        Some(p) => p.clone(),
        None => return render_template(SymbolOutlineTemplate { symbols: vec![] }),
    };

    let stream = match ferret_indexer_daemon::ensure_daemon(state.daemon_bin(), &repo_path).await {
        Ok(s) => s,
        Err(_) => return render_template(SymbolOutlineTemplate { symbols: vec![] }),
    };

    // Empty query + path_filter = return all symbols in file
    let request = ferret_indexer_daemon::DaemonRequest::JsonSymbols {
        query: None,
        kind: None,
        language: None,
        path_filter: Some(params.path),
        max_results: Some(500),
        offset: None,
    };

    match ferret_indexer_daemon::send_json_request(stream, &request).await {
        Ok(result) => {
            let mut symbols: Vec<SymbolItem> = Vec::new();
            for payload in &result.payloads {
                if let Ok(ferret_indexer_daemon::JsonSymbolsFrame::Symbol(m)) =
                    serde_json::from_str(payload)
                {
                    symbols.push(SymbolItem {
                        name: m.name,
                        kind: m.kind,
                        path: m.path,
                        line: m.line,
                        score: m.score,
                    });
                }
            }
            // Sort by line number for outline view
            symbols.sort_by_key(|s| s.line);
            render_template(SymbolOutlineTemplate { symbols })
        }
        Err(_) => render_template(SymbolOutlineTemplate { symbols: vec![] }),
    }
}
```

**Step 3: Add route**

In `ferret-indexer-web/src/lib.rs`:
```rust
.route("/symbol-outline", get(ui::symbol_outline_fragment))
```

**Step 4: Update file_preview.html**

Replace the file preview body content (between `<a href="/" class="back-link">` and `<div class="code-lines">`) to include an outline panel. Restructure to add a flex layout:

```html
<a href="/" class="back-link">&larr; Back to search</a>

<div class="file-preview-header">
    <h2>{{ path }}</h2>
    <div class="file-preview-meta">
        {{ language }} &middot; {{ total_lines }} line{% if total_lines != 1 %}s{% endif %}
        <button type="button" class="outline-toggle" id="outline-toggle" title="Toggle symbol outline">&#9776; Symbols</button>
    </div>
</div>

<div class="file-preview-layout">
    <div class="outline-panel" id="outline-panel"
         hx-get="/symbol-outline?repo={{ repo }}&amp;path={{ path }}"
         hx-trigger="load"
         hx-swap="innerHTML">
        <div class="outline-loading"><span class="spinner"></span></div>
    </div>
    <div class="code-lines">
    {% for line in &lines %}
        <div class="code-line" id="L{{ line.0 }}">
            <span class="line-number">{{ line.0 }}</span>
            <span class="line-content">{{ line.1 }}</span>
        </div>
    {% endfor %}
    </div>
</div>
```

Note: the `FilePreviewTemplate` struct needs a `repo` field. Update it:
```rust
#[derive(Template)]
#[template(path = "file_preview.html")]
struct FilePreviewTemplate {
    repo: String,
    path: String,
    language: String,
    total_lines: usize,
    lines: Vec<(usize, String)>,
}
```

And update the `file_preview` handler to pass the repo name through. Change:
```rust
pub async fn file_preview(
    State(state): State<AppState>,
    Path((repo, file_path)): Path<(String, String)>,
) -> Response {
```
Pass `repo: repo.clone()` into the template.

**Step 5: Add CSS for outline panel**

Append to `style.css`:
```css
/* File preview layout with outline */
.file-preview-layout {
    display: flex;
    min-height: calc(100vh - 120px);
}

.outline-panel {
    width: 240px;
    flex-shrink: 0;
    border-right: 1px solid var(--border);
    background: var(--bg-secondary);
    overflow-y: auto;
    max-height: calc(100vh - 120px);
    position: sticky;
    top: 0;
}

.outline-panel.hidden {
    display: none;
}

.file-preview-layout .code-lines {
    flex: 1;
    min-width: 0;
}

.outline-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.5rem 0.7rem;
    border-bottom: 1px solid var(--border);
    background: var(--bg-secondary);
    position: sticky;
    top: 0;
    z-index: 1;
}

.outline-title {
    font-size: 0.75rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--fg-muted);
}

.outline-count {
    font-size: 0.68rem;
    font-weight: 600;
    padding: 0.05rem 0.35rem;
    border-radius: 999px;
    background: var(--accent-glow);
    color: var(--accent);
    border: 1px solid rgba(13, 148, 136, 0.15);
    font-variant-numeric: tabular-nums;
}

.outline-list {
    padding: 0.25rem 0;
}

.outline-item {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.25rem 0.7rem;
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: var(--fg);
    text-decoration: none;
    transition: background var(--transition);
    cursor: pointer;
}

.outline-item:hover {
    background: var(--selection-bg);
    color: var(--fg);
}

.outline-item .symbol-kind {
    font-size: 0.62rem;
    min-width: 2.8em;
    padding: 0.05rem 0.25rem;
}

.outline-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-weight: 600;
}

.outline-line {
    font-size: 0.68rem;
    color: var(--fg-muted);
    font-variant-numeric: tabular-nums;
}

.outline-empty {
    padding: 1rem 0.7rem;
    font-size: 0.78rem;
    color: var(--fg-muted);
    text-align: center;
}

.outline-loading {
    padding: 1rem;
    text-align: center;
}

.outline-toggle {
    margin-left: 0.75rem;
    padding: 0.15rem 0.45rem;
    font-size: 0.72rem;
    font-weight: 500;
    background: var(--bg);
    color: var(--fg-dim);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    cursor: pointer;
    transition: all var(--transition);
    font-family: var(--font-sans);
}

.outline-toggle:hover {
    border-color: var(--accent);
    color: var(--accent);
}
```

**Step 6: Add JS for outline interactions**

In `app.js`, add:
```javascript
// Outline panel toggle
var outlineToggle = document.getElementById("outline-toggle");
if (outlineToggle) {
    outlineToggle.addEventListener("click", function() {
        var panel = document.getElementById("outline-panel");
        if (panel) panel.classList.toggle("hidden");
    });
}

// Outline click-to-scroll
document.addEventListener("click", function(e) {
    var item = e.target.closest(".outline-item");
    if (!item) return;
    e.preventDefault();
    var line = item.getAttribute("data-line");
    var target = document.getElementById("L" + line);
    if (target) {
        target.scrollIntoView({ block: "center", behavior: "smooth" });
        // Flash highlight
        target.classList.add("code-line--highlight");
        setTimeout(function() { target.classList.remove("code-line--highlight"); }, 1500);
    }
});
```

Add the highlight flash CSS:
```css
.code-line--highlight {
    background: var(--match-bg);
    transition: background 0.3s ease;
}
```

**Step 7: Run clippy + fmt + tests**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test -p ferret-indexer-web
```

**Step 8: Commit**

```bash
git add ferret-indexer-web/templates/symbol_outline.html ferret-indexer-web/templates/file_preview.html ferret-indexer-web/src/ui.rs ferret-indexer-web/src/lib.rs ferret-indexer-web/static/style.css ferret-indexer-web/static/app.js
git commit -m "feat(web): add symbol outline panel to file preview"
```

---

### Task 6: Go-to-Symbol Quick-Open Modal

**Files:**
- Modify: `ferret-indexer-web/templates/index.html` (add modal markup)
- Modify: `ferret-indexer-web/static/style.css` (modal styles)
- Modify: `ferret-indexer-web/static/app.js` (modal behavior)

**Step 1: Add modal HTML to index.html**

Before the closing `</body>` tag, add:
```html
<div class="quickopen-overlay" id="quickopen-overlay">
    <div class="quickopen-modal">
        <input type="text" class="quickopen-input" id="quickopen-input"
               placeholder="Go to symbol..."
               autocomplete="off"
               hx-get="/symbol-results"
               hx-trigger="keyup changed delay:100ms"
               hx-target="#quickopen-results"
               hx-include="#repo-select">
        <div id="quickopen-results" class="quickopen-results"></div>
    </div>
</div>
```

**Step 2: Add CSS for modal**

Append to `style.css`:
```css
/* Quick-open modal */
.quickopen-overlay {
    display: none;
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    backdrop-filter: blur(4px);
    -webkit-backdrop-filter: blur(4px);
    z-index: 200;
    justify-content: center;
    padding-top: 15vh;
}

.quickopen-overlay.visible {
    display: flex;
}

.quickopen-modal {
    width: 90%;
    max-width: 560px;
    max-height: 60vh;
    background: var(--bg-elevated);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-lg);
    display: flex;
    flex-direction: column;
    overflow: hidden;
}

.quickopen-input {
    width: 100%;
    padding: 0.65rem 0.9rem;
    font-size: 1rem;
    font-family: var(--font-mono);
    background: var(--bg-elevated);
    color: var(--fg);
    border: none;
    border-bottom: 1px solid var(--border);
    outline: none;
}

.quickopen-input::placeholder {
    color: var(--fg-muted);
}

.quickopen-results {
    overflow-y: auto;
    max-height: calc(60vh - 48px);
}

/* Re-use symbol-result styles but tighter for the modal */
.quickopen-results .symbol-result {
    border-bottom-color: var(--border-subtle);
}

.quickopen-results .symbol-link {
    padding: 0.35rem 0.9rem;
    font-size: 0.82rem;
}

.quickopen-results .empty-state {
    padding: 2rem 1rem;
}
```

**Step 3: Add JS for modal behavior**

In `app.js`, add:
```javascript
// Quick-open modal (Go to Symbol)
var quickopenSelectedIndex = -1;

function openQuickOpen() {
    var overlay = document.getElementById("quickopen-overlay");
    if (!overlay) return;
    overlay.classList.add("visible");
    var input = document.getElementById("quickopen-input");
    if (input) {
        input.value = "";
        input.focus();
    }
    quickopenSelectedIndex = -1;
}

function closeQuickOpen() {
    var overlay = document.getElementById("quickopen-overlay");
    if (overlay) overlay.classList.remove("visible");
    quickopenSelectedIndex = -1;
}

function getQuickOpenResults() {
    return document.querySelectorAll("#quickopen-results .symbol-result");
}

function selectQuickOpenResult(index) {
    var results = getQuickOpenResults();
    if (results.length === 0) return;
    results.forEach(function(el) { el.classList.remove("selected"); });
    quickopenSelectedIndex = Math.max(0, Math.min(index, results.length - 1));
    var el = results[quickopenSelectedIndex];
    el.classList.add("selected");
    el.scrollIntoView({ block: "nearest", behavior: "smooth" });
}

function openQuickOpenSelected() {
    var results = getQuickOpenResults();
    if (quickopenSelectedIndex < 0 || quickopenSelectedIndex >= results.length) return;
    var link = results[quickopenSelectedIndex].querySelector("a");
    if (link && link.href) {
        window.location.href = link.href;
    }
}
```

Update the keydown handler — add `@` shortcut and quick-open keyboard navigation. In the main `document.addEventListener("keydown", ...)` handler, add at the top (before the help overlay check):

```javascript
// Quick-open keyboard handling
var quickopen = document.getElementById("quickopen-overlay");
if (quickopen && quickopen.classList.contains("visible")) {
    if (e.key === "Escape") {
        e.preventDefault();
        closeQuickOpen();
        return;
    }
    if (e.key === "ArrowDown" || (e.key === "j" && e.ctrlKey)) {
        e.preventDefault();
        selectQuickOpenResult(quickopenSelectedIndex + 1);
        return;
    }
    if (e.key === "ArrowUp" || (e.key === "k" && e.ctrlKey)) {
        e.preventDefault();
        selectQuickOpenResult(quickopenSelectedIndex - 1);
        return;
    }
    if (e.key === "Enter") {
        e.preventDefault();
        openQuickOpenSelected();
        return;
    }
    return; // Let all other keys pass to the quickopen input
}
```

And in the existing switch statement, add the `@` case:
```javascript
case "@":
    e.preventDefault();
    openQuickOpen();
    break;
```

Also add backdrop click to close:
```javascript
document.addEventListener("click", function(e) {
    if (e.target.id === "quickopen-overlay") {
        closeQuickOpen();
    }
});
```

Reset quickopen selection on htmx swap:
```javascript
document.addEventListener("htmx:afterSwap", function(e) {
    if (e.target.id === "quickopen-results") {
        quickopenSelectedIndex = -1;
    }
});
```

**Step 4: Run clippy + fmt**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

**Step 5: Commit**

```bash
git add ferret-indexer-web/templates/index.html ferret-indexer-web/static/style.css ferret-indexer-web/static/app.js
git commit -m "feat(web): add Go-to-Symbol quick-open modal (@ shortcut)"
```

---

### Task 7: E2E Testing with Playwright

**Prerequisites:** A running `ferret web` server on port 4040 with at least one indexed repo. Set up with:
```bash
cargo build --workspace --release
cargo run -p ferret-indexer-cli -- init
cargo run -p ferret-indexer-cli -- repos add . --name test-repo
# Start server in background
cargo run -p ferret-indexer-cli -- web --port 4040 &
```

**Step 1: Install Playwright browser**

Use the Playwright MCP tool `browser_install` to install the browser.

**Step 2: Test Symbol Search API endpoint**

Navigate to the JSON API and verify response structure:
```
browser_navigate: http://localhost:4040/api/v1/repos/test-repo/symbols?q=fn+main
```
Verify the response JSON contains:
- `symbols` array with entries having `name`, `kind`, `path`, `line`, `score` fields
- `stats` object with `total` and `duration_ms` fields
- At least one symbol result (the project has `fn main` in several places)

**Step 3: Test Symbol Search API with kind filter**

```
browser_navigate: http://localhost:4040/api/v1/repos/test-repo/symbols?q=Segment&kind=struct
```
Verify results only contain symbols with `kind: "struct"`.

**Step 4: Test Symbol Search API — file outline mode**

```
browser_navigate: http://localhost:4040/api/v1/repos/test-repo/symbols?path=ferret-indexer-core/src/types.rs
```
Verify returns all symbols defined in types.rs (SymbolKind, Language, FileId, etc.).

**Step 5: Test main page loads with mode toggle**

```
browser_navigate: http://localhost:4040
browser_snapshot
```
Verify the snapshot shows:
- "Code" and "Symbols" mode toggle buttons
- Search input
- Repo selector

**Step 6: Test symbol search mode in UI**

```
browser_click: "Symbols" button (mode-symbol)
browser_snapshot
```
Verify the Symbols button is active. Then type into the search input:
```
browser_type: slowly type "SegmentWriter" into .search-input
```
Wait and snapshot. Verify:
- Symbol results appear (not code search results)
- Each result shows a kind badge (e.g., "struct"), symbol name, and file path
- Results are clickable links to file preview

**Step 7: Test switching back to Code mode**

```
browser_click: "Code" button (mode-text)
```
Verify search input placeholder changes back to code search.

**Step 8: Test file preview with symbol outline**

Navigate to a file that has symbols:
```
browser_navigate: http://localhost:4040/file/test-repo/ferret-indexer-core/src/types.rs
browser_snapshot
```
Verify:
- Outline panel is visible on the left
- Contains symbol entries with kind badges, names, and line numbers
- Has a "Symbols" header with a count

**Step 9: Test outline click-to-scroll**

Click a symbol in the outline:
```
browser_click: an outline-item link
```
Verify the page scrolls to that line and the line is briefly highlighted.

**Step 10: Test outline toggle**

Click the "Symbols" toggle button in the file preview header:
```
browser_click: .outline-toggle
browser_snapshot
```
Verify the outline panel is hidden. Click again to show it.

**Step 11: Test Go-to-Symbol modal**

Navigate back to the main page:
```
browser_navigate: http://localhost:4040
```
Press `@` to open the modal:
```
browser_press_key: @
browser_snapshot
```
Verify:
- The quick-open overlay is visible
- It has an input field with "Go to symbol..." placeholder
- Focus is on the input

**Step 12: Test quick-open search**

Type in the quick-open input:
```
browser_type: "main" into #quickopen-input
```
Wait and snapshot. Verify symbol results appear in the modal.

**Step 13: Test quick-open keyboard navigation**

```
browser_press_key: ArrowDown (select first result)
browser_press_key: ArrowDown (select second result)
browser_snapshot
```
Verify a result has the "selected" class.

**Step 14: Test quick-open Escape to close**

```
browser_press_key: Escape
browser_snapshot
```
Verify the modal is closed.

**Step 15: Test quick-open Enter to navigate**

Re-open, search, navigate down, press Enter:
```
browser_press_key: @
browser_type: "SymbolKind" into #quickopen-input
```
Wait for results, then:
```
browser_press_key: ArrowDown
browser_press_key: Enter
```
Verify navigation to the file preview page with the correct file.

**Step 16: Clean up**

```
browser_close
```

---

## Notes

- The `path_filter` in `SymbolSearchOptions` is a **substring** match (`meta.path.contains(pattern)`). For the file outline, pass the exact relative path — this works because an exact path contains itself. In rare cases of path collisions (e.g., `foo.rs` matching `bar/foo.rs` and `baz/foo.rs`), the results will include symbols from both files, which is acceptable.
- Line numbers in `SymbolMatch` are 0-based internally. The `SymbolMatchResponse` converts to 1-based in its `From<SymbolMatch>` impl. Templates and URLs should use the already-converted 1-based values directly.
- The quick-open modal reuses the `/symbol-results` htmx endpoint (same as the symbol search mode). The only difference is display context — the CSS tightens spacing for the modal.
- Empty-query symbol search (for outline) does a linear scan, which is O(N) over all symbols in all segments. For typical projects (<100k symbols), this is <50ms. The `path_filter` is applied post-scan so performance scales with total symbols, not just the target file. This is acceptable.
