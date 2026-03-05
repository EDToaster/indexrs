# Reindex Progress Bars Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add rich indicatif progress bars to `ferret reindex` and `ferret init` so users see per-file progress percentages, phase labels, and change breakdowns instead of static text messages.

**Architecture:** Three-layer change. (1) Add a `ReindexProgress` enum in `ferret-indexer-core` carrying structured data (phase, counts, totals). Refactor `run_catchup_with_progress` and add `apply_changes_with_progress` to emit these events. (2) Daemon serializes `ReindexProgress` as JSON inside the existing `DaemonResponse::Progress { message }` wire type — no protocol changes. (3) CLI parses the JSON, drives `indicatif` progress bars client-side. Falls back to plain text for unparseable messages. (4) Rewrite `init.rs` to use indicatif instead of the custom `ProgressLine` struct, preserving all existing functionality (skip breakdowns, content bytes, auto-registration).

**Tech Stack:** Rust, serde (already in ferret-indexer-core), indicatif (new dep in ferret-indexer-cli)

---

### Task 1: Add `ReindexProgress` enum to ferret-indexer-core

**Files:**
- Create: `ferret-indexer-core/src/reindex_progress.rs`
- Modify: `ferret-indexer-core/src/lib.rs:1-40` (add module + re-export)

**Step 1: Create the `ReindexProgress` enum**

Create `ferret-indexer-core/src/reindex_progress.rs`:

```rust
//! Structured progress events emitted during reindex operations.

use serde::{Deserialize, Serialize};

/// A structured progress event emitted during reindex.
///
/// Sent as JSON over the daemon wire protocol inside
/// `DaemonResponse::Progress { message }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReindexProgress {
    /// Change detection started.
    DetectingChanges,
    /// Fell back to hash-based scanning (git unavailable).
    ScanningFallback,
    /// Change detection complete.
    ChangesDetected {
        created: usize,
        modified: usize,
        deleted: usize,
    },
    /// No changes found.
    NoChanges,
    /// Reading and filtering changed files before indexing.
    PreparingFiles {
        current: usize,
        total: usize,
    },
    /// Building a segment: file `files_done` of `files_total` processed.
    BuildingSegment {
        segment_id: u32,
        files_done: usize,
        files_total: usize,
    },
    /// Writing tombstones for old file entries.
    Tombstoning {
        count: u32,
    },
    /// Segment compaction started.
    CompactingSegments {
        input_segments: usize,
    },
    /// Reindex finished successfully.
    Complete {
        changes_applied: usize,
    },
}
```

**Step 2: Wire up module and re-export in `lib.rs`**

In `ferret-indexer-core/src/lib.rs`, add `pub mod reindex_progress;` after the `pub mod recovery;` line, and add `pub use reindex_progress::ReindexProgress;` after the `recover_segments` re-export line.

**Step 3: Run `cargo check -p ferret-indexer-core`**

Expected: PASS — new module compiles, no consumers yet.

**Step 4: Add serde roundtrip test**

Append to the bottom of `ferret-indexer-core/src/reindex_progress.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serde_roundtrip_detecting_changes() {
        let event = ReindexProgress::DetectingChanges;
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"type":"detecting_changes"}"#);
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_building_segment() {
        let event = ReindexProgress::BuildingSegment {
            segment_id: 3,
            files_done: 100,
            files_total: 500,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_changes_detected() {
        let event = ReindexProgress::ChangesDetected {
            created: 10,
            modified: 20,
            deleted: 5,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn test_serde_roundtrip_complete() {
        let event = ReindexProgress::Complete {
            changes_applied: 42,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }
}
```

**Step 5: Run tests**

Run: `cargo test -p ferret-indexer-core -- test_serde_roundtrip`
Expected: 4 PASS

**Step 6: Commit**

```
feat(core): add ReindexProgress enum for structured reindex events
```

---

### Task 2: Add `apply_changes_with_progress` to `SegmentManager`

**Files:**
- Modify: `ferret-indexer-core/src/segment_manager.rs:373-491` (add new method)

**Step 1: Write the failing test**

Add at the bottom of the `#[cfg(test)] mod tests` block in `segment_manager.rs`:

```rust
    #[test]
    fn test_apply_changes_with_progress_reports_events() {
        use crate::reindex_progress::ReindexProgress;

        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path();
        let ferret_dir = repo_dir.join(".ferret_index");
        fs::create_dir_all(ferret_dir.join("segments")).unwrap();

        // Write a source file.
        fs::write(repo_dir.join("hello.rs"), "fn hello() {}").unwrap();

        let manager = SegmentManager::new(&ferret_dir).unwrap();

        let changes = vec![ChangeEvent {
            path: PathBuf::from("hello.rs"),
            kind: ChangeKind::Created,
        }];

        let events = std::sync::Mutex::new(Vec::new());
        manager
            .apply_changes_with_progress(repo_dir, &changes, |ev| {
                events.lock().unwrap().push(ev);
            })
            .unwrap();

        let events = events.into_inner().unwrap();
        // Must contain at least a BuildingSegment event.
        assert!(
            events.iter().any(|e| matches!(e, ReindexProgress::BuildingSegment { .. })),
            "expected BuildingSegment event, got: {events:?}"
        );
    }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_apply_changes_with_progress_reports_events`
Expected: FAIL — method doesn't exist.

**Step 3: Implement `apply_changes_with_progress`**

Add after the existing `apply_changes` method in `segment_manager.rs`. This is a near-copy of `apply_changes` but with a progress callback. The key additions:
- Calls `on_progress(PreparingFiles { current, total })` while reading files
- Calls `on_progress(BuildingSegment { segment_id, files_done, files_total })` per file via `build_with_progress`
- Calls `on_progress(Tombstoning { count })` when writing tombstones

```rust
    /// Like [`apply_changes`](Self::apply_changes) but emits structured
    /// [`ReindexProgress`](crate::reindex_progress::ReindexProgress) events.
    pub fn apply_changes_with_progress<F: Fn(crate::reindex_progress::ReindexProgress) + Send + Sync>(
        &self,
        repo_dir: &Path,
        changes: &[ChangeEvent],
        on_progress: F,
    ) -> Result<(), IndexError> {
        use crate::reindex_progress::ReindexProgress;

        if changes.is_empty() {
            return Ok(());
        }

        tracing::info!(change_count = changes.len(), "applying changes");
        let start = std::time::Instant::now();

        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let current_segments: Vec<Arc<Segment>> = self.state.snapshot().as_ref().clone();

        // Track tombstones to write per segment index
        let mut tombstone_updates: std::collections::HashMap<usize, TombstoneSet> =
            std::collections::HashMap::new();

        // Collect files that need new entries
        let mut new_files: Vec<InputFile> = Vec::new();
        let total_changes = changes.len();

        for (i, change) in changes.iter().enumerate() {
            let path_str = change.path.to_string_lossy().to_string();

            // Tombstone old entries if needed
            if tombstone::needs_tombstone(&change.kind) {
                let locations = Self::find_file_in_segments(&current_segments, &path_str);
                for (seg_idx, file_id) in locations {
                    tombstone_updates
                        .entry(seg_idx)
                        .or_default()
                        .insert(file_id);
                }
            }

            // Read new content if needed
            if tombstone::needs_new_entry(&change.kind) {
                let has_dotdot = change
                    .path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir);
                if has_dotdot || change.path.is_absolute() {
                    tracing::warn!(
                        path = %change.path.display(),
                        "skipping change with potentially unsafe path (contains '..' or is absolute)"
                    );
                    continue;
                }

                let full_path = repo_dir.join(&change.path);
                if full_path.exists() {
                    let content = fs::read(&full_path)?;

                    if !crate::binary::should_index_file(&full_path, &content, 1_048_576) {
                        continue;
                    }

                    let mtime = full_path
                        .metadata()
                        .and_then(|m| m.modified())
                        .map(|t| {
                            t.duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs()
                        })
                        .unwrap_or(0);
                    new_files.push(InputFile {
                        path: path_str,
                        content,
                        mtime,
                    });
                }
            }

            on_progress(ReindexProgress::PreparingFiles {
                current: i + 1,
                total: total_changes,
            });
        }

        // Build new segment BEFORE writing tombstones
        let mut updated_segments = current_segments.clone();
        let new_file_count = new_files.len();
        if !new_files.is_empty() {
            let seg_id = self.next_segment_id()?;
            tracing::debug!(
                segment_id = seg_id.0,
                new_file_count,
                "building replacement segment"
            );
            let writer = SegmentWriter::new(&self.segments_dir, seg_id);
            let files_total = new_files.len();
            let files_done = std::sync::atomic::AtomicUsize::new(0);
            let segment = writer.build_with_progress(new_files, || {
                let done = files_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                on_progress(ReindexProgress::BuildingSegment {
                    segment_id: seg_id.0,
                    files_done: done,
                    files_total,
                });
            })?;
            updated_segments.push(Arc::new(segment));
        }

        // Write tombstones
        let tombstone_count: u32 = tombstone_updates.values().map(|ts| ts.len()).sum();
        if tombstone_count > 0 {
            on_progress(ReindexProgress::Tombstoning {
                count: tombstone_count,
            });
        }
        for (seg_idx, new_tombstones) in &tombstone_updates {
            let segment = &current_segments[*seg_idx];
            let mut existing = segment.load_tombstones()?;
            existing.merge(new_tombstones);
            existing.write_to(&segment.dir_path().join("tombstones.bin"))?;
            segment.set_cached_tombstones(existing);
        }

        self.state.publish(updated_segments);

        tracing::info!(
            change_count = changes.len(),
            tombstone_count,
            new_file_count,
            segments_affected = tombstone_updates.len(),
            elapsed_ms = start.elapsed().as_millis() as u64,
            "changes applied"
        );
        Ok(())
    }
```

**Step 4: Run tests**

Run: `cargo test -p ferret-indexer-core -- test_apply_changes_with_progress`
Expected: PASS

Run: `cargo test -p ferret-indexer-core -- test_apply_changes`
Expected: PASS (existing tests unchanged)

**Step 5: Commit**

```
feat(core): add apply_changes_with_progress to SegmentManager
```

---

### Task 3: Refactor `run_catchup_with_progress` to emit `ReindexProgress`

**Files:**
- Modify: `ferret-indexer-core/src/catchup.rs:38-98`
- Modify: `ferret-indexer-core/src/lib.rs:40` (update re-export)

**Step 1: Update the test first**

Replace `test_catchup_with_progress_reports_phases` in `catchup.rs` with:

```rust
    #[test]
    fn test_catchup_with_progress_reports_structured_events() {
        use crate::reindex_progress::ReindexProgress;

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let ferret_dir = repo.join(".ferret_index");
        fs::create_dir_all(ferret_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&ferret_dir).unwrap());

        // Write checkpoint at current HEAD.
        let git = GitChangeDetector::new(repo.to_path_buf());
        let head = git.get_head_sha().unwrap();
        let cp = Checkpoint::new(Some(head), 0);
        write_checkpoint(&ferret_dir, &cp).unwrap();

        // Create an untracked file so there's something to detect.
        fs::write(repo.join("progress.rs"), "fn progress() { let x = 1; }").unwrap();

        let events = std::sync::Mutex::new(Vec::new());
        let changes = run_catchup_with_progress(repo, &ferret_dir, &manager, |ev| {
            events.lock().unwrap().push(ev);
        })
        .unwrap();

        let events = events.into_inner().unwrap();
        assert!(!changes.is_empty());
        assert!(
            events.iter().any(|e| matches!(e, ReindexProgress::DetectingChanges)),
            "expected DetectingChanges, got: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, ReindexProgress::ChangesDetected { .. })),
            "expected ChangesDetected, got: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, ReindexProgress::Complete { .. })),
            "expected Complete, got: {events:?}"
        );
    }
```

**Step 2: Update `test_catchup_with_progress_no_changes`**

```rust
    #[test]
    fn test_catchup_with_progress_no_changes_structured() {
        use crate::reindex_progress::ReindexProgress;

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let ferret_dir = repo.join(".ferret_index");
        fs::create_dir_all(ferret_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&ferret_dir).unwrap());

        let git = GitChangeDetector::new(repo.to_path_buf());
        let head = git.get_head_sha().unwrap();
        let cp = Checkpoint::new(Some(head), 0);
        write_checkpoint(&ferret_dir, &cp).unwrap();

        let events = std::sync::Mutex::new(Vec::new());
        let changes = run_catchup_with_progress(repo, &ferret_dir, &manager, |ev| {
            events.lock().unwrap().push(ev);
        })
        .unwrap();

        let events = events.into_inner().unwrap();
        assert!(changes.is_empty());
        assert!(
            events.iter().any(|e| matches!(e, ReindexProgress::NoChanges)),
            "expected NoChanges, got: {events:?}"
        );
    }
```

**Step 3: Run tests to verify they fail**

Run: `cargo test -p ferret-indexer-core -- test_catchup_with_progress`
Expected: FAIL — signature changed.

**Step 4: Rewrite `run_catchup_with_progress`**

Change the callback from `FnMut(&str)` to `FnMut(ReindexProgress)`:

```rust
/// Like [`run_catchup`], but calls `on_progress` with a structured
/// [`ReindexProgress`] event at each phase so callers can stream status to a UI.
pub fn run_catchup_with_progress<F: FnMut(ReindexProgress) + Send + Sync>(
    repo_root: &Path,
    ferret_dir: &Path,
    manager: &Arc<SegmentManager>,
    mut on_progress: F,
) -> Result<Vec<ChangeEvent>> {
    use crate::reindex_progress::ReindexProgress;

    let checkpoint = read_checkpoint(ferret_dir)?;

    on_progress(ReindexProgress::DetectingChanges);

    // Try git fast path.
    let changes = match try_git_catchup(repo_root, &checkpoint) {
        Some(Ok(events)) => {
            tracing::info!(event_count = events.len(), "catch-up via git diff");
            events
        }
        Some(Err(e)) => {
            tracing::warn!(error = %e, "git catch-up failed, falling back to hash diff");
            on_progress(ReindexProgress::ScanningFallback);
            run_hash_fallback(repo_root, manager)?
        }
        None => {
            tracing::info!("no git checkpoint, using hash diff fallback");
            on_progress(ReindexProgress::ScanningFallback);
            run_hash_fallback(repo_root, manager)?
        }
    };

    if changes.is_empty() {
        on_progress(ReindexProgress::NoChanges);
    } else {
        // Count change types.
        let mut created = 0usize;
        let mut modified = 0usize;
        let mut deleted = 0usize;
        for c in &changes {
            match c.kind {
                ChangeKind::Created => created += 1,
                ChangeKind::Modified => modified += 1,
                ChangeKind::Deleted => deleted += 1,
                ChangeKind::Renamed => modified += 1,
            }
        }
        on_progress(ReindexProgress::ChangesDetected {
            created,
            modified,
            deleted,
        });

        manager.apply_changes_with_progress(repo_root, &changes, &on_progress)?;

        if manager.should_compact() {
            tracing::info!("compaction recommended after catch-up");
            let snap = manager.snapshot();
            on_progress(ReindexProgress::CompactingSegments {
                input_segments: snap.len(),
            });
            drop(manager.compact_background());
        }

        on_progress(ReindexProgress::Complete {
            changes_applied: changes.len(),
        });
    }

    // Write updated checkpoint.
    let git = GitChangeDetector::new(repo_root.to_path_buf());
    let git_commit = git.get_head_sha().ok();
    let snapshot = manager.snapshot();
    let file_count: u64 = snapshot.iter().map(|s| s.entry_count() as u64).sum();
    let new_checkpoint = Checkpoint::new(git_commit, file_count);
    write_checkpoint(ferret_dir, &new_checkpoint)?;

    Ok(changes)
}
```

Also update `run_catchup` to adapt the old no-op:

```rust
pub fn run_catchup(
    repo_root: &Path,
    ferret_dir: &Path,
    manager: &Arc<SegmentManager>,
) -> Result<Vec<ChangeEvent>> {
    run_catchup_with_progress(repo_root, ferret_dir, manager, |_| {})
}
```

Update `lib.rs` re-export to include `ReindexProgress`:

```rust
pub use catchup::{run_catchup, run_catchup_with_progress};
```
(This line is unchanged — `ReindexProgress` is already re-exported from `reindex_progress` module.)

**Step 5: Run tests**

Run: `cargo test -p ferret-indexer-core -- test_catchup`
Expected: ALL PASS

**Step 6: Commit**

```
feat(core): emit structured ReindexProgress from run_catchup_with_progress
```

---

### Task 4: Update daemon to serialize `ReindexProgress` as JSON

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs:990-1047` (reindex handler)

**Step 1: Update the reindex handler**

Replace the `DaemonRequest::Reindex` handler in `daemon.rs`. The change: the progress callback now receives `ReindexProgress` instead of `&str`, and we serialize it to JSON:

```rust
DaemonRequest::Reindex => {
    let start = Instant::now();
    let repo = repo_root.to_path_buf();
    let idir = ferret_dir.to_path_buf();
    let mgr = manager.clone();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let handle = tokio::task::spawn_blocking(move || {
        ferret_indexer_core::run_catchup_with_progress(&repo, &idir, &mgr, |event| {
            if let Ok(json) = serde_json::to_string(&event) {
                let _ = tx.send(json);
            }
        })
    });

    // Stream progress messages to client.
    while let Some(msg) = rx.recv().await {
        wire::write_response(&mut writer, &DaemonResponse::Progress { message: msg })
            .await
            .map_err(IndexError::Io)?;
    }

    // Task finished (tx dropped). Get the result.
    match handle.await {
        Ok(Ok(changes)) => {
            let elapsed = start.elapsed();
            wire::write_response(
                &mut writer,
                &DaemonResponse::Done {
                    total: changes.len(),
                    duration_ms: elapsed.as_millis() as u64,
                    stale: false,
                },
            )
            .await
            .map_err(IndexError::Io)?;
        }
        Ok(Err(e)) => {
            wire::write_response(
                &mut writer,
                &DaemonResponse::Error {
                    message: e.to_string(),
                },
            )
            .await
            .map_err(IndexError::Io)?;
        }
        Err(e) => {
            wire::write_response(
                &mut writer,
                &DaemonResponse::Error {
                    message: format!("reindex task panicked: {e}"),
                },
            )
            .await
            .map_err(IndexError::Io)?;
        }
    }
}
```

**Step 2: Run `cargo check --workspace`**

Expected: PASS

**Step 3: Commit**

```
feat(daemon): serialize ReindexProgress as JSON in progress frames
```

---

### Task 5: Add `indicatif` dependency and reindex progress renderer to CLI

**Files:**
- Modify: `ferret-indexer-cli/Cargo.toml` (add indicatif)
- Create: `ferret-indexer-cli/src/reindex_display.rs`
- Modify: `ferret-indexer-cli/src/main.rs:225-238` (replace run_via_daemon with custom handler)

**Step 1: Add `indicatif` to `ferret-indexer-cli/Cargo.toml`**

Add to `[dependencies]`:
```toml
indicatif = "0.17"
```

**Step 2: Create `reindex_display.rs`**

Create `ferret-indexer-cli/src/reindex_display.rs`:

```rust
//! Renders structured [`ReindexProgress`] events as indicatif progress bars.

use std::io::BufReader;

use indicatif::{ProgressBar, ProgressStyle};
use ferret_indexer_core::ReindexProgress;
use ferret_indexer_daemon::types::DaemonResponse;
use ferret_indexer_daemon::wire;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedWriteHalf;

use crate::daemon::{ensure_daemon, ExitCode};
use ferret_indexer_core::IndexError;
use ferret_indexer_daemon::types::DaemonRequest;

/// Run `ferret reindex` with indicatif progress bars.
pub async fn run_reindex_with_progress(
    repo_root: &std::path::Path,
) -> Result<ExitCode, IndexError> {
    let stream = ensure_daemon(repo_root).await?;
    let (reader, mut sock_writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send Reindex request.
    let json = serde_json::to_string(&DaemonRequest::Reindex)
        .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    sock_writer
        .write_all(format!("{json}\n").as_bytes())
        .await
        .map_err(IndexError::Io)?;

    let mut renderer = ProgressRenderer::new();

    // Read TLV responses.
    loop {
        let resp = match wire::read_response(&mut reader).await {
            Ok(resp) => resp,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                renderer.finish();
                return Err(IndexError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "daemon disconnected without sending Done",
                )));
            }
            Err(e) => {
                renderer.finish();
                return Err(IndexError::Io(e));
            }
        };

        match resp {
            DaemonResponse::Progress { message } => {
                match serde_json::from_str::<ReindexProgress>(&message) {
                    Ok(event) => renderer.handle(event),
                    Err(_) => eprintln!("{message}"),
                }
            }
            DaemonResponse::Done {
                total, duration_ms, ..
            } => {
                renderer.finish();
                let secs = duration_ms as f64 / 1000.0;
                if total > 0 {
                    eprintln!("Reindex complete: {total} changes applied in {secs:.1}s");
                } else {
                    eprintln!("No changes detected.");
                }
                return Ok(ExitCode::Success);
            }
            DaemonResponse::Error { message } => {
                renderer.finish();
                return Err(IndexError::Io(std::io::Error::other(message)));
            }
            _ => {}
        }
    }
}

/// Manages indicatif progress bars for reindex phases.
struct ProgressRenderer {
    spinner: Option<ProgressBar>,
    bar: Option<ProgressBar>,
}

impl ProgressRenderer {
    fn new() -> Self {
        Self {
            spinner: None,
            bar: None,
        }
    }

    fn handle(&mut self, event: ReindexProgress) {
        match event {
            ReindexProgress::DetectingChanges => {
                self.clear();
                let sp = ProgressBar::new_spinner();
                sp.set_style(
                    ProgressStyle::with_template("{spinner:.cyan} {msg}")
                        .unwrap()
                        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
                );
                sp.set_message("Detecting changes...");
                sp.enable_steady_tick(std::time::Duration::from_millis(80));
                self.spinner = Some(sp);
            }
            ReindexProgress::ScanningFallback => {
                if let Some(sp) = &self.spinner {
                    sp.set_message("Scanning files (hash fallback)...");
                }
            }
            ReindexProgress::ChangesDetected {
                created,
                modified,
                deleted,
            } => {
                let total = created + modified + deleted;
                let mut parts = Vec::new();
                if modified > 0 {
                    parts.push(format!("{modified} modified"));
                }
                if created > 0 {
                    parts.push(format!("{created} created"));
                }
                if deleted > 0 {
                    parts.push(format!("{deleted} deleted"));
                }
                let detail = parts.join(", ");

                if let Some(sp) = self.spinner.take() {
                    sp.finish_and_clear();
                }
                eprintln!("Found {total} changes ({detail})");

                // Set up the progress bar for indexing phase.
                let bar = ProgressBar::new(total as u64);
                bar.set_style(
                    ProgressStyle::with_template(
                        "Indexing  [{bar:30.green/dim}] {pos}/{len} files  {msg}",
                    )
                    .unwrap()
                    .progress_chars("█▓▒░"),
                );
                self.bar = Some(bar);
            }
            ReindexProgress::NoChanges => {
                if let Some(sp) = self.spinner.take() {
                    sp.finish_and_clear();
                }
            }
            ReindexProgress::PreparingFiles { current, total } => {
                if let Some(bar) = &self.bar {
                    bar.set_length(total as u64);
                    bar.set_position(current as u64);
                    bar.set_message("preparing...");
                }
            }
            ReindexProgress::BuildingSegment {
                segment_id,
                files_done,
                files_total: _,
            } => {
                if let Some(bar) = &self.bar {
                    bar.set_position(bar.position().max(files_done as u64));
                    bar.set_message(format!("seg_{segment_id:04}"));
                }
            }
            ReindexProgress::Tombstoning { count } => {
                if let Some(bar) = &self.bar {
                    bar.set_message(format!("tombstoning {count} entries"));
                }
            }
            ReindexProgress::CompactingSegments { input_segments } => {
                if let Some(bar) = self.bar.take() {
                    bar.finish_and_clear();
                }
                let sp = ProgressBar::new_spinner();
                sp.set_style(
                    ProgressStyle::with_template("{spinner:.cyan} {msg}")
                        .unwrap()
                        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
                );
                sp.set_message(format!("Compacting {input_segments} segments..."));
                sp.enable_steady_tick(std::time::Duration::from_millis(80));
                self.spinner = Some(sp);
            }
            ReindexProgress::Complete { .. } => {
                self.finish();
            }
        }
    }

    fn clear(&mut self) {
        if let Some(sp) = self.spinner.take() {
            sp.finish_and_clear();
        }
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
    }

    fn finish(&mut self) {
        if let Some(sp) = self.spinner.take() {
            sp.finish_and_clear();
        }
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
    }
}
```

**Step 3: Wire up `reindex_display` in `main.rs`**

Add `mod reindex_display;` near the top of `main.rs` with the other module declarations.

Replace the `Command::Reindex` match arm in `main.rs`:

```rust
        Command::Reindex { full } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            if full {
                // Full rebuild — same as init --force.
                init::run_init(&repo_root, true)?;
            } else {
                reindex_display::run_reindex_with_progress(&repo_root).await?;
            }
            Ok(ExitCode::Success)
        }
```

**Step 4: Run `cargo check --workspace`**

Expected: PASS

**Step 5: Commit**

```
feat(cli): add indicatif progress bars to reindex command
```

---

### Task 6: Final integration test and cleanup

**Step 1: Run full workspace checks**

Run: `cargo clippy --workspace -- -D warnings`
Expected: PASS

Run: `cargo fmt --all -- --check`
Expected: PASS

Run: `cargo test --workspace`
Expected: ALL PASS

**Step 2: Fix any issues found**

Address clippy/fmt/test failures if any.

**Step 3: Manual smoke test**

If a repo is available:
```bash
cargo run -p ferret-indexer-cli -- reindex
```

Expected output (for a repo with changes):
```
Found 12 changes (8 modified, 3 created, 1 deleted)
Indexing  [██████████████████████████████] 12/12 files  seg_0005
Reindex complete: 12 changes applied in 0.3s
```

Expected output (for a repo with no changes):
```
No changes detected.
```

**Step 4: Commit**

```
chore: clippy and fmt fixes for reindex progress bars
```

---

### Task 7: Rewrite `init.rs` to use indicatif progress bars

**Files:**
- Modify: `ferret-indexer-cli/src/init.rs` (replace `ProgressLine` with indicatif)

This task replaces the custom `ProgressLine` struct with `indicatif` progress bars while preserving **all** existing functionality:
- Force mode (remove existing index)
- Existing index check (error if not `--force`)
- Phase 1: Walk file tree with spinner showing file count
- Phase 2: Filter files in parallel with progress bar showing percentage; skip breakdown (binary, too large, filtered, read errors)
- Phase 3: Build index with progress bar showing files done / total with percentage
- Phase 4: Write checkpoint
- Summary line with file count, content bytes, total time
- Phase 5: Auto-register in repo registry

**Step 1: Rewrite `init.rs`**

Replace the full contents of `ferret-indexer-cli/src/init.rs`:

```rust
use std::path::Path;
use std::time::Instant;

use indicatif::{ProgressBar, ProgressStyle};
use ferret_indexer_core::checkpoint::{Checkpoint, read_checkpoint, write_checkpoint};
use ferret_indexer_core::error::IndexError;
use ferret_indexer_core::git_diff::GitChangeDetector;
use ferret_indexer_core::registry::{add_repo, config_file_path, load_config, save_config};
use ferret_indexer_core::segment::InputFile;
use ferret_indexer_core::walker::DirectoryWalkerBuilder;
use ferret_indexer_core::{DEFAULT_MAX_FILE_SIZE, SegmentManager, should_index_file};

/// Format a number with comma separators (e.g. 1234567 -> "1,234,567").
fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Format bytes in human-readable form (e.g. 1048576 -> "1.0 MB").
fn fmt_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
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

/// Create a spinner with the standard style.
fn new_spinner(msg: &str) -> ProgressBar {
    let sp = ProgressBar::new_spinner();
    sp.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    sp.set_message(msg.to_string());
    sp.enable_steady_tick(std::time::Duration::from_millis(80));
    sp
}

/// Run the `ferret init` command.
///
/// Walks the repo tree, builds the full index, and writes a checkpoint.
/// If `force` is false and an index already exists, returns an error.
pub fn run_init(repo_root: &Path, force: bool) -> Result<(), IndexError> {
    let ferret_dir = repo_root.join(".ferret_index");

    // Check for existing index unless --force.
    if !force {
        match read_checkpoint(&ferret_dir) {
            Ok(Some(_)) => {
                return Err(IndexError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "index already exists. Use --force to rebuild.",
                )));
            }
            Err(e) => return Err(e),
            Ok(None) => {} // No checkpoint — proceed with init.
        }
    }

    // If forcing, remove existing segments and stale checkpoint.
    if force {
        let segments_dir = ferret_dir.join("segments");
        if segments_dir.exists() {
            eprintln!("Removing existing index...");
            std::fs::remove_dir_all(&segments_dir)?;
        }
        let checkpoint_path = ferret_dir.join("checkpoint.json");
        if checkpoint_path.exists() {
            std::fs::remove_file(&checkpoint_path)?;
        }
    }

    let start = Instant::now();

    // ── Phase 1: Walk the file tree ──────────────────────────────────
    let walk_start = Instant::now();
    let spinner = new_spinner("Walking file tree...");

    let walker = DirectoryWalkerBuilder::new(repo_root).build();
    let sp_ref = &spinner;
    let walked = walker.run_parallel_with_progress(|count| {
        if count % 100 == 0 {
            sp_ref.set_message(format!(
                "Walking file tree... {} files found",
                fmt_count(count)
            ));
        }
    })?;

    let walk_elapsed = walk_start.elapsed();
    spinner.finish_and_clear();
    eprintln!(
        "Walking file tree... {} files found ({:.1}s)",
        fmt_count(walked.len()),
        walk_elapsed.as_secs_f64()
    );

    // ── Phase 2: Filter and load file contents ───────────────────────
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    let filter_start = Instant::now();
    let total_walked = walked.len();
    let skipped_size = AtomicUsize::new(0);
    let skipped_binary = AtomicUsize::new(0);
    let skipped_content = AtomicUsize::new(0);
    let skipped_read_err = AtomicUsize::new(0);
    let total_content_bytes = AtomicU64::new(0);
    let filter_done = AtomicUsize::new(0);

    let bar = ProgressBar::new(total_walked as u64);
    bar.set_style(
        ProgressStyle::with_template(
            "Filtering [{bar:30.green/dim}] {pos}/{len} files  {msg}",
        )
        .unwrap()
        .progress_chars("█▓▒░"),
    );

    let bar_ref = &bar;
    let files: Vec<InputFile> = walked
        .par_iter()
        .filter_map(|wf| {
            let current = filter_done.fetch_add(1, Ordering::Relaxed) + 1;
            if current % 100 == 0 || current == total_walked {
                bar_ref.set_position(current as u64);
            }

            // Pre-filter by size and extension before reading content.
            if wf.metadata.len() > DEFAULT_MAX_FILE_SIZE {
                skipped_size.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            if ferret_indexer_core::is_binary_path(&wf.path) {
                skipped_binary.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            let content = match std::fs::read(&wf.path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(path = %wf.path.display(), error = %e, "skipping file: read error");
                    skipped_read_err.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            };
            if !should_index_file(&wf.path, &content, DEFAULT_MAX_FILE_SIZE) {
                skipped_content.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            let rel_path = wf
                .path
                .strip_prefix(repo_root)
                .unwrap_or(&wf.path)
                .to_string_lossy()
                .to_string();
            let mtime = wf
                .metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            total_content_bytes.fetch_add(content.len() as u64, Ordering::Relaxed);
            Some(InputFile {
                path: rel_path,
                content,
                mtime,
            })
        })
        .collect();

    let skipped_size = skipped_size.load(Ordering::Relaxed);
    let skipped_binary = skipped_binary.load(Ordering::Relaxed);
    let skipped_content = skipped_content.load(Ordering::Relaxed);
    let skipped_read_err = skipped_read_err.load(Ordering::Relaxed);
    let total_content_bytes = total_content_bytes.load(Ordering::Relaxed);

    let filter_elapsed = filter_start.elapsed();
    let total_skipped = skipped_size + skipped_binary + skipped_content + skipped_read_err;
    bar.finish_and_clear();
    eprintln!(
        "Filtering files... {} indexable, {} skipped ({:.1}s)",
        fmt_count(files.len()),
        fmt_count(total_skipped),
        filter_elapsed.as_secs_f64()
    );

    // Print skip breakdown if anything was skipped.
    if total_skipped > 0 {
        let mut reasons = Vec::new();
        if skipped_binary > 0 {
            reasons.push(format!("{} binary", fmt_count(skipped_binary)));
        }
        if skipped_size > 0 {
            reasons.push(format!("{} too large", fmt_count(skipped_size)));
        }
        if skipped_content > 0 {
            reasons.push(format!("{} filtered", fmt_count(skipped_content)));
        }
        if skipped_read_err > 0 {
            reasons.push(format!("{} read errors", fmt_count(skipped_read_err)));
        }
        eprintln!("  Skipped: {}", reasons.join(", "));
    }

    let file_count = files.len() as u64;

    if file_count == 0 {
        eprintln!("No indexable files found.");
        return Ok(());
    }

    // ── Phase 3: Build the index ─────────────────────────────────────
    let index_start = Instant::now();
    let total_files = files.len();

    let bar = ProgressBar::new(total_files as u64);
    bar.set_style(
        ProgressStyle::with_template(
            "Indexing  [{bar:30.green/dim}] {pos}/{len} files  ({msg})",
        )
        .unwrap()
        .progress_chars("█▓▒░"),
    );
    bar.set_message(fmt_bytes(total_content_bytes));

    let manager = SegmentManager::new(&ferret_dir)?;
    let bar_ref = &bar;
    manager.index_files_with_progress(files, |done, _total| {
        if done % 100 == 0 || done == total_files {
            bar_ref.set_position(done as u64);
        }
    })?;

    let index_elapsed = index_start.elapsed();
    bar.finish_and_clear();
    eprintln!(
        "Building index... {}/{} (100%) ({:.1}s)",
        fmt_count(total_files),
        fmt_count(total_files),
        index_elapsed.as_secs_f64()
    );

    // ── Phase 4: Write checkpoint ────────────────────────────────────
    eprintln!("Writing checkpoint...");
    let git = GitChangeDetector::new(repo_root.to_path_buf());
    let git_commit = git.get_head_sha().ok();
    let checkpoint = Checkpoint::new(git_commit, file_count);
    write_checkpoint(&ferret_dir, &checkpoint)?;

    // ── Summary ──────────────────────────────────────────────────────
    let elapsed = start.elapsed();
    eprintln!(
        "Done. Indexed {} files ({}) in {:.1}s.",
        fmt_count(total_files),
        fmt_bytes(total_content_bytes),
        elapsed.as_secs_f64()
    );

    // ── Phase 5: Auto-register in repo registry ──────────────────────

    let derived_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    match load_config() {
        Ok(mut config) => {
            if add_repo(&mut config, repo_root.to_path_buf(), None) {
                if let Err(e) = save_config(&config) {
                    eprintln!("Warning: could not save registry: {e}");
                } else {
                    eprintln!(
                        "Registered repo \"{derived_name}\" in {}",
                        config_file_path().display()
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("Warning: could not load registry: {e}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_count() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(1), "1");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1,000");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }

    #[test]
    fn test_fmt_bytes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1_048_576), "1.0 MB");
        assert_eq!(fmt_bytes(1_073_741_824), "1.0 GB");
    }
}
```

**Functionality preserved checklist:**
- [x] `--force` check and existing index removal
- [x] Phase 1: Walk file tree with live count
- [x] Phase 2: Filter files in parallel with skip tracking (size, binary, content, read errors)
- [x] Skip breakdown printed after filtering
- [x] Phase 3: Build index with progress (`index_files_with_progress`)
- [x] Phase 4: Write checkpoint with git SHA
- [x] Summary line with file count + content bytes + total time
- [x] Phase 5: Auto-register in repo registry
- [x] `fmt_count` and `fmt_bytes` helpers preserved
- [x] Unit tests for `fmt_count` and `fmt_bytes` preserved

**Step 2: Run `cargo check --workspace`**

Expected: PASS

**Step 3: Run existing tests**

Run: `cargo test -p ferret-indexer-cli -- test_fmt_count`
Expected: PASS

Run: `cargo test -p ferret-indexer-cli -- test_fmt_bytes`
Expected: PASS

**Step 4: Run full workspace tests**

Run: `cargo test --workspace`
Expected: ALL PASS

**Step 5: Run clippy and fmt**

Run: `cargo clippy --workspace -- -D warnings`
Expected: PASS

Run: `cargo fmt --all -- --check`
Expected: PASS

**Step 6: Commit**

```
feat(cli): rewrite init command to use indicatif progress bars
```
