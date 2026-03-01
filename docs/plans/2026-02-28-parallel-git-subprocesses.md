# Parallel Git Subprocesses Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Parallelize the three independent git subprocess calls in `detect_changes()` to reduce latency by ~2/3.

**Architecture:** `detect_changes()` currently runs three sequential `git` commands (committed diff, unstaged diff, untracked files). Since these are independent read-only queries, we use `std::thread::scope` to run all three concurrently. Each thread captures its `Result<String>` independently. After all threads join, we merge results into the HashMap exactly as before.

**Tech Stack:** `std::thread::scope` (stable since Rust 1.63, no new dependencies)

---

### Task 1: Parallelize the three git subprocess calls in detect_changes

**Files:**
- Modify: `indexrs-core/src/git_diff.rs:69-103` (the `detect_changes` method)

**Step 1: Refactor detect_changes to use std::thread::scope**

Replace the sequential git calls with `std::thread::scope`. Each of the three git commands runs in its own scoped thread. After all threads complete, merge results into the HashMap as before.

The key insight: `run_git` takes `&self` and returns `Result<String>`. Since `std::thread::scope` allows borrowing from the enclosing scope, each thread can call `self.run_git(...)` without needing `Arc` or `clone`.

```rust
pub fn detect_changes(&self) -> Result<Vec<ChangeEvent>> {
    // Run the three git commands in parallel using scoped threads.
    // Each is an independent read-only query against git state.
    let (committed_output, unstaged_output, untracked_output) = std::thread::scope(|s| {
        // 1. Committed changes since last indexed commit.
        let committed_handle = s.spawn(|| -> Result<Option<String>> {
            if let Some(ref base) = self.last_indexed_commit {
                Ok(Some(self.run_git(&["diff", "--name-status", "-z", base, "HEAD"])?))
            } else {
                Ok(None)
            }
        });

        // 2. Unstaged working-tree changes.
        let unstaged_handle = s.spawn(|| -> Result<String> {
            self.run_git(&["diff", "--name-status", "-z"])
        });

        // 3. Untracked files.
        let untracked_handle = s.spawn(|| -> Result<String> {
            self.run_git(&["ls-files", "-z", "--others", "--exclude-standard"])
        });

        // Collect results (join unwrap is safe: scoped threads don't panic
        // unless our closures panic, and they don't).
        let committed = committed_handle.join().expect("committed thread panicked");
        let unstaged = unstaged_handle.join().expect("unstaged thread panicked");
        let untracked = untracked_handle.join().expect("untracked thread panicked");

        (committed, unstaged, untracked)
    });

    // Merge results into the HashMap exactly as before.
    let mut changes: HashMap<PathBuf, ChangeKind> = HashMap::new();

    // 1. Committed changes.
    if let Some(output) = committed_output? {
        for event in parse_name_status_nul(&output) {
            changes.insert(event.path, event.kind);
        }
    }

    // 2. Unstaged changes.
    let unstaged = unstaged_output?;
    for event in parse_name_status_nul(&unstaged) {
        changes.insert(event.path, event.kind);
    }

    // 3. Untracked files.
    let untracked = untracked_output?;
    for event in parse_untracked_nul(&untracked) {
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
```

**Step 2: Run clippy, fmt check, and tests**

Run:
```bash
cd /Users/howard/src/indexrs/.claude/worktrees/parallel-git-subprocesses && cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check && cargo test --workspace
```

Expected: All pass. The existing tests cover parsing and integration behavior, so no new tests needed -- the parallelization is a pure internal refactor that preserves identical observable behavior.

**Step 3: Commit**

```bash
git add indexrs-core/src/git_diff.rs
git commit -m "perf: parallelize git subprocess calls in detect_changes"
```
