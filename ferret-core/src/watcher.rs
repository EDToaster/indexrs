//! File watcher with debounced change detection.
//!
//! Uses [`notify_debouncer_full`] to watch a directory tree recursively,
//! translating raw filesystem events into [`ChangeEvent`] values.  Events for
//! paths matched by `.gitignore` are silently dropped.
//!
//! # Quick start
//!
//! ```no_run
//! use ferret_indexer_core::watcher::FileWatcher;
//! use ferret_indexer_core::changes::ChangeEvent;
//!
//! let mut watcher = FileWatcher::new("/path/to/repo".into()).unwrap();
//! let rx = watcher.start().unwrap();
//!
//! // Events arrive as Vec<ChangeEvent> batches (one per debounce window).
//! for batch in rx.iter() {
//!     for event in &batch {
//!         println!("{:?}: {}", event.kind, event.path.display());
//!     }
//! }
//! ```

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use ignore::gitignore::Gitignore;
use notify_debouncer_full::notify::event::{EventKind, ModifyKind, RenameMode};
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};

use crate::changes::{ChangeEvent, ChangeKind};
use crate::error::{IndexError, Result};

/// Debounce timeout in milliseconds.  Events within this window are coalesced
/// into a single batch.
const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// FileWatcher
// ---------------------------------------------------------------------------

/// File watcher for live change detection.
///
/// Wraps [`notify_debouncer_full`] to provide debounced filesystem events,
/// filtered through `.gitignore` rules.  Events for paths inside `.git/`
/// directories are also suppressed.
pub struct FileWatcher {
    root: PathBuf,
    gitignore: Gitignore,
    debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
}

impl FileWatcher {
    /// Create a new file watcher rooted at the given path.
    ///
    /// Loads `.gitignore` rules from the root directory (if present).
    /// Does **not** start watching until [`start`](Self::start) is called.
    pub fn new(root: PathBuf) -> Result<Self> {
        let gitignore_path = root.join(".gitignore");
        // `Gitignore::new` returns (matcher, Option<Error>).  The error only
        // contains parse warnings — we intentionally ignore them.  When no
        // `.gitignore` file exists, the matcher simply never matches.
        let (gitignore, _parse_warning) = Gitignore::new(gitignore_path);

        Ok(Self {
            root,
            gitignore,
            debouncer: None,
        })
    }

    /// Start watching the root directory recursively.
    ///
    /// Returns a channel receiver that yields batches of [`ChangeEvent`]s.
    /// Each batch corresponds to one debounce window (~200 ms).
    pub fn start(&mut self) -> Result<mpsc::Receiver<Vec<ChangeEvent>>> {
        let (tx, rx) = mpsc::channel();
        let gitignore = self.gitignore.clone();
        let root = self.root.clone();

        let mut debouncer = new_debouncer(
            DEBOUNCE_TIMEOUT,
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    let changes: Vec<ChangeEvent> = events
                        .into_iter()
                        .flat_map(|debounced| {
                            let event = &debounced.event;
                            let kind = match classify_event_kind(&event.kind) {
                                Some(k) => k,
                                None => return vec![],
                            };

                            // For renames with both source and destination paths,
                            // emit a Deleted event for the old path and a Created
                            // event for the new path.
                            if kind == ChangeKind::Renamed && event.paths.len() >= 2 {
                                let old_path = match event.paths.first() {
                                    Some(p) => p,
                                    None => return vec![],
                                };
                                let new_path = match event.paths.last() {
                                    Some(p) => p,
                                    None => return vec![],
                                };
                                let mut result = vec![];
                                // Emit Deleted for old path
                                if !path_has_component(old_path, ".git") {
                                    let is_dir = old_path.is_dir();
                                    if !gitignore
                                        .matched_path_or_any_parents(old_path, is_dir)
                                        .is_ignore()
                                    {
                                        let rel = old_path.strip_prefix(&root).unwrap_or(old_path);
                                        result.push(ChangeEvent {
                                            path: rel.to_path_buf(),
                                            kind: ChangeKind::Deleted,
                                        });
                                    }
                                }
                                // Emit Created for new path
                                if !path_has_component(new_path, ".git") {
                                    let is_dir = new_path.is_dir();
                                    if !gitignore
                                        .matched_path_or_any_parents(new_path, is_dir)
                                        .is_ignore()
                                    {
                                        let rel = new_path.strip_prefix(&root).unwrap_or(new_path);
                                        result.push(ChangeEvent {
                                            path: rel.to_path_buf(),
                                            kind: ChangeKind::Created,
                                        });
                                    }
                                }
                                return result;
                            }

                            // Use the last path for all other events.
                            let path = match event.paths.last() {
                                Some(p) => p,
                                None => return vec![],
                            };

                            // Drop events inside `.git/` directories.
                            if path_has_component(path, ".git") {
                                return vec![];
                            }

                            // Drop events for gitignored paths.
                            let is_dir = path.is_dir();
                            if gitignore
                                .matched_path_or_any_parents(path, is_dir)
                                .is_ignore()
                            {
                                return vec![];
                            }

                            // Strip the root prefix to emit relative paths,
                            // matching git_diff.rs which emits repo-relative paths.
                            let relative_path = path.strip_prefix(&root).unwrap_or(path);

                            vec![ChangeEvent {
                                path: relative_path.to_path_buf(),
                                kind,
                            }]
                        })
                        .collect();

                    if !changes.is_empty()
                        && let Err(e) = tx.send(changes)
                    {
                        tracing::warn!("failed to send change events: {e}");
                    }
                }
                Err(errors) => {
                    for error in errors {
                        tracing::warn!("watcher error: {error}");
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
        // Dropping the debouncer signals its background thread to stop.
        self.debouncer.take();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a notify [`EventKind`] to our [`ChangeKind`], returning `None` for
/// event kinds we do not care about (e.g. `Access`, `Other`).
fn classify_event_kind(kind: &EventKind) -> Option<ChangeKind> {
    match kind {
        EventKind::Create(_) => Some(ChangeKind::Created),
        EventKind::Remove(_) => Some(ChangeKind::Deleted),
        EventKind::Modify(ModifyKind::Name(
            RenameMode::Both | RenameMode::To | RenameMode::From | RenameMode::Any,
        )) => Some(ChangeKind::Renamed),
        EventKind::Modify(_) => Some(ChangeKind::Modified),
        _ => None,
    }
}

/// Returns `true` if any component of `path` equals `component`.
fn path_has_component(path: &Path, component: &str) -> bool {
    path.components().any(|c| c.as_os_str() == component)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    // -- ChangeKind / ChangeEvent unit tests --------------------------------

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

    // -- classify_event_kind ------------------------------------------------

    #[test]
    fn classify_create() {
        use notify_debouncer_full::notify::event::CreateKind;
        let kind = EventKind::Create(CreateKind::File);
        assert_eq!(classify_event_kind(&kind), Some(ChangeKind::Created));
    }

    #[test]
    fn classify_remove() {
        use notify_debouncer_full::notify::event::RemoveKind;
        let kind = EventKind::Remove(RemoveKind::File);
        assert_eq!(classify_event_kind(&kind), Some(ChangeKind::Deleted));
    }

    #[test]
    fn classify_modify_data() {
        use notify_debouncer_full::notify::event::DataChange;
        let kind = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        assert_eq!(classify_event_kind(&kind), Some(ChangeKind::Modified));
    }

    #[test]
    fn classify_rename_both() {
        let kind = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        assert_eq!(classify_event_kind(&kind), Some(ChangeKind::Renamed));
    }

    #[test]
    fn classify_access_ignored() {
        use notify_debouncer_full::notify::event::AccessKind;
        let kind = EventKind::Access(AccessKind::Read);
        assert_eq!(classify_event_kind(&kind), None);
    }

    // -- path_has_component -------------------------------------------------

    #[test]
    fn detects_git_component() {
        assert!(path_has_component(Path::new("/repo/.git/config"), ".git"));
    }

    #[test]
    fn no_false_positive_on_similar_name() {
        assert!(!path_has_component(
            Path::new("/repo/.github/workflows/ci.yml"),
            ".git"
        ));
    }

    // -- FileWatcher API surface tests --------------------------------------

    #[test]
    fn file_watcher_new_on_valid_dir() {
        let tmp = TempDir::new().unwrap();
        let watcher = FileWatcher::new(tmp.path().to_path_buf());
        assert!(watcher.is_ok());
    }

    #[test]
    fn file_watcher_new_on_nonexistent_dir() {
        // Should still succeed at construction — the watch error comes at start().
        let result = FileWatcher::new(PathBuf::from("/nonexistent/path/that/does/not/exist"));
        assert!(result.is_ok());
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
        watcher.stop(); // must not panic
    }

    #[test]
    fn file_watcher_start_returns_receiver() {
        let tmp = TempDir::new().unwrap();
        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start().unwrap();
        // Channel should exist but have no events yet.
        assert!(rx.try_recv().is_err());
        watcher.stop();
    }

    // -- Integration tests (filesystem-dependent, flaky in CI) --------------

    /// Helper to drain events from the receiver with a timeout.
    fn recv_events(rx: &mpsc::Receiver<Vec<ChangeEvent>>, timeout: Duration) -> Vec<ChangeEvent> {
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

        // Give the watcher a moment to initialise.
        std::thread::sleep(Duration::from_millis(100));

        std::fs::write(tmp.path().join("new_file.txt"), "hello").unwrap();

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
        std::fs::write(&file_path, "initial").unwrap();

        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        std::fs::write(&file_path, "modified").unwrap();

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
        std::fs::write(&file_path, "delete me").unwrap();

        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        std::fs::remove_file(&file_path).unwrap();

        let events = recv_events(&rx, Duration::from_secs(2));
        assert!(
            events
                .iter()
                .any(|e| e.path.ends_with("to_delete.txt") && e.kind == ChangeKind::Deleted),
            "expected delete event for to_delete.txt, got: {events:?}"
        );
        watcher.stop();
    }

    #[test]
    #[ignore] // Filesystem watcher timing is inherently flaky in CI
    fn gitignored_files_are_filtered() {
        let tmp = TempDir::new().unwrap();

        // Initialise a git repo so `.gitignore` is honoured.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();

        let mut watcher = FileWatcher::new(tmp.path().to_path_buf()).unwrap();
        let rx = watcher.start().unwrap();
        std::thread::sleep(Duration::from_millis(100));

        // Create both an ignored and a non-ignored file.
        std::fs::write(tmp.path().join("debug.log"), "should be ignored").unwrap();
        std::fs::write(tmp.path().join("visible.txt"), "should appear").unwrap();

        let events = recv_events(&rx, Duration::from_secs(2));
        assert!(
            !events.iter().any(|e| e.path.ends_with("debug.log")),
            "debug.log should be filtered by gitignore, got: {events:?}"
        );
        watcher.stop();
    }
}
