# Git-based Change Detection Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create a `git_diff` module that detects file changes (created, modified, deleted, renamed) by shelling out to `git` commands, enabling incremental re-indexing.

**Architecture:** Two new modules: `changes.rs` defines the shared `ChangeEvent` and `ChangeKind` types; `git_diff.rs` defines `GitChangeDetector` which uses `std::process::Command` to invoke `git diff --name-status`, `git ls-files`, and `git rev-parse` to detect changes since the last indexed commit. Both modules are registered in `lib.rs`.

**Tech Stack:** Rust 2024, `std::process::Command` (no external git library), existing `IndexError` / `Result` from `error.rs`, `thiserror` for error variants, `tempfile` (dev-dependency, already present)

---

## Task 1: Add `Git` error variant to `IndexError`

**Files:**
- Modify: `ferret-indexer-core/src/error.rs`

**Step 1: Add the `Git` variant**

In `ferret-indexer-core/src/error.rs`, add a `Git` variant to `IndexError` after the `Walk` variant:

```rust
/// A git command failed or the directory is not a git repository.
#[error("git error: {0}")]
Git(String),
```

**Step 2: Verify compilation**

Run: `cargo check -p ferret-indexer-core`
Expected: success

**Step 3: Commit**

```bash
git add ferret-indexer-core/src/error.rs
git commit -m "feat(git_diff): add Git variant to IndexError (HHC-38)"
```

---

## Task 2: Create `changes.rs` with shared `ChangeEvent` and `ChangeKind`

**Files:**
- Create: `ferret-indexer-core/src/changes.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

**Step 1: Create `changes.rs`**

```rust
//! Shared change-event types for file-system and git-based change detection.

use std::path::PathBuf;

/// The kind of change detected for a file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

/// A single file-change event.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeEvent {
    /// Path to the changed file, relative to the repository root.
    pub path: PathBuf,
    /// What kind of change occurred.
    pub kind: ChangeKind,
}
```

**Step 2: Register in `lib.rs`**

Add `pub mod changes;` and re-export:
```rust
pub use changes::{ChangeEvent, ChangeKind};
```

**Step 3: Verify compilation**

Run: `cargo check -p ferret-indexer-core`
Expected: success

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/changes.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(changes): add shared ChangeEvent/ChangeKind types (HHC-38)"
```

---

## Task 3: Create `git_diff.rs` with `GitChangeDetector`

**Files:**
- Create: `ferret-indexer-core/src/git_diff.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

**Step 1: Create the module with struct, constructor, and helpers**

Implement:
- `GitChangeDetector::new(repo_root)` — stores root, last_commit = None
- `set_last_indexed_commit(&mut self, sha)`
- `get_head_sha(&self) -> Result<String>` — runs `git rev-parse HEAD`
- `detect_changes(&self) -> Result<Vec<ChangeEvent>>` — unions committed + unstaged + untracked changes
- Internal helpers: `run_git`, `parse_name_status`, `parse_untracked`

**Step 2: Register in `lib.rs`**

Add `pub mod git_diff;` and re-export `GitChangeDetector`.

**Step 3: Verify compilation**

Run: `cargo check -p ferret-indexer-core`

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/git_diff.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(git_diff): add GitChangeDetector with change detection (HHC-38)"
```

---

## Task 4: Write unit tests for parsing logic

**Files:**
- Modify: `ferret-indexer-core/src/git_diff.rs` (add `#[cfg(test)] mod tests`)

**Tests to write:**
1. `test_parse_name_status_added` — "A\tfile.rs" -> Created
2. `test_parse_name_status_modified` — "M\tfile.rs" -> Modified
3. `test_parse_name_status_deleted` — "D\tfile.rs" -> Deleted
4. `test_parse_name_status_renamed` — "R100\told.rs\tnew.rs" -> Renamed (path = new.rs)
5. `test_parse_name_status_mixed` — multi-line output with mixed statuses
6. `test_parse_untracked` — newline-separated paths -> Created events
7. `test_dedup_by_path` — same path from two sources, latest wins
8. `test_get_head_sha` — integration test in real repo
9. `test_detect_changes_in_real_repo` — integration test

**Step 1: Write all tests**
**Step 2: Run tests**

Run: `cargo test -p ferret-indexer-core -- git_diff`
Expected: all pass

**Step 3: Commit**

```bash
git add ferret-indexer-core/src/git_diff.rs
git commit -m "test(git_diff): add unit and integration tests (HHC-38)"
```

---

## Task 5: Run clippy and final validation

**Step 1: Run clippy**

Run: `cargo clippy -p ferret-indexer-core -- -D warnings`
Expected: no warnings

**Step 2: Run full test suite**

Run: `cargo test -p ferret-indexer-core`
Expected: all pass

**Step 3: Final commit (if any fixes needed)**

```bash
git commit -m "fix(git_diff): address clippy warnings (HHC-38)"
```
