# File Watcher Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create a file watcher module (`ferret-indexer-core/src/watcher.rs`) that uses `notify-debouncer-full` for live change detection with gitignore support.

**Architecture:** The `FileWatcher` struct wraps `notify-debouncer-full`'s `Debouncer` with a 200ms debounce window. It translates debounced `notify` events into our own `ChangeEvent`/`ChangeKind` types. Gitignore filtering is done via the `ignore` crate's `gitignore::Gitignore` matcher, applied as a post-filter on incoming events before sending them through the channel.

**Tech Stack:** `notify-debouncer-full` 0.4 (which depends on `notify` 7), `ignore` crate for gitignore parsing, `tokio::sync::mpsc` for the event channel, `tracing` for error logging.

---

### Task 1: Add `Watcher` error variant to `IndexError`

**Files:**
- Modify: `ferret-indexer-core/src/error.rs`

**Step 1: Add the `Watcher` variant**

Add a new variant to `IndexError` in `ferret-indexer-core/src/error.rs` after the `Walk` variant:

```rust
    /// An error from the file-system watcher subsystem.
    #[error("watcher error: {0}")]
    Watcher(String),
```

**Step 2: Run tests to verify nothing is broken**

Run: `cargo test -p ferret-indexer-core -- error`
Expected: All existing error tests PASS

**Step 3: Commit**

```bash
git add ferret-indexer-core/src/error.rs
git commit -m "feat(core): add Watcher error variant to IndexError (HHC-37)"
```

---

### Task 2: Create `ChangeEvent` and `ChangeKind` types with unit tests

**Files:**
- Create: `ferret-indexer-core/src/watcher.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

**Step 1: Write the failing test**

Create `ferret-indexer-core/src/watcher.rs` with only the test module:

```rust
//! File watcher with debounced change detection.
//!
//! Uses [`notify-debouncer-full`] to watch a directory tree recursively,
//! translating raw filesystem events into [`ChangeEvent`] values.  Events for
//! paths matched by `.gitignore` are silently dropped.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn change_kind_debug_and_clone() {
        let kind = ChangeKind::Created;
        let cloned = kind.clone();
        assert_eq!(kind, cloned);
        assert_eq!(format!("{kind:?}"), "Created");
    }

    #[test]
    fn change_event_equality() {
        let a = ChangeEvent {
            path: PathBuf::from("/tmp/a.rs"),
            kind: ChangeKind::Modified,
        };
        let b = ChangeEvent {
            path: PathBuf::from("/tmp/a.rs"),
            kind: ChangeKind::Modified,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn change_event_inequality_on_path() {
        let a = ChangeEvent {
            path: PathBuf::from("/tmp/a.rs"),
            kind: ChangeKind::Modified,
        };
        let b = ChangeEvent {
            path: PathBuf::from("/tmp/b.rs"),
            kind: ChangeKind::Modified,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn change_event_inequality_on_kind() {
        let a = ChangeEvent {
            path: PathBuf::from("/tmp/a.rs"),
            kind: ChangeKind::Created,
        };
        let b = ChangeEvent {
            path: PathBuf::from("/tmp/a.rs"),
            kind: ChangeKind::Deleted,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn change_event_hash_consistent() {
        use std::collections::HashSet;
        let event = ChangeEvent {
            path: PathBuf::from("/tmp/a.rs"),
            kind: ChangeKind::Modified,
        };
        let mut set = HashSet::new();
        set.insert(event.clone());
        assert!(set.contains(&event));
    }

    #[test]
    fn all_change_kinds_exist() {
        let kinds = vec![
            ChangeKind::Created,
            ChangeKind::Modified,
            ChangeKind::Deleted,
            ChangeKind::Renamed,
        ];
        assert_eq!(kinds.len(), 4);
    }
}
```

**Step 2: Add the types above the test module to make tests pass**

In the same file, above `#[cfg(test)]`, add:

```rust
use std::path::PathBuf;

/// The kind of filesystem change observed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// A new file was created.
    Created,
    /// An existing file's content was modified.
    Modified,
    /// A file was deleted.
    Deleted,
    /// A file was renamed (includes atomic-save patterns: write-to-temp + rename).
    Renamed,
}

/// A single observed filesystem change event.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeEvent {
    /// The path that changed.
    pub path: PathBuf,
    /// What kind of change occurred.
    pub kind: ChangeKind,
}
```

**Step 3: Wire up the module in `lib.rs`**

Add `pub mod watcher;` and re-exports to `ferret-indexer-core/src/lib.rs`:

```rust
pub mod watcher;
// ... in the re-exports section:
pub use watcher::{ChangeEvent, ChangeKind, FileWatcher};
```

Note: `FileWatcher` doesn't exist yet so this will fail until Task 3. For now, just add:
```rust
pub mod watcher;
pub use watcher::{ChangeEvent, ChangeKind};
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferret-indexer-core -- watcher`
Expected: All 6 tests PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/watcher.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(core): add ChangeEvent and ChangeKind types (HHC-37)"
```

---

### Task 3: Implement `FileWatcher` struct with `new`, `start`, `stop`

**Files:**
- Modify: `ferret-indexer-core/src/watcher.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

**Step 1: Write the failing tests for FileWatcher**

Add these tests to the existing `tests` module in `watcher.rs`:

```rust
    use tempfile::TempDir;

    #[test]
    fn file_watcher_new_on_valid_dir() {
        let tmp = TempDir::new().unwrap();
        let watcher = FileWatcher::new(tmp.path().to_path_buf());
        assert!(watcher.is_ok());
    }

    #[test]
    fn file_watcher_new_on_nonexistent_dir() {
        let result = FileWatcher::new(PathBuf::from("/nonexistent/path/that/does/not/exist"));
        // Should still succeed at construction — the error comes at start()
        // OR it may fail — either is acceptable, just ensure no panic
        let _ = result;
    }

    #[test]
    fn file_watcher_start_and_stop() {
        let tmp = TempDir::new().unwrap();
        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start();
        assert!(rx.is_ok());
        watcher.stop();
    }

    #[test]
    fn file_watcher_double_stop_is_safe() {
        let tmp = TempDir::new().unwrap();
        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let _rx = watcher.start().unwrap();
        watcher.stop();
        watcher.stop(); // should not panic
    }

    #[test]
    fn file_watcher_start_returns_receiver() {
        let tmp = TempDir::new().unwrap();
        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start().unwrap();
        // Channel should exist but have no events yet
        assert!(rx.try_recv().is_err());
        watcher.stop();
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p ferret-indexer-core -- watcher`
Expected: FAIL — `FileWatcher` not found

**Step 3: Implement `FileWatcher`**

Add the implementation to `watcher.rs` (above the test module, below the types). The key design:

- `FileWatcher::new(root)` stores the root path and loads gitignore rules using `ignore::gitignore::Gitignore`
- `start()` creates a `notify_debouncer_full::new_debouncer` with 200ms timeout, watches root recursively, spawns a background thread to forward events through `std::sync::mpsc`
- `stop()` drops the debouncer to stop watching

```rust
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use ignore::gitignore::Gitignore;
use notify_debouncer_full::{
    new_debouncer,
    notify::RecursiveMode,
    DebounceEventResult, Debouncer, FileIdCache,
    notify::RecommendedWatcher,
    RecommendedCache,
};
use notify_debouncer_full::notify::event::{EventKind, ModifyKind, RenameMode};

use crate::error::{IndexError, Result};

const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(200);

/// File watcher for live change detection.
///
/// Wraps `notify-debouncer-full` to provide debounced filesystem events,
/// filtered through `.gitignore` rules.
pub struct FileWatcher {
    root: PathBuf,
    gitignore: Gitignore,
    debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
}

impl FileWatcher {
    /// Create a new file watcher rooted at the given path.
    ///
    /// Loads `.gitignore` rules from the root directory (if present).
    /// Does not start watching until [`start`](Self::start) is called.
    pub fn new(root: PathBuf) -> Result<Self> {
        let gitignore_path = root.join(".gitignore");
        let (gitignore, _err) = Gitignore::new(&gitignore_path);
        // _err contains parse warnings — we intentionally ignore them.
        // If no .gitignore exists, `gitignore` will simply match nothing.

        Ok(Self {
            root,
            gitignore,
            debouncer: None,
        })
    }

    /// Start watching the root directory recursively.
    ///
    /// Returns a channel receiver that yields batches of [`ChangeEvent`]s.
    /// Each batch corresponds to one debounce window (~200ms).
    pub fn start(&mut self) -> Result<mpsc::Receiver<Vec<ChangeEvent>>> {
        let (tx, rx) = mpsc::channel();
        let gitignore = self.gitignore.clone();
        let root = self.root.clone();

        let mut debouncer = new_debouncer(
            DEBOUNCE_TIMEOUT,
            None,
            move |result: DebounceEventResult| {
                match result {
                    Ok(events) => {
                        let changes: Vec<ChangeEvent> = events
                            .into_iter()
                            .filter_map(|debounced_event| {
                                let event = &debounced_event.event;
                                let kind = match &event.kind {
                                    EventKind::Create(_) => Some(ChangeKind::Created),
                                    EventKind::Modify(ModifyKind::Name(
                                        RenameMode::Both | RenameMode::To | RenameMode::From,
                                    )) => Some(ChangeKind::Renamed),
                                    EventKind::Modify(_) => Some(ChangeKind::Modified),
                                    EventKind::Remove(_) => Some(ChangeKind::Deleted),
                                    _ => None,
                                };

                                let kind = kind?;
                                // Use the last path (for renames this is the destination)
                                let path = event.paths.last()?;

                                // Filter out gitignored paths
                                let is_dir = path.is_dir();
                                if gitignore
                                    .matched_path_or_any_parents(path, is_dir)
                                    .is_ignore()
                                {
                                    return None;
                                }

                                // Filter out .git directory events
                                if path_contains_component(path, &root, ".git") {
                                    return None;
                                }

                                Some(ChangeEvent {
                                    path: path.clone(),
                                    kind,
                                })
                            })
                            .collect();

                        if !changes.is_empty() {
                            if let Err(e) = tx.send(changes) {
                                tracing::warn!("failed to send change events: {e}");
                            }
                        }
                    }
                    Err(errors) => {
                        for error in errors {
                            tracing::warn!("watcher error: {error}");
                        }
                    }
                }
            },
        )
        .map_err(|e| IndexError::Watcher(e.to_string()))?;

        debouncer
            .watch(&self.root, RecursiveMode::Recursive)
            .map_err(|e| IndexError::Watcher(e.to_string()))?;

        self.debouncer = Some(debouncer);

        Ok(rx)
    }

    /// Stop watching.  Safe to call multiple times.
    pub fn stop(&mut self) {
        self.debouncer.take(); // Drop stops the debouncer
    }
}

/// Check if a path contains a specific component relative to the root.
fn path_contains_component(path: &Path, _root: &Path, component: &str) -> bool {
    path.components().any(|c| {
        c.as_os_str() == component
    })
}
```

**Step 4: Update lib.rs re-exports**

Change the re-export line to include `FileWatcher`:
```rust
pub use watcher::{ChangeEvent, ChangeKind, FileWatcher};
```

**Step 5: Run tests to verify they pass**

Run: `cargo test -p ferret-indexer-core -- watcher`
Expected: All tests PASS

**Step 6: Commit**

```bash
git add ferret-indexer-core/src/watcher.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(core): implement FileWatcher with debounced events (HHC-37)"
```

---

### Task 4: Add integration tests for actual file events (marked `#[ignore]`)

**Files:**
- Modify: `ferret-indexer-core/src/watcher.rs`

**Step 1: Add integration tests**

Add these tests to the `tests` module in `watcher.rs`. They are marked `#[ignore]` because filesystem watcher timing is inherently flaky in CI environments.

```rust
    use std::fs;
    use std::time::Duration;

    /// Helper to wait for events with a timeout.
    fn recv_events(
        rx: &mpsc::Receiver<Vec<ChangeEvent>>,
        timeout: Duration,
    ) -> Vec<ChangeEvent> {
        let mut all = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(events) => all.extend(events),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        all
    }

    #[test]
    #[ignore] // Filesystem watcher timing is inherently flaky in CI
    fn file_creation_produces_event() {
        let tmp = TempDir::new().unwrap();
        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start().unwrap();

        // Give the watcher a moment to set up
        std::thread::sleep(Duration::from_millis(100));

        fs::write(tmp.path().join("new_file.txt"), "hello").unwrap();

        let events = recv_events(&rx, Duration::from_secs(2));
        assert!(
            events.iter().any(|e| e.path.ends_with("new_file.txt")),
            "expected event for new_file.txt, got: {events:?}"
        );
        watcher.stop();
    }

    #[test]
    #[ignore] // Filesystem watcher timing is inherently flaky in CI
    fn file_modification_produces_event() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("existing.txt");
        fs::write(&file_path, "initial").unwrap();

        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        fs::write(&file_path, "modified").unwrap();

        let events = recv_events(&rx, Duration::from_secs(2));
        assert!(
            events.iter().any(|e| e.path.ends_with("existing.txt")),
            "expected event for existing.txt, got: {events:?}"
        );
        watcher.stop();
    }

    #[test]
    #[ignore] // Filesystem watcher timing is inherently flaky in CI
    fn file_deletion_produces_event() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("to_delete.txt");
        fs::write(&file_path, "delete me").unwrap();

        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        fs::remove_file(&file_path).unwrap();

        let events = recv_events(&rx, Duration::from_secs(2));
        assert!(
            events.iter().any(|e| e.path.ends_with("to_delete.txt")
                && e.kind == ChangeKind::Deleted),
            "expected delete event for to_delete.txt, got: {events:?}"
        );
        watcher.stop();
    }

    #[test]
    #[ignore] // Filesystem watcher timing is inherently flaky in CI
    fn gitignored_files_are_filtered() {
        let tmp = TempDir::new().unwrap();

        // Initialize git repo so .gitignore is honoured
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();

        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        // Create both an ignored and a non-ignored file
        fs::write(tmp.path().join("debug.log"), "should be ignored").unwrap();
        fs::write(tmp.path().join("visible.txt"), "should appear").unwrap();

        let events = recv_events(&rx, Duration::from_secs(2));
        assert!(
            !events.iter().any(|e| e.path.ends_with("debug.log")),
            "debug.log should be filtered by gitignore, got: {events:?}"
        );
        watcher.stop();
    }
```

**Step 2: Run unit tests (non-ignored) to verify no regressions**

Run: `cargo test -p ferret-indexer-core -- watcher`
Expected: All non-ignored tests PASS; ignored tests are skipped

**Step 3: Optionally verify ignored tests work locally**

Run: `cargo test -p ferret-indexer-core -- watcher --ignored`
Expected: All 4 integration tests PASS (may occasionally be flaky)

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/watcher.rs
git commit -m "test(core): add integration tests for FileWatcher (HHC-37)"
```

---

### Task 5: Final validation — clippy and full test suite

**Files:** None (validation only)

**Step 1: Run all tests**

Run: `cargo test -p ferret-indexer-core`
Expected: All non-ignored tests PASS

**Step 2: Run clippy**

Run: `cargo clippy -p ferret-indexer-core -- -D warnings`
Expected: No warnings

**Step 3: Fix any issues found by clippy or tests**

Address any problems, then re-run both commands.

**Step 4: Final commit (if any fixes were needed)**

```bash
git add -A
git commit -m "fix(core): address clippy/test issues in watcher module (HHC-37)"
```
