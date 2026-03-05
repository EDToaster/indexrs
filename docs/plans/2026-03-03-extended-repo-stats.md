# Extended Repo Stats Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Extend the daemon `StatusResponse` with five new fields (index disk size, last indexed time, language breakdown, tombstone ratio, path validity) and display them on the web repos overview page.

**Architecture:** The daemon's `DaemonRequest::Status` handler already has access to the `SegmentManager` snapshot, `repo_root`, and `ferret_dir`. We compute the new stats in the handler (mirroring patterns from `ferret-indexer-cli/src/status.rs`), add fields to `StatusResponse`, propagate through the web proxy, and render on the repos page. The `dir_size()` utility is moved to `ferret-indexer-core` so both the CLI and daemon can use it.

**Tech Stack:** Rust (serde, askama templates), CSS, existing daemon/web infrastructure.

---

### Task 1: Move `dir_size` utility to `ferret-indexer-core`

The CLI's `status.rs:181-196` has a `dir_size()` function. Move it to `ferret-indexer-core` so the daemon handler can use it too.

**Files:**
- Create: `ferret-indexer-core/src/disk.rs`
- Modify: `ferret-indexer-core/src/lib.rs`
- Modify: `ferret-indexer-cli/src/status.rs`

**Step 1: Create `ferret-indexer-core/src/disk.rs`**

```rust
use std::path::Path;

/// Recursively compute the total size of all files under `path`.
pub fn dir_size(path: &Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                total += dir_size(&entry.path());
            } else if ft.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}
```

**Step 2: Export from `ferret-indexer-core/src/lib.rs`**

Add after the existing `pub mod content;` line:
```rust
pub mod disk;
```

Add in the `pub use` section:
```rust
pub use disk::dir_size;
```

**Step 3: Update CLI `status.rs` to use `ferret_indexer_core::dir_size`**

In `ferret-indexer-cli/src/status.rs`, remove the local `dir_size` function (lines 181-196) and update the import at the top to include `dir_size`:

```rust
use ferret_indexer_core::{Language, SegmentManager, dir_size, search_segments};
```

Update the call at line 137: `let disk_bytes = dir_size(&segments_dir);` — this already matches the signature, no change needed.

**Step 4: Run tests**

Run: `cargo test --workspace`
Run: `cargo clippy --workspace -- -D warnings`

**Step 5: Commit**

```
feat(core): move dir_size utility to ferret-indexer-core for reuse
```

---

### Task 2: Extend `StatusResponse` with new fields

Add five new fields to the daemon protocol's `StatusResponse` struct with backward-compatible defaults.

**Files:**
- Modify: `ferret-indexer-daemon/src/json_protocol.rs:36-41`

**Step 1: Update `StatusResponse` struct**

Replace the current struct with:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub files_indexed: usize,
    pub segments: usize,
    /// Total bytes on disk for the index directory (.ferret_index/segments/).
    #[serde(default)]
    pub index_bytes: u64,
    /// Unix epoch seconds of the most recently modified file in the index.
    #[serde(default)]
    pub last_indexed_ts: u64,
    /// Top languages by file count: vec of (language_name, file_count).
    #[serde(default)]
    pub languages: Vec<(String, usize)>,
    /// Fraction of entries that are tombstoned (0.0 to 1.0).
    #[serde(default)]
    pub tombstone_ratio: f32,
    /// Whether the registered repo path exists on disk.
    #[serde(default = "default_true")]
    pub path_valid: bool,
}

fn default_true() -> bool {
    true
}
```

Using `#[serde(default)]` ensures old JSON payloads (from older daemons) still deserialize.

**Step 2: Run tests**

Run: `cargo test --workspace`
Run: `cargo clippy --workspace -- -D warnings`

**Step 3: Commit**

```
feat(daemon): extend StatusResponse with disk size, last indexed, languages, tombstone ratio, path validity
```

---

### Task 3: Compute new stats in the daemon Status handler

Update the `DaemonRequest::Status` handler in `ferret-indexer-cli/src/daemon.rs` to populate the new fields.

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs:892-931` (the `DaemonRequest::Status` match arm)

**Step 1: Replace the Status handler**

Replace the `DaemonRequest::Status => { ... }` block (lines 892-931) with:

```rust
DaemonRequest::Status => {
    let snapshot = manager.snapshot();
    let mut files_indexed: usize = 0;
    let mut total_entries: u32 = 0;
    let mut total_tombstoned: u32 = 0;
    let mut last_mtime: u64 = 0;
    let mut lang_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for seg in snapshot.iter() {
        let tombstones = seg.load_tombstones()?;
        let count = seg.entry_count();
        total_entries += count;
        total_tombstoned += tombstones.len();

        let reader = seg.metadata_reader();
        for entry_result in reader.iter_all() {
            let entry = entry_result?;
            if tombstones.contains(entry.file_id) {
                continue;
            }
            files_indexed += 1;
            if entry.mtime_epoch_secs > last_mtime {
                last_mtime = entry.mtime_epoch_secs;
            }
            *lang_counts
                .entry(entry.language.to_string())
                .or_insert(0) += 1;
        }
    }

    // Sort languages by count descending, take top 10.
    let mut languages: Vec<(String, usize)> =
        lang_counts.into_iter().collect();
    languages.sort_by(|a, b| b.1.cmp(&a.1));
    languages.truncate(10);

    let tombstone_ratio = if total_entries == 0 {
        0.0
    } else {
        total_tombstoned as f32 / total_entries as f32
    };

    let segments_dir = ferret_dir.join("segments");
    let index_bytes = ferret_indexer_core::dir_size(&segments_dir);
    let path_valid = repo_root.is_dir();

    let status = if caught_up.load(Ordering::Relaxed) {
        "ready"
    } else {
        "catching_up"
    };

    let resp = StatusResponse {
        status: status.to_string(),
        files_indexed,
        segments: snapshot.len(),
        index_bytes,
        last_indexed_ts: last_mtime,
        languages,
        tombstone_ratio,
        path_valid,
    };
    let payload = serde_json::to_string(&resp)
        .map_err(|e| IndexError::Io(std::io::Error::other(e)))?;
    wire::write_response(&mut writer, &DaemonResponse::Json { payload })
        .await
        .map_err(IndexError::Io)?;
    wire::write_response(
        &mut writer,
        &DaemonResponse::Done {
            total: 1,
            duration_ms: 0,
            stale: !caught_up.load(Ordering::Relaxed),
        },
    )
    .await
    .map_err(IndexError::Io)?;
}
```

**Step 2: Run tests**

Run: `cargo test --workspace`
Run: `cargo clippy --workspace -- -D warnings`

**Step 3: Commit**

```
feat(daemon): compute extended stats in Status handler
```

---

### Task 4: Update the daemon Status test

The existing daemon integration test at `ferret-indexer-cli/src/daemon.rs` (around line 2108) checks `files_indexed` and `segments`. Add assertions for the new fields.

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs` (test near line 2108-2119)

**Step 1: Add assertions for new fields after the existing assertions**

After the existing `assert!(status.segments > 0, ...)` assertion, add:

```rust
assert!(
    status.index_bytes > 0,
    "expected index_bytes > 0, got {}",
    status.index_bytes
);
assert!(
    status.last_indexed_ts > 0,
    "expected last_indexed_ts > 0, got {}",
    status.last_indexed_ts
);
assert!(
    !status.languages.is_empty(),
    "expected non-empty languages breakdown"
);
// Should contain Rust since we indexed .rs files
assert!(
    status.languages.iter().any(|(lang, _)| lang == "Rust"),
    "expected Rust in languages, got: {:?}",
    status.languages
);
assert!(
    status.path_valid,
    "expected path_valid to be true"
);
assert!(
    status.tombstone_ratio >= 0.0 && status.tombstone_ratio <= 1.0,
    "expected tombstone_ratio in [0, 1], got {}",
    status.tombstone_ratio
);
```

**Step 2: Run tests**

Run: `cargo test --workspace`

**Step 3: Commit**

```
test(daemon): add assertions for extended StatusResponse fields
```

---

### Task 5: Update web `RepoOverviewItem` and template with new stats

Update the web layer to propagate and display the new fields.

**Files:**
- Modify: `ferret-indexer-web/src/ui.rs` — update `RepoOverviewItem` struct and `repos_page` handler
- Modify: `ferret-indexer-web/templates/repos.html` — display new stats
- Modify: `ferret-indexer-web/static/style.css` — styles for new UI elements

**Step 1: Update `RepoOverviewItem` in `ui.rs`**

Replace the existing struct with:

```rust
/// A repo entry for the repos overview page.
pub struct RepoOverviewItem {
    pub name: String,
    pub path: String,
    pub status: String,
    pub files_indexed: usize,
    pub segments: usize,
    pub online: bool,
    pub index_bytes: String,
    pub last_indexed: String,
    pub languages: Vec<(String, usize)>,
    pub tombstone_ratio: f32,
    pub tombstone_pct: String,
    pub needs_compaction: bool,
    pub path_valid: bool,
}
```

**Step 2: Add a `format_bytes` helper in `ui.rs`**

Add near the other helpers (before the handlers section):

```rust
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_relative_time(epoch_secs: u64) -> String {
    if epoch_secs == 0 {
        return "never".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if epoch_secs > now {
        return "just now".to_string();
    }
    let delta = now - epoch_secs;
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}
```

**Step 3: Update the `repos_page` handler to populate new fields**

Replace the body of the `for name in &repo_names` loop in `repos_page`:

```rust
for name in &repo_names {
    let path = repos_map[name].clone();
    let (status, files_indexed, segments, online, index_bytes, last_indexed_ts, languages, tombstone_ratio, path_valid) =
        match proxy_status_raw(state.daemon_bin(), &path).await {
            Ok(sr) => (
                sr.status.clone(),
                sr.files_indexed,
                sr.segments,
                true,
                sr.index_bytes,
                sr.last_indexed_ts,
                sr.languages.clone(),
                sr.tombstone_ratio,
                sr.path_valid,
            ),
            Err(_) => ("offline".to_string(), 0, 0, false, 0, 0, vec![], 0.0, path.is_dir()),
        };
    let tombstone_pct = format!("{:.1}%", tombstone_ratio * 100.0);
    let needs_compaction = tombstone_ratio > 0.3;
    repos.push(RepoOverviewItem {
        name: name.clone(),
        path: path.display().to_string(),
        status,
        files_indexed,
        segments,
        online,
        index_bytes: format_bytes(index_bytes),
        last_indexed: format_relative_time(last_indexed_ts),
        languages,
        tombstone_ratio,
        tombstone_pct,
        needs_compaction,
        path_valid,
    });
}
```

**Step 4: Update `repos.html` template**

Replace the repo card body (everything inside `{% for repo in &repos %}...{% endfor %}`) with:

```html
<div class="repo-card{% if !repo.online %} repo-card--offline{% endif %}{% if !repo.path_valid %} repo-card--invalid{% endif %}">
    <div class="repo-card-header">
        <span class="status-dot{% if !repo.online %} status-dot--offline{% endif %}" title="{{ repo.status }}"></span>
        <span class="repo-card-name">{{ repo.name }}</span>
        <span class="repo-card-status badge{% if repo.online %} badge--ok{% else %} badge--error{% endif %}">{{ repo.status }}</span>
    </div>
    <div class="repo-card-path">
        {{ repo.path }}
        {% if !repo.path_valid %}
        <span class="path-warning">path not found</span>
        {% endif %}
    </div>
    <div class="repo-card-stats">
        <span class="repo-stat">
            <span class="repo-stat-value">{{ repo.files_indexed }}</span>
            <span class="repo-stat-label">files</span>
        </span>
        <span class="repo-stat">
            <span class="repo-stat-value">{{ repo.segments }}</span>
            <span class="repo-stat-label">segments</span>
        </span>
        <span class="repo-stat">
            <span class="repo-stat-value">{{ repo.index_bytes }}</span>
            <span class="repo-stat-label">on disk</span>
        </span>
        <span class="repo-stat">
            <span class="repo-stat-value">{{ repo.last_indexed }}</span>
            <span class="repo-stat-label">indexed</span>
        </span>
    </div>
    {% if !repo.languages.is_empty() %}
    <div class="repo-card-langs">
        {% for lang in &repo.languages %}
        <span class="lang-chip">{{ lang.0 }} <span class="lang-chip-count">{{ lang.1 }}</span></span>
        {% endfor %}
    </div>
    {% endif %}
    {% if repo.needs_compaction %}
    <div class="repo-card-warning">
        <span class="tombstone-warning">{{ repo.tombstone_pct }} tombstoned — compaction recommended</span>
    </div>
    {% endif %}
</div>
```

**Step 5: Add CSS for new elements**

Append to the repos page section in `style.css`:

```css
.repo-card--invalid {
    border-left-color: var(--fg-muted);
    opacity: 0.6;
}

.path-warning {
    display: inline-block;
    font-size: 0.65rem;
    font-weight: 600;
    color: var(--danger);
    margin-left: 0.4rem;
}

.repo-card-langs {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    padding: 0.45rem 0.85rem;
    border-top: 1px solid var(--border-subtle);
}

.lang-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.2rem;
    font-size: 0.68rem;
    font-weight: 500;
    padding: 0.1rem 0.4rem;
    border-radius: 3px;
    background: var(--bg);
    border: 1px solid var(--border);
    color: var(--fg-dim);
    letter-spacing: 0.02em;
}

.lang-chip-count {
    font-variant-numeric: tabular-nums;
    font-weight: 700;
    color: var(--fg);
}

.repo-card-warning {
    padding: 0.4rem 0.85rem;
    border-top: 1px solid var(--border-subtle);
}

.tombstone-warning {
    font-size: 0.7rem;
    color: var(--danger);
    font-weight: 500;
}

[data-theme="dark"] .tombstone-warning {
    color: var(--danger);
}
```

**Step 6: Run tests and lint**

Run: `cargo test --workspace`
Run: `cargo clippy --workspace -- -D warnings`
Run: `cargo fmt --all -- --check`

**Step 7: Commit**

```
feat(web): display extended repo stats on overview page
```

---

### Task 6: Update SSE fallback strings

The SSE status stream in `ferret-indexer-web/src/sse.rs` has hardcoded fallback JSON strings that only include the original 3 fields. Update them to include the new fields with zero/default values.

**Files:**
- Modify: `ferret-indexer-web/src/sse.rs:156-168`

**Step 1: Update fallback strings**

Replace the three hardcoded fallback JSON strings in `status_stream()` with:

```rust
// For "unknown" status (line 156):
r#"{"status":"unknown","files_indexed":0,"segments":0,"index_bytes":0,"last_indexed_ts":0,"languages":[],"tombstone_ratio":0.0,"path_valid":true}"#.to_string()

// For "offline" status (lines 161 and 167):
r#"{"status":"offline","files_indexed":0,"segments":0,"index_bytes":0,"last_indexed_ts":0,"languages":[],"tombstone_ratio":0.0,"path_valid":true}"#.to_string()
```

**Step 2: Run tests**

Run: `cargo test --workspace`
Run: `cargo clippy --workspace -- -D warnings`

**Step 3: Commit**

```
fix(web): update SSE fallback status strings with new fields
```

---

### Task 7: Final verification

**Step 1: Run the full CI suite**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

**Step 2: Commit if any formatting fixes are needed**

```
style: format
```
