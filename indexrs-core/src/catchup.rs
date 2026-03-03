//! Catch-up logic for daemon startup.
//!
//! Detects changes since the last checkpoint using git (fast path) or
//! hash-based diff (fallback), applies them to the segment manager, and
//! writes a new checkpoint.

use std::path::Path;
use std::sync::Arc;

use crate::changes::{ChangeEvent, ChangeKind};
use crate::checkpoint::{Checkpoint, read_checkpoint, write_checkpoint};
use crate::error::Result;
use crate::git_diff::GitChangeDetector;
use crate::hash_diff::hash_diff;
use crate::reindex_progress::ReindexProgress;
use crate::segment_manager::SegmentManager;

/// Run catch-up: detect changes since last checkpoint and apply them.
///
/// Returns the list of changes that were applied (empty if nothing changed).
///
/// Strategy:
/// 1. Read checkpoint from disk.
/// 2. **Fast path**: if checkpoint has a `git_commit` and repo is git,
///    use `git diff` to find changes.
/// 3. **Fallback**: walk the tree and compare blake3 hashes.
/// 4. Apply changes via `SegmentManager::apply_changes()`.
/// 5. Write updated checkpoint.
pub fn run_catchup(
    repo_root: &Path,
    indexrs_dir: &Path,
    manager: &Arc<SegmentManager>,
) -> Result<Vec<ChangeEvent>> {
    run_catchup_with_progress(repo_root, indexrs_dir, manager, |_| {})
}

/// Like [`run_catchup`], but calls `on_progress` with structured
/// [`ReindexProgress`] events at each phase so callers can stream status to a UI.
pub fn run_catchup_with_progress<F: Fn(ReindexProgress) + Send + Sync>(
    repo_root: &Path,
    indexrs_dir: &Path,
    manager: &Arc<SegmentManager>,
    on_progress: F,
) -> Result<Vec<ChangeEvent>> {
    let checkpoint = read_checkpoint(indexrs_dir)?;

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
    write_checkpoint(indexrs_dir, &new_checkpoint)?;

    Ok(changes)
}

/// Try git-based catch-up. Returns `None` if no git checkpoint available.
fn try_git_catchup(
    repo_root: &Path,
    checkpoint: &Option<Checkpoint>,
) -> Option<Result<Vec<ChangeEvent>>> {
    let cp = checkpoint.as_ref()?;
    let git_commit = cp.git_commit.as_ref()?;

    let git = GitChangeDetector::new(repo_root.to_path_buf());

    // Fast path: if checkpoint SHA matches HEAD and working tree is clean,
    // skip the expensive 3-way git diff entirely.
    if let Ok(head) = git.get_head_sha()
        && head == *git_commit
        && git.is_working_tree_clean()
    {
        tracing::debug!("checkpoint matches HEAD and tree is clean, skipping git diff");
        return Some(Ok(Vec::new()));
    }

    let mut git = git;
    git.set_last_indexed_commit(git_commit.clone());
    Some(git.detect_changes())
}

/// Hash-based fallback: walk tree, compare hashes, return changes.
fn run_hash_fallback(repo_root: &Path, manager: &SegmentManager) -> Result<Vec<ChangeEvent>> {
    let snapshot = manager.snapshot();
    hash_diff(repo_root, &snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_git_repo(path: &Path) {
        let out = std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(out.status.success(), "git init failed");
        // Configure user for CI environments where no global git config exists.
        for (key, val) in [("user.name", "test"), ("user.email", "test@test.com")] {
            let out = std::process::Command::new("git")
                .args(["config", key, val])
                .current_dir(path)
                .output()
                .unwrap();
            assert!(out.status.success(), "git config {key} failed");
        }
        let out = std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn test_catchup_no_checkpoint_uses_hash_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let indexrs_dir = repo.join(".indexrs");
        fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&indexrs_dir).unwrap());

        // Write a file on disk but don't index it.
        fs::write(repo.join("new.rs"), "fn new() { let x = 1; }").unwrap();

        let changes = run_catchup(repo, &indexrs_dir, &manager).unwrap();

        assert!(
            changes
                .iter()
                .any(|e| e.path.to_string_lossy().contains("new.rs")),
            "expected new.rs in changes, got: {changes:?}"
        );

        // Checkpoint should be written.
        let cp = read_checkpoint(&indexrs_dir).unwrap();
        assert!(cp.is_some());
    }

    #[test]
    fn test_catchup_with_checkpoint_uses_git() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let indexrs_dir = repo.join(".indexrs");
        fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&indexrs_dir).unwrap());

        // Write a checkpoint with the current HEAD.
        let git = GitChangeDetector::new(repo.to_path_buf());
        let head = git.get_head_sha().unwrap();
        let cp = Checkpoint::new(Some(head), 0);
        write_checkpoint(&indexrs_dir, &cp).unwrap();

        // Create an untracked file (will show in git ls-files).
        fs::write(repo.join("added.rs"), "fn added() { let x = 1; }").unwrap();

        let changes = run_catchup(repo, &indexrs_dir, &manager).unwrap();

        assert!(
            changes
                .iter()
                .any(|e| e.path.to_string_lossy().contains("added.rs")),
            "expected added.rs in changes, got: {changes:?}"
        );
    }

    #[test]
    fn test_catchup_with_progress_reports_structured_events() {
        use crate::reindex_progress::ReindexProgress;

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let indexrs_dir = repo.join(".indexrs");
        fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&indexrs_dir).unwrap());

        // Write checkpoint at current HEAD.
        let git = GitChangeDetector::new(repo.to_path_buf());
        let head = git.get_head_sha().unwrap();
        let cp = Checkpoint::new(Some(head), 0);
        write_checkpoint(&indexrs_dir, &cp).unwrap();

        // Create an untracked file so there's something to detect.
        fs::write(repo.join("progress.rs"), "fn progress() { let x = 1; }").unwrap();

        let events = std::sync::Mutex::new(Vec::new());
        let changes = run_catchup_with_progress(repo, &indexrs_dir, &manager, |ev| {
            events.lock().unwrap().push(ev);
        })
        .unwrap();

        let events = events.into_inner().unwrap();
        assert!(!changes.is_empty());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReindexProgress::DetectingChanges)),
            "expected DetectingChanges, got: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReindexProgress::ChangesDetected { .. })),
            "expected ChangesDetected, got: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReindexProgress::Complete { .. })),
            "expected Complete, got: {events:?}"
        );
    }

    #[test]
    fn test_catchup_with_progress_no_changes_structured() {
        use crate::reindex_progress::ReindexProgress;

        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let indexrs_dir = repo.join(".indexrs");
        fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&indexrs_dir).unwrap());

        let git = GitChangeDetector::new(repo.to_path_buf());
        let head = git.get_head_sha().unwrap();
        let cp = Checkpoint::new(Some(head), 0);
        write_checkpoint(&indexrs_dir, &cp).unwrap();

        let events = std::sync::Mutex::new(Vec::new());
        let changes = run_catchup_with_progress(repo, &indexrs_dir, &manager, |ev| {
            events.lock().unwrap().push(ev);
        })
        .unwrap();

        let events = events.into_inner().unwrap();
        assert!(changes.is_empty());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReindexProgress::NoChanges)),
            "expected NoChanges, got: {events:?}"
        );
    }

    #[test]
    fn test_catchup_no_changes_writes_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let indexrs_dir = repo.join(".indexrs");
        fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&indexrs_dir).unwrap());

        // Write checkpoint at current HEAD, no changes.
        let git = GitChangeDetector::new(repo.to_path_buf());
        let head = git.get_head_sha().unwrap();
        let cp = Checkpoint::new(Some(head), 0);
        write_checkpoint(&indexrs_dir, &cp).unwrap();

        let changes = run_catchup(repo, &indexrs_dir, &manager).unwrap();
        assert!(changes.is_empty());

        // Checkpoint should still be present.
        let cp2 = read_checkpoint(&indexrs_dir).unwrap();
        assert!(cp2.is_some());
    }
}
