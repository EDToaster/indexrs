# Hybrid Change Detector Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Combine the file watcher (sub-second latency) and git-based change detection (periodic timer) into a single unified change stream, deduped by path.

**Architecture:** `HybridDetector` owns a `FileWatcher` and `GitChangeDetector`. On `start()`, it spawns a background thread that runs the git diff loop on a configurable interval and forwards watcher events. Both sources feed into a single `mpsc::Sender<Vec<ChangeEvent>>`. Dedup happens per batch (git diff results) and across sources (HashMap keyed by path, latest kind wins). On watcher error, an immediate git diff scan is triggered. A `reindex()` method signals the background thread to run an immediate git diff.

**Tech Stack:** Rust std (`mpsc`, `thread`, `sync`), existing `FileWatcher`, `GitChangeDetector`, `ChangeEvent`/`ChangeKind` types.

---

### Task 1: Create `hybrid_detector.rs` with struct definition and `new()`

**Files:**
- Create: `ferret-indexer-core/src/hybrid_detector.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

**Step 1: Write the failing test**

In `ferret-indexer-core/src/hybrid_detector.rs`, create the module with a test:

```rust
//! Hybrid change detector combining file watcher and git-based detection.
//!
//! [`HybridDetector`] merges two change-detection sources into one unified
//! stream of [`ChangeEvent`] batches:
//!
//! - **File watcher** — sub-second latency via filesystem notifications
//! - **Git diff** — periodic scan (default 30 s) and on-demand via [`reindex`](HybridDetector::reindex)
//!
//! Events from both sources are de-duplicated by path (latest [`ChangeKind`]
//! wins) before being sent to the consumer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::changes::{ChangeEvent, ChangeKind};
use crate::error::{IndexError, Result};
use crate::git_diff::GitChangeDetector;
use crate::watcher::FileWatcher;

/// Default interval between periodic git diff scans.
const DEFAULT_GIT_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Hybrid change detector that unions file watcher and git diff events.
///
/// Call [`start`](Self::start) to begin watching; events arrive as
/// `Vec<ChangeEvent>` batches on the returned channel.  Call
/// [`reindex`](Self::reindex) to force an immediate git diff scan.
pub struct HybridDetector {
    root: PathBuf,
    watcher: FileWatcher,
    git_detector: GitChangeDetector,
    git_poll_interval: Duration,
    running: Arc<AtomicBool>,
    reindex_flag: Arc<AtomicBool>,
    bg_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_new_creates_successfully() {
        let tmp = TempDir::new().unwrap();
        let detector = HybridDetector::new(tmp.path().to_path_buf());
        assert!(detector.is_ok());
    }
}
```

**Step 2: Write `new()` implementation**

```rust
impl HybridDetector {
    /// Create a new hybrid detector rooted at the given path.
    ///
    /// Does **not** start watching — call [`start`](Self::start) to begin.
    pub fn new(root: PathBuf) -> Result<Self> {
        let watcher = FileWatcher::new(root.clone())?;
        let git_detector = GitChangeDetector::new(root.clone());

        Ok(Self {
            root,
            watcher,
            git_detector,
            git_poll_interval: DEFAULT_GIT_POLL_INTERVAL,
            running: Arc::new(AtomicBool::new(false)),
            reindex_flag: Arc::new(AtomicBool::new(false)),
            bg_thread: None,
        })
    }
}
```

**Step 3: Wire into `lib.rs`**

Add `pub mod hybrid_detector;` and `pub use hybrid_detector::HybridDetector;` to `lib.rs`.

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-indexer-core -- test_new_creates_successfully -v`
Expected: PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/hybrid_detector.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(HHC-39): add HybridDetector struct with new() constructor"
```

---

### Task 2: Implement `set_git_poll_interval()` and `stop()`

**Files:**
- Modify: `ferret-indexer-core/src/hybrid_detector.rs`

**Step 1: Write the failing tests**

```rust
#[test]
fn test_set_git_poll_interval() {
    let tmp = TempDir::new().unwrap();
    let mut detector = HybridDetector::new(tmp.path().to_path_buf()).unwrap();
    detector.set_git_poll_interval(Duration::from_secs(60));
    // No panic means success — interval is stored internally.
}

#[test]
fn test_stop_before_start_is_safe() {
    let tmp = TempDir::new().unwrap();
    let mut detector = HybridDetector::new(tmp.path().to_path_buf()).unwrap();
    detector.stop(); // must not panic
}

#[test]
fn test_double_stop_is_safe() {
    let tmp = TempDir::new().unwrap();
    let mut detector = HybridDetector::new(tmp.path().to_path_buf()).unwrap();
    detector.stop();
    detector.stop(); // must not panic
}
```

**Step 2: Implement the methods**

```rust
/// Configure the interval between periodic git diff scans.
pub fn set_git_poll_interval(&mut self, interval: Duration) {
    self.git_poll_interval = interval;
}

/// Stop both the file watcher and the git diff background thread.
///
/// Safe to call multiple times or before [`start`](Self::start).
pub fn stop(&mut self) {
    self.running.store(false, Ordering::SeqCst);
    self.watcher.stop();
    if let Some(handle) = self.bg_thread.take() {
        let _ = handle.join();
    }
}
```

**Step 3: Run tests**

Run: `cargo test -p ferret-indexer-core -- test_set_git_poll_interval test_stop_before_start test_double_stop -v`
Expected: PASS

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/hybrid_detector.rs
git commit -m "feat(HHC-39): add set_git_poll_interval() and stop() methods"
```

---

### Task 3: Implement dedup helper

**Files:**
- Modify: `ferret-indexer-core/src/hybrid_detector.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_dedup_keeps_latest_kind() {
    let events = vec![
        ChangeEvent { path: PathBuf::from("a.rs"), kind: ChangeKind::Created },
        ChangeEvent { path: PathBuf::from("b.rs"), kind: ChangeKind::Modified },
        ChangeEvent { path: PathBuf::from("a.rs"), kind: ChangeKind::Deleted },
    ];
    let deduped = dedup_events(events);
    assert_eq!(deduped.len(), 2);
    let a_event = deduped.iter().find(|e| e.path == PathBuf::from("a.rs")).unwrap();
    assert_eq!(a_event.kind, ChangeKind::Deleted);
}

#[test]
fn test_dedup_empty_input() {
    let deduped = dedup_events(vec![]);
    assert!(deduped.is_empty());
}

#[test]
fn test_dedup_no_duplicates() {
    let events = vec![
        ChangeEvent { path: PathBuf::from("a.rs"), kind: ChangeKind::Created },
        ChangeEvent { path: PathBuf::from("b.rs"), kind: ChangeKind::Modified },
    ];
    let deduped = dedup_events(events);
    assert_eq!(deduped.len(), 2);
}
```

**Step 2: Implement the dedup function**

```rust
/// De-duplicate events by path, keeping the latest [`ChangeKind`] for each path.
fn dedup_events(events: Vec<ChangeEvent>) -> Vec<ChangeEvent> {
    let mut map: HashMap<PathBuf, ChangeKind> = HashMap::new();
    for event in events {
        map.insert(event.path, event.kind);
    }
    let mut result: Vec<ChangeEvent> = map
        .into_iter()
        .map(|(path, kind)| ChangeEvent { path, kind })
        .collect();
    result.sort_by(|a, b| a.path.cmp(&b.path));
    result
}
```

**Step 3: Run tests**

Run: `cargo test -p ferret-indexer-core -- test_dedup -v`
Expected: PASS

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/hybrid_detector.rs
git commit -m "feat(HHC-39): add dedup_events helper for path-based deduplication"
```

---

### Task 4: Implement `start()` and `reindex()`

**Files:**
- Modify: `ferret-indexer-core/src/hybrid_detector.rs`

**Step 1: Write the tests**

```rust
#[test]
fn test_start_and_stop_lifecycle() {
    let tmp = TempDir::new().unwrap();
    // Initialize a git repo so git commands work.
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let mut detector = HybridDetector::new(tmp.path().to_path_buf()).unwrap();
    let rx = detector.start();
    assert!(rx.is_ok());
    // Give the background thread a moment to start.
    std::thread::sleep(Duration::from_millis(100));
    detector.stop();
}

#[test]
fn test_reindex_triggers_detection() {
    let tmp = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    let mut detector = HybridDetector::new(tmp.path().to_path_buf()).unwrap();
    detector.set_git_poll_interval(Duration::from_secs(3600)); // very long poll
    let _rx = detector.start().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    detector.reindex(); // should set the flag without panic
    std::thread::sleep(Duration::from_millis(100));
    detector.stop();
}

#[test]
#[ignore] // Filesystem-dependent integration test
fn test_start_receives_git_diff_events() {
    let tmp = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    // Create an untracked file so git diff has something to report.
    std::fs::write(tmp.path().join("new_file.rs"), "fn main() {}").unwrap();

    let mut detector = HybridDetector::new(tmp.path().to_path_buf()).unwrap();
    detector.set_git_poll_interval(Duration::from_millis(200));
    let rx = detector.start().unwrap();

    // Wait for at least one git diff cycle.
    let mut all_events = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(batch) => all_events.extend(batch),
            Err(_) => continue,
        }
        if !all_events.is_empty() {
            break;
        }
    }

    assert!(
        all_events.iter().any(|e| e.path.ends_with("new_file.rs")),
        "expected event for new_file.rs, got: {all_events:?}"
    );
    detector.stop();
}
```

**Step 2: Implement `start()` and `reindex()`**

```rust
/// Start both the file watcher and the periodic git diff loop.
///
/// Returns a channel receiver that yields batches of [`ChangeEvent`]s.
/// Events from both sources are de-duplicated by path before sending.
pub fn start(&mut self) -> Result<mpsc::Receiver<Vec<ChangeEvent>>> {
    let (tx, rx) = mpsc::channel();

    // Start the file watcher.
    let watcher_rx = self.watcher.start()?;

    self.running.store(true, Ordering::SeqCst);

    let running = Arc::clone(&self.running);
    let reindex_flag = Arc::clone(&self.reindex_flag);
    let poll_interval = self.git_poll_interval;
    let root = self.root.clone();

    let handle = thread::spawn(move || {
        let git = GitChangeDetector::new(root);
        let poll_step = Duration::from_millis(100);

        // Run an initial git diff on startup.
        if let Ok(events) = git.detect_changes() {
            if !events.is_empty() {
                let _ = tx.send(dedup_events(events));
            }
        }

        let mut elapsed = Duration::ZERO;

        while running.load(Ordering::SeqCst) {
            // Check for watcher events (non-blocking).
            match watcher_rx.try_recv() {
                Ok(events) => {
                    let deduped = dedup_events(events);
                    if !deduped.is_empty() {
                        if tx.send(deduped).is_err() {
                            break;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Watcher channel closed — trigger a git diff scan.
                    if let Ok(events) = git.detect_changes() {
                        if !events.is_empty() {
                            let _ = tx.send(dedup_events(events));
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }

            // Check reindex flag.
            if reindex_flag.swap(false, Ordering::SeqCst) {
                if let Ok(events) = git.detect_changes() {
                    if !events.is_empty() {
                        let _ = tx.send(dedup_events(events));
                    }
                }
                elapsed = Duration::ZERO;
            }

            // Periodic git diff.
            if elapsed >= poll_interval {
                if let Ok(events) = git.detect_changes() {
                    if !events.is_empty() {
                        let _ = tx.send(dedup_events(events));
                    }
                }
                elapsed = Duration::ZERO;
            }

            thread::sleep(poll_step);
            elapsed += poll_step;
        }
    });

    self.bg_thread = Some(handle);
    Ok(rx)
}

/// Trigger an immediate git diff scan.
///
/// The scan runs asynchronously on the background thread; results will
/// appear on the channel returned by [`start`](Self::start).
pub fn reindex(&self) {
    self.reindex_flag.store(true, Ordering::SeqCst);
}
```

**Step 3: Run all tests**

Run: `cargo test -p ferret-indexer-core -- hybrid_detector -v`
Expected: PASS (ignored tests skipped)

**Step 4: Run clippy**

Run: `cargo clippy -p ferret-indexer-core -- -D warnings`
Expected: No warnings

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/hybrid_detector.rs
git commit -m "feat(HHC-39): implement start(), stop(), and reindex() for HybridDetector"
```

---

### Task 5: Final verification and commit

**Step 1: Run full test suite**

Run: `cargo test -p ferret-indexer-core`
Expected: All non-ignored tests PASS

**Step 2: Run clippy**

Run: `cargo clippy -p ferret-indexer-core -- -D warnings`
Expected: No warnings

**Step 3: Run fmt check**

Run: `cargo fmt --all -- --check`
Expected: No formatting issues

**Step 4: Final commit (if any fixups needed)**

```bash
git add -A
git commit -m "feat(HHC-39): hybrid change detector - final cleanup"
```
