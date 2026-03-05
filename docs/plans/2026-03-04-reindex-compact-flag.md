# `--compact` Flag for `ferret reindex` Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a `--compact` CLI flag to `ferret reindex` that forces compaction after reindex, mutually exclusive with `--full`.

**Architecture:** Thread a `force_compact: bool` from CLI args → `DaemonRequest::Reindex` → `run_catchup_with_progress()` → compaction logic. When `true`, always compact after reindex regardless of `should_compact()` heuristic.

**Tech Stack:** Rust, clap, serde, tokio

---

### Task 1: Add `compact` field to `DaemonRequest::Reindex`

**Files:**
- Modify: `ferret-indexer-daemon/src/types.rs:91`

**Step 1: Add the field**

Change line 91 from:
```rust
    Reindex,
```
to:
```rust
    Reindex {
        /// When true, force compaction after reindex regardless of heuristics.
        #[serde(default)]
        compact: bool,
    },
```

`#[serde(default)]` ensures backward compatibility — a bare `{"type":"Reindex"}` without `compact` deserializes `compact` as `false`.

**Step 2: Run `cargo check -p ferret-indexer-daemon`**

Expected: PASS (types only; callers haven't been updated yet, but this crate compiles standalone).

**Step 3: Commit**

```bash
git add ferret-indexer-daemon/src/types.rs
git commit -m "feat(daemon): add compact field to Reindex request"
```

---

### Task 2: Add `force_compact` parameter to `run_catchup_with_progress`

**Files:**
- Modify: `ferret-indexer-core/src/catchup.rs:29-44` (function signatures)
- Modify: `ferret-indexer-core/src/lib.rs:47` (re-export unchanged, but verify)

**Step 1: Write a failing test**

Add to `ferret-indexer-core/src/catchup.rs` in the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn test_catchup_force_compact_emits_compacting_event() {
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

        // Create a file so there's a change to apply.
        fs::write(repo.join("compact_test.rs"), "fn compact() {}").unwrap();

        let events = std::sync::Mutex::new(Vec::new());
        let _changes = run_catchup_with_progress(repo, &ferret_dir, &manager, true, |ev| {
            events.lock().unwrap().push(ev);
        })
        .unwrap();

        let events = events.into_inner().unwrap();
        // With force_compact=true and at least 1 segment, CompactingSegments should fire.
        assert!(
            events.iter().any(|e| matches!(e, ReindexProgress::CompactingSegments { .. })),
            "expected CompactingSegments with force_compact=true, got: {events:?}"
        );
    }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_catchup_force_compact_emits_compacting_event`
Expected: FAIL — `run_catchup_with_progress` doesn't accept `force_compact` parameter yet.

**Step 3: Update function signatures**

In `catchup.rs`, update `run_catchup` (line 29) to pass `false`:
```rust
pub fn run_catchup(
    repo_root: &Path,
    ferret_dir: &Path,
    manager: &Arc<SegmentManager>,
) -> Result<Vec<ChangeEvent>> {
    run_catchup_with_progress(repo_root, ferret_dir, manager, false, |_| {})
}
```

Update `run_catchup_with_progress` (line 39) to accept `force_compact`:
```rust
pub fn run_catchup_with_progress<F: Fn(ReindexProgress) + Send + Sync>(
    repo_root: &Path,
    ferret_dir: &Path,
    manager: &Arc<SegmentManager>,
    force_compact: bool,
    on_progress: F,
) -> Result<Vec<ChangeEvent>> {
```

Update the compaction decision block (lines 90-97) from:
```rust
        if manager.should_compact() {
```
to:
```rust
        if force_compact || manager.should_compact() {
```

Also: when `force_compact` is true but changes were empty (the `NoChanges` branch), we should still compact. Add after the `NoChanges` progress event (after line 68):
```rust
    if changes.is_empty() {
        on_progress(ReindexProgress::NoChanges);

        // Force compaction even with no changes if requested.
        if force_compact && !manager.snapshot().is_empty() {
            let snap = manager.snapshot();
            on_progress(ReindexProgress::CompactingSegments {
                input_segments: snap.len(),
            });
            drop(manager.compact_background());
            on_progress(ReindexProgress::Complete { changes_applied: 0 });
        }
    } else {
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-indexer-core -- test_catchup_force_compact`
Expected: PASS

**Step 5: Run all catchup tests to check no regressions**

Run: `cargo test -p ferret-indexer-core -- test_catchup`
Expected: All PASS (existing tests pass `false` via `run_catchup`).

**Step 6: Commit**

```bash
git add ferret-indexer-core/src/catchup.rs
git commit -m "feat(core): add force_compact param to run_catchup_with_progress"
```

---

### Task 3: Update daemon handler to pass `compact` through

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs:1338-1351`

**Step 1: Update the match arm**

Change line 1338 from:
```rust
            DaemonRequest::Reindex => {
```
to:
```rust
            DaemonRequest::Reindex { compact } => {
```

Change line 1347 from:
```rust
                    ferret_indexer_core::run_catchup_with_progress(&repo, &idir, &mgr, |ev| {
```
to:
```rust
                    ferret_indexer_core::run_catchup_with_progress(&repo, &idir, &mgr, compact, |ev| {
```

**Step 2: Verify compilation**

Run: `cargo check --workspace`
Expected: PASS

**Step 3: Commit**

```bash
git add ferret-indexer-cli/src/daemon.rs
git commit -m "feat(daemon): pass compact flag from Reindex request to catchup"
```

---

### Task 4: Add `--compact` CLI flag (mutually exclusive with `--full`)

**Files:**
- Modify: `ferret-indexer-cli/src/args.rs:166-171`
- Modify: `ferret-indexer-cli/src/main.rs:242-251`
- Modify: `ferret-indexer-cli/src/reindex_display.rs:15-28`

**Step 1: Add the flag to args.rs**

Change lines 166-171 from:
```rust
    /// Trigger reindex of the repository
    Reindex {
        /// Perform a full reindex (default: incremental)
        #[arg(long)]
        full: bool,
    },
```
to:
```rust
    /// Trigger reindex of the repository
    Reindex {
        /// Perform a full reindex (default: incremental)
        #[arg(long, conflicts_with = "compact")]
        full: bool,

        /// Force compaction after reindex
        #[arg(long, conflicts_with = "full")]
        compact: bool,
    },
```

**Step 2: Update main.rs dispatch**

Change lines 242-251 from:
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
to:
```rust
        Command::Reindex { full, compact } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            if full {
                // Full rebuild — same as init --force.
                init::run_init(&repo_root, true)?;
            } else {
                reindex_display::run_reindex_with_progress(&repo_root, compact).await?;
            }
            Ok(ExitCode::Success)
        }
```

**Step 3: Update reindex_display.rs to accept and send compact flag**

Change the function signature (line 15-17) from:
```rust
pub async fn run_reindex_with_progress(
    repo_root: &std::path::Path,
) -> Result<ExitCode, IndexError> {
```
to:
```rust
pub async fn run_reindex_with_progress(
    repo_root: &std::path::Path,
    compact: bool,
) -> Result<ExitCode, IndexError> {
```

Change lines 23-24 from:
```rust
    let json = serde_json::to_string(&DaemonRequest::Reindex)
```
to:
```rust
    let json = serde_json::to_string(&DaemonRequest::Reindex { compact })
```

**Step 4: Verify compilation**

Run: `cargo check --workspace`
Expected: PASS

**Step 5: Verify mutual exclusivity**

Run: `cargo run -p ferret-indexer-cli -- reindex --full --compact`
Expected: clap error: "the argument '--full' cannot be used with '--compact'"

**Step 6: Run all tests**

Run: `cargo test --workspace`
Expected: All PASS

**Step 7: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings

**Step 8: Commit**

```bash
git add ferret-indexer-cli/src/args.rs ferret-indexer-cli/src/main.rs ferret-indexer-cli/src/reindex_display.rs
git commit -m "feat(cli): add --compact flag to reindex command"
```

---

### Task 5: Verify end-to-end and run CI checks

**Step 1: Format check**

Run: `cargo fmt --all -- --check`
Expected: PASS

**Step 2: Clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: PASS

**Step 3: Full test suite**

Run: `cargo test --workspace`
Expected: All PASS
