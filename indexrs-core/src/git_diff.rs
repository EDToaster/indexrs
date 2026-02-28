//! Git-based change detection via `git` CLI commands.
//!
//! [`GitChangeDetector`] shells out to `git` to discover file changes since
//! the last indexed commit.  It combines three sources of changes:
//!
//! 1. **Committed changes** — `git diff --name-status <last_commit> HEAD`
//! 2. **Unstaged changes** — `git diff --name-status`
//! 3. **Untracked files** — `git ls-files --others --exclude-standard`
//!
//! Results are de-duplicated by path (later sources override earlier ones) and
//! returned as [`ChangeEvent`] values with paths relative to the repository root.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::changes::{ChangeEvent, ChangeKind};
use crate::error::{IndexError, Result};

/// Detects file changes by invoking `git` CLI commands.
///
/// Does **not** require a git library dependency — all interaction happens via
/// `std::process::Command`.
pub struct GitChangeDetector {
    /// Root directory of the git repository.
    repo_root: PathBuf,
    /// SHA of the last commit that was fully indexed.  When `None`, only
    /// unstaged and untracked changes are reported.
    last_indexed_commit: Option<String>,
}

impl GitChangeDetector {
    /// Create a new detector for the repository rooted at `repo_root`.
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            last_indexed_commit: None,
        }
    }

    /// Record the SHA of the most recently indexed commit.
    ///
    /// Subsequent calls to [`detect_changes`](Self::detect_changes) will use
    /// this as the baseline for `git diff`.
    ///
    /// # Panics
    ///
    /// Panics if `sha` is not a valid hex string of at least 7 characters.
    pub fn set_last_indexed_commit(&mut self, sha: String) {
        assert!(
            sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()),
            "set_last_indexed_commit expects a hex SHA (>= 7 chars), got: {sha}"
        );
        self.last_indexed_commit = Some(sha);
    }

    /// Return the SHA of the current `HEAD` commit.
    pub fn get_head_sha(&self) -> Result<String> {
        let output = self.run_git(&["rev-parse", "HEAD"])?;
        Ok(output.trim().to_string())
    }

    /// Detect all file changes and return them as [`ChangeEvent`] values.
    ///
    /// The events are de-duplicated by path: if the same path appears in
    /// multiple sources (committed, unstaged, untracked), the latest source
    /// wins. Paths under `.indexrs/` are always excluded since those are
    /// index files, not source files.
    pub fn detect_changes(&self) -> Result<Vec<ChangeEvent>> {
        let mut changes: HashMap<PathBuf, ChangeKind> = HashMap::new();

        // 1. Committed changes since last indexed commit.
        if let Some(ref base) = self.last_indexed_commit {
            let output = self.run_git(&["diff", "--name-status", base, "HEAD"])?;
            for event in parse_name_status(&output) {
                changes.insert(event.path, event.kind);
            }
        }

        // 2. Unstaged working-tree changes.
        let unstaged = self.run_git(&["diff", "--name-status"])?;
        for event in parse_name_status(&unstaged) {
            changes.insert(event.path, event.kind);
        }

        // 3. Untracked files.
        let untracked = self.run_git(&["ls-files", "--others", "--exclude-standard"])?;
        for event in parse_untracked(&untracked) {
            changes.insert(event.path, event.kind);
        }

        let mut events: Vec<ChangeEvent> = changes
            .into_iter()
            .filter(|(path, _)| !is_indexrs_path(path))
            .map(|(path, kind)| ChangeEvent { path, kind })
            .collect();

        // Sort for deterministic output.
        events.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(events)
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Run a `git` command in the repository root and return its stdout.
    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| IndexError::Git(format!("failed to execute git: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(IndexError::Git(format!(
                "git {} failed ({}): {}",
                args.join(" "),
                output.status,
                stderr.trim()
            )));
        }

        String::from_utf8(output.stdout)
            .map_err(|e| IndexError::Git(format!("git output was not valid UTF-8: {e}")))
    }
}

/// Check whether a path is under the `.indexrs/` directory.
///
/// These are index files (segments, tombstones, etc.) and should never be
/// reported as source-file changes.
fn is_indexrs_path(path: &Path) -> bool {
    path.starts_with(".indexrs")
}

// ------------------------------------------------------------------
// Parsing helpers (free functions so they are easily unit-testable)
// ------------------------------------------------------------------

/// Parse the output of `git diff --name-status`.
///
/// Each line has the format `<status>\t<path>` or, for renames,
/// `R<score>\t<old>\t<new>`.
fn parse_name_status(output: &str) -> Vec<ChangeEvent> {
    let mut events = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }

        let status = parts[0];

        // Renames and copies have two paths: old and new.
        // Emit a Deleted event for the old path and a Created event for the new path,
        // so the indexer removes the stale entry and indexes the new one.
        if status.starts_with('R') && parts.len() >= 3 {
            events.push(ChangeEvent {
                path: PathBuf::from(parts[1]),
                kind: ChangeKind::Deleted,
            });
            events.push(ChangeEvent {
                path: PathBuf::from(parts[2]),
                kind: ChangeKind::Created,
            });
            continue;
        }

        if status.starts_with('C') && parts.len() >= 3 {
            // Copy: original still exists, new copy needs indexing.
            events.push(ChangeEvent {
                path: PathBuf::from(parts[2]),
                kind: ChangeKind::Created,
            });
            continue;
        }

        let kind = if status == "A" {
            ChangeKind::Created
        } else if status == "M" {
            ChangeKind::Modified
        } else if status == "D" {
            ChangeKind::Deleted
        } else {
            // Unknown status code — treat as Modified.
            ChangeKind::Modified
        };

        events.push(ChangeEvent {
            path: PathBuf::from(parts[1]),
            kind,
        });
    }

    events
}

/// Parse the output of `git ls-files --others --exclude-standard`.
///
/// Each line is a single file path; all are treated as [`ChangeKind::Created`].
fn parse_untracked(output: &str) -> Vec<ChangeEvent> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| ChangeEvent {
            path: PathBuf::from(l.trim()),
            kind: ChangeKind::Created,
        })
        .collect()
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_name_status ------------------------------------------

    #[test]
    fn test_parse_name_status_added() {
        let output = "A\tsrc/main.rs\n";
        let events = parse_name_status(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, PathBuf::from("src/main.rs"));
        assert_eq!(events[0].kind, ChangeKind::Created);
    }

    #[test]
    fn test_parse_name_status_modified() {
        let output = "M\tlib.rs\n";
        let events = parse_name_status(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, PathBuf::from("lib.rs"));
        assert_eq!(events[0].kind, ChangeKind::Modified);
    }

    #[test]
    fn test_parse_name_status_deleted() {
        let output = "D\told_file.rs\n";
        let events = parse_name_status(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, PathBuf::from("old_file.rs"));
        assert_eq!(events[0].kind, ChangeKind::Deleted);
    }

    #[test]
    fn test_parse_name_status_renamed() {
        let output = "R100\told.rs\tnew.rs\n";
        let events = parse_name_status(output);
        // Renames emit two events: Delete old + Create new.
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, PathBuf::from("old.rs"));
        assert_eq!(events[0].kind, ChangeKind::Deleted);
        assert_eq!(events[1].path, PathBuf::from("new.rs"));
        assert_eq!(events[1].kind, ChangeKind::Created);
    }

    #[test]
    fn test_parse_name_status_renamed_partial_score() {
        let output = "R075\tsrc/foo.rs\tsrc/bar.rs\n";
        let events = parse_name_status(output);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, PathBuf::from("src/foo.rs"));
        assert_eq!(events[0].kind, ChangeKind::Deleted);
        assert_eq!(events[1].path, PathBuf::from("src/bar.rs"));
        assert_eq!(events[1].kind, ChangeKind::Created);
    }

    #[test]
    fn test_parse_name_status_mixed() {
        let output = "A\tnew.rs\nM\texisting.rs\nD\tremoved.rs\nR100\told.rs\trenamed.rs\n";
        let events = parse_name_status(output);
        // 3 normal events + 2 for rename (Delete old + Create new) = 5
        assert_eq!(events.len(), 5);

        assert_eq!(events[0].kind, ChangeKind::Created);
        assert_eq!(events[0].path, PathBuf::from("new.rs"));

        assert_eq!(events[1].kind, ChangeKind::Modified);
        assert_eq!(events[1].path, PathBuf::from("existing.rs"));

        assert_eq!(events[2].kind, ChangeKind::Deleted);
        assert_eq!(events[2].path, PathBuf::from("removed.rs"));

        assert_eq!(events[3].kind, ChangeKind::Deleted);
        assert_eq!(events[3].path, PathBuf::from("old.rs"));

        assert_eq!(events[4].kind, ChangeKind::Created);
        assert_eq!(events[4].path, PathBuf::from("renamed.rs"));
    }

    #[test]
    fn test_parse_name_status_empty() {
        let events = parse_name_status("");
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_name_status_blank_lines() {
        let output = "\n\n  \n";
        let events = parse_name_status(output);
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_name_status_copy_indexes_new_path() {
        // 'C' (copy) should emit a Created event for the new copy path.
        let output = "C100\toriginal.rs\tcopy.rs\n";
        let events = parse_name_status(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, PathBuf::from("copy.rs"));
        assert_eq!(events[0].kind, ChangeKind::Created);
    }

    // ---- parse_untracked --------------------------------------------

    #[test]
    fn test_parse_untracked() {
        let output = "foo.rs\nbar/baz.rs\n";
        let events = parse_untracked(output);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].path, PathBuf::from("foo.rs"));
        assert_eq!(events[0].kind, ChangeKind::Created);
        assert_eq!(events[1].path, PathBuf::from("bar/baz.rs"));
        assert_eq!(events[1].kind, ChangeKind::Created);
    }

    #[test]
    fn test_parse_untracked_empty() {
        let events = parse_untracked("");
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_untracked_trailing_blanks() {
        let output = "a.rs\n\n  \n";
        let events = parse_untracked(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, PathBuf::from("a.rs"));
    }

    // ---- dedup behaviour --------------------------------------------

    #[test]
    fn test_dedup_by_path_latest_wins() {
        // Simulate: committed says "file.rs" was Modified, then unstaged
        // says it was Deleted.  The HashMap insert from the later source
        // should overwrite the earlier one.
        let mut changes: HashMap<PathBuf, ChangeKind> = HashMap::new();

        // Committed source.
        let committed = parse_name_status("M\tfile.rs\n");
        for e in committed {
            changes.insert(e.path, e.kind);
        }

        // Unstaged source (overwrites).
        let unstaged = parse_name_status("D\tfile.rs\n");
        for e in unstaged {
            changes.insert(e.path, e.kind);
        }

        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes.get(&PathBuf::from("file.rs")),
            Some(&ChangeKind::Deleted)
        );
    }

    // ---- Integration tests (require a real git repo) ----------------

    #[test]
    fn test_get_head_sha_in_real_repo() {
        // This test runs inside the indexrs repository which is a git repo.
        let repo_root = find_repo_root();
        let detector = GitChangeDetector::new(repo_root);
        let sha = detector.get_head_sha().expect("should get HEAD sha");

        // A SHA-1 hex string is 40 characters.
        assert_eq!(sha.len(), 40, "HEAD sha should be 40 hex chars: {sha}");
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit()),
            "sha should be hex: {sha}"
        );
    }

    #[test]
    fn test_detect_changes_returns_results() {
        // In a working repo with uncommitted files, detect_changes should
        // succeed (though the exact set of events depends on working-tree
        // state, so we just check it doesn't error).
        let repo_root = find_repo_root();
        let detector = GitChangeDetector::new(repo_root);
        let events = detector
            .detect_changes()
            .expect("detect_changes should succeed");

        // The result is a Vec — it may be empty if the tree is pristine,
        // but the call itself must not fail.
        assert!(
            events.iter().all(|e| e.path.is_relative()),
            "all paths should be relative to repo root"
        );
    }

    #[test]
    fn test_git_error_on_non_repo() {
        let detector = GitChangeDetector::new(PathBuf::from("/tmp"));
        let result = detector.get_head_sha();
        assert!(result.is_err(), "should error for non-git directory");

        if let Err(IndexError::Git(msg)) = result {
            assert!(
                msg.contains("git") || msg.contains("fatal"),
                "error message should mention git: {msg}"
            );
        } else {
            panic!("expected IndexError::Git variant");
        }
    }

    // ---- .indexrs path filtering ------------------------------------

    #[test]
    fn test_is_indexrs_path() {
        assert!(is_indexrs_path(&PathBuf::from(".indexrs/segments/seg_0000/trigrams.bin")));
        assert!(is_indexrs_path(&PathBuf::from(".indexrs/lock")));
        assert!(is_indexrs_path(&PathBuf::from(".indexrs")));
        assert!(!is_indexrs_path(&PathBuf::from("src/main.rs")));
        assert!(!is_indexrs_path(&PathBuf::from(".gitignore")));
        assert!(!is_indexrs_path(&PathBuf::from("foo/.indexrs/bar")));
    }

    #[test]
    fn test_parse_untracked_filters_indexrs() {
        // Simulate git reporting .indexrs files as untracked
        let output = "src/main.rs\n.indexrs/segments/seg_0000/trigrams.bin\n.indexrs/lock\nlib.rs\n";
        let events = parse_untracked(output);

        // parse_untracked itself doesn't filter — the filter is in detect_changes.
        // But we can test is_indexrs_path on the results.
        let filtered: Vec<_> = events
            .into_iter()
            .filter(|e| !is_indexrs_path(&e.path))
            .collect();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].path, PathBuf::from("src/main.rs"));
        assert_eq!(filtered[1].path, PathBuf::from("lib.rs"));
    }

    // ---- helpers ----------------------------------------------------

    /// Walk up from the manifest directory to find the git repo root.
    fn find_repo_root() -> PathBuf {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            if dir.join(".git").exists() {
                return dir;
            }
            if !dir.pop() {
                panic!("could not find git repo root above CARGO_MANIFEST_DIR");
            }
        }
    }
}
