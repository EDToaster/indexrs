# Daemon Indexing Service Implementation Plan (HHC-83)

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Integrate `HybridDetector` into the daemon with checkpoint-based catch-up so the index stays fresh during active use and recovers cleanly after restart.

**Architecture:** The daemon starts serving queries immediately from the on-disk index, then runs catch-up in a background task (git-checkpoint fast path, hash-walk fallback). After catch-up, `HybridDetector` keeps the index live. A `stale` flag in the response protocol lets the CLI warn users on stderr. `ferret init` handles the initial full build.

**Tech Stack:** Rust, tokio (async), serde_json (checkpoint file), blake3 (content hashing), Unix domain sockets (daemon IPC)

---

### Task 1: Checkpoint persistence module

**Files:**
- Create: `ferret-indexer-core/src/checkpoint.rs`
- Modify: `ferret-indexer-core/src/lib.rs` (add `pub mod checkpoint; pub use checkpoint::...;`)
- Test: inline `#[cfg(test)] mod tests` in `checkpoint.rs`

**Step 1: Write failing tests for Checkpoint struct and read/write**

```rust
// ferret-indexer-core/src/checkpoint.rs

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{IndexError, Result};

/// Persisted index checkpoint for restart catch-up.
///
/// Written atomically (temp file + rename) after every successful
/// indexing operation. Read on daemon startup to determine what
/// changed since the last run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Schema version (always 1 for now).
    pub version: u32,
    /// SHA of the last indexed git commit, if available.
    pub git_commit: Option<String>,
    /// Unix epoch seconds when the checkpoint was written.
    pub indexed_at_epoch: u64,
    /// Number of files in the index at checkpoint time.
    pub file_count: u64,
}

impl Checkpoint {
    /// Create a new checkpoint with the current timestamp.
    pub fn new(git_commit: Option<String>, file_count: u64) -> Self {
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            version: 1,
            git_commit,
            indexed_at_epoch: epoch,
            file_count,
        }
    }
}

/// Read a checkpoint from `.ferret_index/checkpoint.json`.
///
/// Returns `Ok(None)` if the file does not exist.
/// Returns `Err` if the file exists but cannot be read or parsed.
pub fn read_checkpoint(ferret_dir: &Path) -> Result<Option<Checkpoint>> {
    let path = ferret_dir.join("checkpoint.json");
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path)?;
    let checkpoint: Checkpoint =
        serde_json::from_str(&data).map_err(|e| IndexError::Io(std::io::Error::other(e)))?;
    Ok(Some(checkpoint))
}

/// Write a checkpoint atomically to `.ferret_index/checkpoint.json`.
///
/// Uses temp-file-then-rename for crash safety.
pub fn write_checkpoint(ferret_dir: &Path, checkpoint: &Checkpoint) -> Result<()> {
    let path = ferret_dir.join("checkpoint.json");
    let tmp_path = ferret_dir.join("checkpoint.json.tmp");
    let data =
        serde_json::to_string_pretty(checkpoint).map_err(|e| IndexError::Io(std::io::Error::other(e)))?;
    std::fs::write(&tmp_path, data)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_new_sets_current_timestamp() {
        let cp = Checkpoint::new(Some("abc1234".to_string()), 100);
        assert_eq!(cp.version, 1);
        assert_eq!(cp.git_commit, Some("abc1234".to_string()));
        assert_eq!(cp.file_count, 100);
        // Timestamp should be recent (within last 10 seconds).
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(cp.indexed_at_epoch <= now);
        assert!(cp.indexed_at_epoch >= now - 10);
    }

    #[test]
    fn test_checkpoint_new_without_git() {
        let cp = Checkpoint::new(None, 0);
        assert_eq!(cp.git_commit, None);
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoint::new(Some("deadbeef1234567".to_string()), 42);
        write_checkpoint(dir.path(), &cp).unwrap();

        let loaded = read_checkpoint(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(
            loaded.git_commit,
            Some("deadbeef1234567".to_string())
        );
        assert_eq!(loaded.file_count, 42);
        assert_eq!(loaded.indexed_at_epoch, cp.indexed_at_epoch);
    }

    #[test]
    fn test_read_checkpoint_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_checkpoint(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_checkpoint_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("checkpoint.json"), "not json").unwrap();
        let result = read_checkpoint(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_write_checkpoint_atomic_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoint::new(None, 0);
        write_checkpoint(dir.path(), &cp).unwrap();
        assert!(dir.path().join("checkpoint.json").exists());
        // Temp file should be cleaned up by rename.
        assert!(!dir.path().join("checkpoint.json.tmp").exists());
    }

    #[test]
    fn test_write_checkpoint_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let cp1 = Checkpoint::new(Some("aaa1111".to_string()), 10);
        write_checkpoint(dir.path(), &cp1).unwrap();

        let cp2 = Checkpoint::new(Some("bbb2222".to_string()), 20);
        write_checkpoint(dir.path(), &cp2).unwrap();

        let loaded = read_checkpoint(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.git_commit, Some("bbb2222".to_string()));
        assert_eq!(loaded.file_count, 20);
    }
}
```

**Step 2: Run tests to verify they pass**

The struct, functions, and tests are all in one file. Write the complete file above.

Run: `cargo test -p ferret-indexer-core -- checkpoint`
Expected: All tests PASS.

**Step 3: Register module in lib.rs**

Add to `ferret-indexer-core/src/lib.rs` after the existing module declarations:

```rust
pub mod checkpoint;
pub use checkpoint::{Checkpoint, read_checkpoint, write_checkpoint};
```

**Step 4: Run full workspace check**

Run: `cargo check --workspace && cargo test -p ferret-indexer-core -- checkpoint`
Expected: All pass.

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/checkpoint.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(HHC-83): add checkpoint persistence module"
```

---

### Task 2: Staleness flag in daemon response protocol

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs:57-66` (add `stale` to `Done`)
- Modify: `ferret-indexer-cli/src/daemon.rs:83-119` (pass `caught_up` through daemon)
- Modify: `ferret-indexer-cli/src/daemon.rs:198-333` (pass `caught_up` to response)
- Modify: `ferret-indexer-cli/src/daemon.rs:377-428` (CLI prints warning on stale)
- Test: inline tests in `daemon.rs`

**Step 1: Add `stale` field to `DaemonResponse::Done`**

In `ferret-indexer-cli/src/daemon.rs`, modify the `Done` variant (line 61):

```rust
    /// End of results with summary.
    Done { total: usize, duration_ms: u64, stale: bool },
```

**Step 2: Add `caught_up` state to daemon**

Modify `start_daemon()` (line 83) to create and pass an `AtomicBool`:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub async fn start_daemon(repo_root: &Path) -> Result<(), IndexError> {
    let sock_path = socket_path(repo_root);

    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let _ = std::fs::remove_file(&sock_path);

    let listener = UnixListener::bind(&sock_path).map_err(IndexError::Io)?;

    let ferret_dir = repo_root.join(".ferret_index");
    let manager = std::sync::Arc::new(SegmentManager::new(&ferret_dir)?);

    // Start as not-caught-up. Will be flipped to true after background
    // catch-up completes (wired in a later task).
    let caught_up = Arc::new(AtomicBool::new(true));

    loop {
        match timeout(IDLE_TIMEOUT, listener.accept()).await {
            Ok(Ok((stream, _))) => {
                let mgr = manager.clone();
                let cu = caught_up.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &mgr, &cu).await {
                        eprintln!("daemon: connection error: {e}");
                    }
                });
            }
            Ok(Err(e)) => {
                eprintln!("daemon: accept error: {e}");
            }
            Err(_) => {
                let _ = std::fs::remove_file(&sock_path);
                return Ok(());
            }
        }
    }
}
```

**Step 3: Thread `caught_up` through `handle_connection` and responses**

Update `handle_connection` signature (line 198):

```rust
async fn handle_connection(
    stream: UnixStream,
    manager: &SegmentManager,
    caught_up: &AtomicBool,
) -> Result<(), IndexError> {
```

In every place that constructs `DaemonResponse::Done` inside `handle_connection`, add the stale flag. For Search (around line 269):

```rust
                        let resp = serde_json::to_string(&DaemonResponse::Done {
                            total: lines.len(),
                            duration_ms: elapsed.as_millis() as u64,
                            stale: !caught_up.load(Ordering::Relaxed),
                        })
```

For Files (around line 308):

```rust
                    let resp = serde_json::to_string(&DaemonResponse::Done {
                        total: lines.len(),
                        duration_ms: elapsed.as_millis() as u64,
                        stale: !caught_up.load(Ordering::Relaxed),
                    })
```

**Step 4: CLI prints warning on stale results**

In `run_via_daemon()` (around line 406), update the `Done` handler:

```rust
            DaemonResponse::Done { total, stale, .. } => {
                let _ = writer.finish();
                if stale {
                    eprintln!("warning: index is updating, results may be incomplete");
                }
                return Ok(if total == 0 {
                    ExitCode::NoResults
                } else {
                    ExitCode::Success
                });
            }
```

**Step 5: Update existing tests**

Update all test assertions that construct or match `DaemonResponse::Done` to include `stale: false`. For example, the `test_response_roundtrip_done` test:

```rust
    #[test]
    fn test_response_roundtrip_done() {
        let resp = DaemonResponse::Done {
            total: 42,
            duration_ms: 123,
            stale: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonResponse::Done { total, duration_ms, stale } => {
                assert_eq!(total, 42);
                assert_eq!(duration_ms, 123);
                assert!(!stale);
            }
            _ => panic!("expected Done variant"),
        }
    }
```

Add a new test for stale responses:

```rust
    #[test]
    fn test_response_roundtrip_done_stale() {
        let resp = DaemonResponse::Done {
            total: 10,
            duration_ms: 50,
            stale: true,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonResponse::Done { stale, .. } => assert!(stale),
            _ => panic!("expected Done variant"),
        }
    }
```

Update all other tests that match on `DaemonResponse::Done` to include the `stale` field — search for `DaemonResponse::Done` in the test module and add `stale` to each destructuring pattern (e.g., `DaemonResponse::Done { total, .. }`).

**Step 6: Run all tests**

Run: `cargo test -p ferret-indexer-cli`
Expected: All pass.

**Step 7: Commit**

```bash
git add ferret-indexer-cli/src/daemon.rs
git commit -m "feat(HHC-83): add stale flag to daemon response protocol"
```

---

### Task 3: Hash-based diff module

**Files:**
- Create: `ferret-indexer-core/src/hash_diff.rs`
- Modify: `ferret-indexer-core/src/lib.rs` (add module + re-export)
- Test: inline `#[cfg(test)] mod tests` in `hash_diff.rs`

This module walks the file tree, compares blake3 hashes against what's stored in segment metadata, and emits `ChangeEvent`s for anything that differs.

**Step 1: Write the module with tests**

```rust
// ferret-indexer-core/src/hash_diff.rs

//! Hash-based diff: compare on-disk files against indexed segment metadata.
//!
//! Walks the file tree, computes blake3 hashes, and compares against what's
//! stored in segment metadata. Emits [`ChangeEvent`]s for new, modified, and
//! deleted files. Used as a fallback when git-based catch-up is unavailable.

use std::collections::HashMap;
use std::path::Path;

use crate::binary::should_index_file;
use crate::changes::{ChangeEvent, ChangeKind};
use crate::error::Result;
use crate::index_state::SegmentList;
use crate::types::FileId;
use crate::walker::DirectoryWalkerBuilder;

/// Default maximum file size for indexing (1 MB).
const MAX_FILE_SIZE: u64 = 1_048_576;

/// Compare the on-disk file tree against indexed segments and return change events.
///
/// Walks `repo_root` using `DirectoryWalkerBuilder` (respects `.gitignore`),
/// computes blake3 hashes for each file, and compares against metadata in the
/// segment snapshot. Returns `ChangeEvent`s for:
/// - **Created**: file on disk but not in any segment
/// - **Modified**: file on disk with different hash than segment metadata
/// - **Deleted**: file in segment metadata but not on disk
pub fn hash_diff(repo_root: &Path, segments: &SegmentList) -> Result<Vec<ChangeEvent>> {
    // Build a map of path -> content_hash from all segments.
    let mut indexed: HashMap<String, [u8; 16]> = HashMap::new();
    for segment in segments.iter() {
        let reader = segment.metadata_reader()?;
        let tombstones = crate::tombstone::TombstoneSet::read_or_empty(
            &segment.dir_path().join("tombstones.bin"),
        )?;
        for entry in reader.iter_all() {
            let meta = entry?;
            if tombstones.contains(meta.file_id) {
                continue;
            }
            indexed.insert(meta.path.clone(), meta.content_hash);
        }
    }

    // Walk the file tree.
    let walker = DirectoryWalkerBuilder::new(repo_root).build();
    let walked = walker.run()?;

    let mut events = Vec::new();
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    for file in walked {
        let rel_path = match file.path.strip_prefix(repo_root) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let rel_str = rel_path.to_string_lossy().to_string();
        seen_paths.insert(rel_str.clone());

        // Read content and check indexability.
        let content = match std::fs::read(&file.path) {
            Ok(c) => c,
            Err(_) => continue, // File may have been deleted between walk and read.
        };
        if !should_index_file(&file.path, &content, MAX_FILE_SIZE) {
            continue;
        }

        // Compute hash.
        let hash = blake3::hash(&content);
        let mut hash_16 = [0u8; 16];
        hash_16.copy_from_slice(&hash.as_bytes()[..16]);

        match indexed.get(&rel_str) {
            None => {
                events.push(ChangeEvent {
                    path: rel_path.to_path_buf(),
                    kind: ChangeKind::Created,
                });
            }
            Some(existing_hash) if *existing_hash != hash_16 => {
                events.push(ChangeEvent {
                    path: rel_path.to_path_buf(),
                    kind: ChangeKind::Modified,
                });
            }
            _ => {} // Hash matches, file unchanged.
        }
    }

    // Find deleted files: in index but not on disk.
    for path_str in indexed.keys() {
        if !seen_paths.contains(path_str) {
            events.push(ChangeEvent {
                path: std::path::PathBuf::from(path_str),
                kind: ChangeKind::Deleted,
            });
        }
    }

    events.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{InputFile, SegmentWriter};
    use crate::types::SegmentId;
    use std::sync::Arc;

    /// Helper: build a segment from files, return a SegmentList snapshot.
    fn build_segment(
        segments_dir: &Path,
        id: u32,
        files: Vec<InputFile>,
    ) -> Arc<crate::segment::Segment> {
        let writer = SegmentWriter::new(segments_dir, SegmentId(id));
        let segment = writer.build(files).unwrap();
        Arc::new(segment)
    }

    #[test]
    fn test_hash_diff_empty_index_all_created() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        // Init git repo so walker respects .gitignore
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("a.rs"), "fn a() { let x = 1; }").unwrap();
        std::fs::write(repo.join("b.rs"), "fn b() { let y = 2; }").unwrap();

        let segments: SegmentList = Arc::new(vec![]);
        let events = hash_diff(repo, &segments).unwrap();

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.kind == ChangeKind::Created));
    }

    #[test]
    fn test_hash_diff_unchanged_files_no_events() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        let content = b"fn hello() { let x = 1; }";
        std::fs::write(repo.join("hello.rs"), content).unwrap();

        let seg_dir = repo.join(".ferret_index").join("segments");
        std::fs::create_dir_all(&seg_dir).unwrap();
        let segment = build_segment(
            &seg_dir,
            0,
            vec![InputFile {
                path: "hello.rs".to_string(),
                content: content.to_vec(),
                mtime: 100,
            }],
        );

        let segments: SegmentList = Arc::new(vec![segment]);
        let events = hash_diff(repo, &segments).unwrap();

        // File content matches — no events.
        assert!(
            events.is_empty(),
            "expected no events for unchanged file, got: {events:?}"
        );
    }

    #[test]
    fn test_hash_diff_modified_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        // Index with old content, disk has new content.
        let old_content = b"fn old() { let x = 1; }";
        let new_content = b"fn new() { let y = 2; }";
        std::fs::write(repo.join("file.rs"), new_content).unwrap();

        let seg_dir = repo.join(".ferret_index").join("segments");
        std::fs::create_dir_all(&seg_dir).unwrap();
        let segment = build_segment(
            &seg_dir,
            0,
            vec![InputFile {
                path: "file.rs".to_string(),
                content: old_content.to_vec(),
                mtime: 100,
            }],
        );

        let segments: SegmentList = Arc::new(vec![segment]);
        let events = hash_diff(repo, &segments).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ChangeKind::Modified);
        assert!(events[0].path.ends_with("file.rs"));
    }

    #[test]
    fn test_hash_diff_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        // File in index but not on disk.
        let seg_dir = repo.join(".ferret_index").join("segments");
        std::fs::create_dir_all(&seg_dir).unwrap();
        let segment = build_segment(
            &seg_dir,
            0,
            vec![InputFile {
                path: "gone.rs".to_string(),
                content: b"fn gone() { let x = 1; }".to_vec(),
                mtime: 100,
            }],
        );

        let segments: SegmentList = Arc::new(vec![segment]);
        let events = hash_diff(repo, &segments).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ChangeKind::Deleted);
        assert_eq!(events[0].path.to_string_lossy(), "gone.rs");
    }
}
```

**Step 2: Register module in lib.rs**

Add to `ferret-indexer-core/src/lib.rs`:

```rust
pub mod hash_diff;
pub use hash_diff::hash_diff;
```

**Step 3: Run tests**

Run: `cargo test -p ferret-indexer-core -- hash_diff`
Expected: All pass.

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/hash_diff.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(HHC-83): add hash-based diff module for catch-up fallback"
```

---

### Task 4: `ferret init` command

**Files:**
- Create: `ferret-indexer-cli/src/init.rs`
- Modify: `ferret-indexer-cli/src/args.rs:48-163` (add `Init` variant)
- Modify: `ferret-indexer-cli/src/main.rs:1` (add `mod init;`)
- Modify: `ferret-indexer-cli/src/main.rs:49-172` (add match arm)

**Step 1: Add `Init` variant to `Command` enum**

In `ferret-indexer-cli/src/args.rs`, add before the `Status` variant:

```rust
    /// Initialize the index for this repository (required before first search)
    Init {
        /// Rebuild the index from scratch even if one exists
        #[arg(long)]
        force: bool,
    },
```

**Step 2: Create `init.rs` module**

```rust
// ferret-indexer-cli/src/init.rs

use std::path::Path;
use std::time::Instant;

use ferret_indexer_core::checkpoint::{Checkpoint, read_checkpoint, write_checkpoint};
use ferret_indexer_core::error::IndexError;
use ferret_indexer_core::git_diff::GitChangeDetector;
use ferret_indexer_core::segment::InputFile;
use ferret_indexer_core::walker::DirectoryWalkerBuilder;
use ferret_indexer_core::{SegmentManager, should_index_file, DEFAULT_MAX_FILE_SIZE};

/// Run the `ferret init` command.
///
/// Walks the repo tree, builds the full index, and writes a checkpoint.
/// If `force` is false and an index already exists, returns an error.
pub fn run_init(repo_root: &Path, force: bool) -> Result<(), IndexError> {
    let ferret_dir = repo_root.join(".ferret_index");

    // Check for existing index unless --force.
    if !force {
        if let Ok(Some(_)) = read_checkpoint(&ferret_dir) {
            return Err(IndexError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "index already exists. Use --force to rebuild.",
            )));
        }
    }

    // If forcing, remove existing segments.
    if force {
        let segments_dir = ferret_dir.join("segments");
        if segments_dir.exists() {
            eprintln!("Removing existing index...");
            std::fs::remove_dir_all(&segments_dir)?;
        }
    }

    let start = Instant::now();
    eprintln!("Walking file tree...");

    // Walk the tree.
    let walker = DirectoryWalkerBuilder::new(repo_root).build();
    let walked = walker.run()?;

    // Collect indexable files.
    let mut files = Vec::new();
    for wf in &walked {
        let content = match std::fs::read(&wf.path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !should_index_file(&wf.path, &content, DEFAULT_MAX_FILE_SIZE) {
            continue;
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
        files.push(InputFile {
            path: rel_path,
            content,
            mtime,
        });
    }

    let file_count = files.len() as u64;
    eprintln!("Indexing {file_count} files...");

    // Build the index.
    let manager = SegmentManager::new(&ferret_dir)?;
    manager.index_files(files)?;

    // Write checkpoint.
    let git = GitChangeDetector::new(repo_root.to_path_buf());
    let git_commit = git.get_head_sha().ok();
    let checkpoint = Checkpoint::new(git_commit, file_count);
    write_checkpoint(&ferret_dir, &checkpoint)?;

    let elapsed = start.elapsed();
    eprintln!(
        "Done. Indexed {} files in {:.1}s.",
        file_count,
        elapsed.as_secs_f64()
    );
    Ok(())
}
```

**Step 3: Wire into main.rs**

Add `mod init;` to the top of `ferret-indexer-cli/src/main.rs`.

Add the match arm in the `run()` function (after `Command::Reindex`):

```rust
        Command::Init { force } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            init::run_init(&repo_root, force)?;
            Ok(ExitCode::Success)
        }
```

**Step 4: Verify `DEFAULT_MAX_FILE_SIZE` is re-exported**

Check if `DEFAULT_MAX_FILE_SIZE` is already exported from `ferret-indexer-core`. If not, add it to the `pub use binary::{...}` line in `ferret-indexer-core/src/lib.rs`.

**Step 5: Run workspace check**

Run: `cargo check --workspace`
Expected: Clean compile.

**Step 6: Manual smoke test**

Run: `cargo run -p ferret-indexer-cli -- init --repo .`
Expected: Walks the ferret repo, indexes files, prints progress to stderr, creates `.ferret_index/checkpoint.json`.

**Step 7: Commit**

```bash
git add ferret-indexer-cli/src/init.rs ferret-indexer-cli/src/args.rs ferret-indexer-cli/src/main.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(HHC-83): implement ferret init command"
```

---

### Task 5: Daemon background catch-up

**Files:**
- Create: `ferret-indexer-core/src/catchup.rs`
- Modify: `ferret-indexer-core/src/lib.rs` (add module + re-export)
- Modify: `ferret-indexer-cli/src/daemon.rs:83-119` (call catch-up on start)

This task adds a `run_catchup()` function to core that the daemon calls as a background task on startup.

**Step 1: Write the catch-up module with tests**

```rust
// ferret-indexer-core/src/catchup.rs

//! Catch-up logic for daemon startup.
//!
//! Detects changes since the last checkpoint using git (fast path) or
//! hash-based diff (fallback), applies them to the segment manager, and
//! writes a new checkpoint.

use std::path::Path;
use std::sync::Arc;

use crate::checkpoint::{Checkpoint, read_checkpoint, write_checkpoint};
use crate::changes::ChangeEvent;
use crate::error::Result;
use crate::git_diff::GitChangeDetector;
use crate::hash_diff::hash_diff;
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
    ferret_dir: &Path,
    manager: &Arc<SegmentManager>,
) -> Result<Vec<ChangeEvent>> {
    let checkpoint = read_checkpoint(ferret_dir)?;

    // Try git fast path.
    let changes = match try_git_catchup(repo_root, &checkpoint) {
        Some(Ok(events)) => {
            tracing::info!(
                event_count = events.len(),
                "catch-up via git diff"
            );
            events
        }
        Some(Err(e)) => {
            tracing::warn!(error = %e, "git catch-up failed, falling back to hash diff");
            run_hash_fallback(repo_root, manager)?
        }
        None => {
            tracing::info!("no git checkpoint, using hash diff fallback");
            run_hash_fallback(repo_root, manager)?
        }
    };

    if !changes.is_empty() {
        manager.apply_changes(repo_root, &changes)?;

        if manager.should_compact() {
            tracing::info!("compaction recommended after catch-up");
            let _ = manager.compact_background();
        }
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

/// Try git-based catch-up. Returns `None` if no git checkpoint available.
fn try_git_catchup(
    repo_root: &Path,
    checkpoint: &Option<Checkpoint>,
) -> Option<Result<Vec<ChangeEvent>>> {
    let cp = checkpoint.as_ref()?;
    let git_commit = cp.git_commit.as_ref()?;

    let mut git = GitChangeDetector::new(repo_root.to_path_buf());
    git.set_last_indexed_commit(git_commit.clone());
    Some(git.detect_changes())
}

/// Hash-based fallback: walk tree, compare hashes, return changes.
fn run_hash_fallback(
    repo_root: &Path,
    manager: &SegmentManager,
) -> Result<Vec<ChangeEvent>> {
    let snapshot = manager.snapshot();
    hash_diff(repo_root, &snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::InputFile;
    use std::fs;

    fn init_git_repo(path: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(path)
            .output()
            .unwrap();
    }

    #[test]
    fn test_catchup_no_checkpoint_uses_hash_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let ferret_dir = repo.join(".ferret_index");
        fs::create_dir_all(ferret_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&ferret_dir).unwrap());

        // Write a file on disk but don't index it.
        fs::write(repo.join("new.rs"), "fn new() { let x = 1; }").unwrap();

        let changes = run_catchup(repo, &ferret_dir, &manager).unwrap();

        assert!(
            changes.iter().any(|e| e.path.to_string_lossy().contains("new.rs")),
            "expected new.rs in changes, got: {changes:?}"
        );

        // Checkpoint should be written.
        let cp = read_checkpoint(&ferret_dir).unwrap();
        assert!(cp.is_some());
    }

    #[test]
    fn test_catchup_with_checkpoint_uses_git() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let ferret_dir = repo.join(".ferret_index");
        fs::create_dir_all(ferret_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&ferret_dir).unwrap());

        // Write a checkpoint with the current HEAD.
        let git = GitChangeDetector::new(repo.to_path_buf());
        let head = git.get_head_sha().unwrap();
        let cp = Checkpoint::new(Some(head), 0);
        write_checkpoint(&ferret_dir, &cp).unwrap();

        // Create an untracked file (will show in git ls-files).
        fs::write(repo.join("added.rs"), "fn added() { let x = 1; }").unwrap();

        let changes = run_catchup(repo, &ferret_dir, &manager).unwrap();

        assert!(
            changes.iter().any(|e| e.path.to_string_lossy().contains("added.rs")),
            "expected added.rs in changes, got: {changes:?}"
        );
    }

    #[test]
    fn test_catchup_no_changes_writes_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        let ferret_dir = repo.join(".ferret_index");
        fs::create_dir_all(ferret_dir.join("segments")).unwrap();
        let manager = Arc::new(SegmentManager::new(&ferret_dir).unwrap());

        // Write checkpoint at current HEAD, no changes.
        let git = GitChangeDetector::new(repo.to_path_buf());
        let head = git.get_head_sha().unwrap();
        let cp = Checkpoint::new(Some(head), 0);
        write_checkpoint(&ferret_dir, &cp).unwrap();

        let changes = run_catchup(repo, &ferret_dir, &manager).unwrap();
        assert!(changes.is_empty());

        // Checkpoint should still be present.
        let cp2 = read_checkpoint(&ferret_dir).unwrap();
        assert!(cp2.is_some());
    }
}
```

**Step 2: Register module in lib.rs**

Add to `ferret-indexer-core/src/lib.rs`:

```rust
pub mod catchup;
pub use catchup::run_catchup;
```

**Step 3: Run tests**

Run: `cargo test -p ferret-indexer-core -- catchup`
Expected: All pass.

**Step 4: Wire catch-up into daemon startup**

In `ferret-indexer-cli/src/daemon.rs`, modify `start_daemon()` to spawn a background catch-up task. After creating the manager and `caught_up` flag, before the accept loop:

```rust
    // Start as not-caught-up.
    let caught_up = Arc::new(AtomicBool::new(false));

    // Spawn background catch-up task.
    {
        let mgr = manager.clone();
        let cu = caught_up.clone();
        let repo = repo_root.to_path_buf();
        let idir = ferret_dir.clone();
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(move || {
                ferret_indexer_core::run_catchup(&repo, &idir, &mgr)
            })
            .await
            {
                Ok(Ok(changes)) => {
                    if !changes.is_empty() {
                        tracing::info!(
                            change_count = changes.len(),
                            "daemon catch-up applied changes"
                        );
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "daemon catch-up failed");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "daemon catch-up task panicked");
                }
            }
            cu.store(true, Ordering::SeqCst);
            tracing::info!("daemon catch-up complete");
        });
    }
```

**Step 5: Run all tests**

Run: `cargo test --workspace`
Expected: All pass.

**Step 6: Commit**

```bash
git add ferret-indexer-core/src/catchup.rs ferret-indexer-core/src/lib.rs ferret-indexer-cli/src/daemon.rs
git commit -m "feat(HHC-83): add daemon background catch-up on startup"
```

---

### Task 6: Wire HybridDetector into daemon

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs` (start HybridDetector after catch-up)

**Step 1: Start HybridDetector after catch-up completes**

Expand the background catch-up task in `start_daemon()` to start the `HybridDetector` and feed events to `apply_changes()`. Replace the catch-up spawn block from Task 5 with:

```rust
    // Spawn background catch-up + live indexing task.
    {
        let mgr = manager.clone();
        let cu = caught_up.clone();
        let repo = repo_root.to_path_buf();
        let idir = ferret_dir.clone();
        tokio::spawn(async move {
            // Phase 1: catch-up.
            match tokio::task::spawn_blocking({
                let repo = repo.clone();
                let idir = idir.clone();
                let mgr = mgr.clone();
                move || ferret_indexer_core::run_catchup(&repo, &idir, &mgr)
            })
            .await
            {
                Ok(Ok(changes)) => {
                    if !changes.is_empty() {
                        tracing::info!(
                            change_count = changes.len(),
                            "daemon catch-up applied changes"
                        );
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "daemon catch-up failed");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "daemon catch-up task panicked");
                }
            }
            cu.store(true, Ordering::SeqCst);
            tracing::info!("daemon catch-up complete, starting live watcher");

            // Phase 2: start HybridDetector for live changes.
            let detector_result = tokio::task::spawn_blocking({
                let repo = repo.clone();
                let idir = idir.clone();
                let mgr = mgr.clone();
                move || run_live_indexing(&repo, &idir, &mgr)
            })
            .await;

            match detector_result {
                Ok(Ok(())) => tracing::debug!("live indexing stopped"),
                Ok(Err(e)) => tracing::warn!(error = %e, "live indexing failed"),
                Err(e) => tracing::warn!(error = %e, "live indexing task panicked"),
            }
        });
    }
```

**Step 2: Add `run_live_indexing` function**

Add this function in `daemon.rs` (before `start_daemon`):

```rust
use ferret_indexer_core::checkpoint::{Checkpoint, write_checkpoint};
use ferret_indexer_core::git_diff::GitChangeDetector;
use ferret_indexer_core::HybridDetector;

/// Run the HybridDetector event loop, applying changes to the index.
///
/// Blocks the calling thread until the detector's channel disconnects
/// (which happens when the detector is dropped).
fn run_live_indexing(
    repo_root: &Path,
    ferret_dir: &Path,
    manager: &Arc<SegmentManager>,
) -> Result<(), IndexError> {
    let mut detector = HybridDetector::new(repo_root.to_path_buf())?;
    let rx = detector.start()?;

    for batch in rx.iter() {
        if batch.is_empty() {
            continue;
        }
        tracing::debug!(event_count = batch.len(), "applying live change batch");

        if let Err(e) = manager.apply_changes(repo_root, &batch) {
            tracing::warn!(error = %e, "failed to apply live changes");
            continue;
        }

        // Update checkpoint.
        let git = GitChangeDetector::new(repo_root.to_path_buf());
        let git_commit = git.get_head_sha().ok();
        let snapshot = manager.snapshot();
        let file_count: u64 = snapshot.iter().map(|s| s.entry_count() as u64).sum();
        let cp = Checkpoint::new(git_commit, file_count);
        if let Err(e) = write_checkpoint(ferret_dir, &cp) {
            tracing::warn!(error = %e, "failed to update checkpoint");
        }

        // Check if compaction needed.
        if manager.should_compact() {
            tracing::info!("compaction triggered by live changes");
            let _ = manager.compact_background();
        }
    }

    detector.stop();
    Ok(())
}
```

**Step 3: Run workspace tests and clippy**

Run: `cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: All pass.

**Step 4: Commit**

```bash
git add ferret-indexer-cli/src/daemon.rs
git commit -m "feat(HHC-83): wire HybridDetector into daemon for live indexing"
```

---

### Task 7: `ferret reindex` implementation

**Files:**
- Modify: `ferret-indexer-cli/src/daemon.rs` (add `Reindex` request type + handler)
- Modify: `ferret-indexer-cli/src/main.rs:162-165` (implement reindex command)

**Step 1: Add `Reindex` variant to `DaemonRequest`**

In `daemon.rs`, add to the `DaemonRequest` enum:

```rust
    Reindex,
```

**Step 2: Handle `Reindex` in `handle_connection`**

In the match arm inside `handle_connection`, add before the closing `line.clear()`:

```rust
            DaemonRequest::Reindex => {
                // Trigger a catch-up cycle by spawning it in the background.
                // The caller gets an immediate acknowledgment.
                let mgr_clone = manager.clone();
                let repo = manager.base_dir().parent().unwrap_or(Path::new(".")).to_path_buf();
                let idir = manager.base_dir().to_path_buf();
                tokio::spawn(async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        ferret_indexer_core::run_catchup(&repo, &idir, &Arc::new(/* ... */))
                    }).await;
                });
                // For simplicity, just respond Done immediately.
                let resp = serde_json::to_string(&DaemonResponse::Done {
                    total: 0,
                    duration_ms: 0,
                    stale: true,
                })
                .unwrap();
                writer
                    .write_all(format!("{resp}\n").as_bytes())
                    .await
                    .map_err(IndexError::Io)?;
            }
```

**Note:** This step requires `SegmentManager` to expose its `base_dir`. The manager is passed as `&SegmentManager` but we need `Arc<SegmentManager>` for `run_catchup`. The cleanest approach: change `handle_connection` to take `&Arc<SegmentManager>` instead. This requires small signature updates through the call chain.

Alternatively, for a simpler first pass: the `Reindex` command can call `HybridDetector::reindex()` on a stored detector handle. This depends on how the daemon stores the detector — if it's in the background task, a shared `Arc<AtomicBool>` reindex flag is the simplest approach.

**Simplest approach:** Store the `HybridDetector`'s reindex flag as an `Arc<AtomicBool>` in `start_daemon`, expose it to `handle_connection`. When `Reindex` arrives, flip the flag:

In `start_daemon`, extract the reindex flag before spawning:

```rust
    let reindex_flag = Arc::new(AtomicBool::new(false));
```

Pass it to the live indexing task (which checks it before each git poll), and also pass it to `handle_connection`:

```rust
async fn handle_connection(
    stream: UnixStream,
    manager: &SegmentManager,
    caught_up: &AtomicBool,
    reindex_flag: &AtomicBool,
) -> Result<(), IndexError> {
```

Then the `Reindex` handler is simply:

```rust
            DaemonRequest::Reindex => {
                reindex_flag.store(true, Ordering::SeqCst);
                let resp = serde_json::to_string(&DaemonResponse::Pong).unwrap();
                writer
                    .write_all(format!("{resp}\n").as_bytes())
                    .await
                    .map_err(IndexError::Io)?;
            }
```

**Step 3: Implement CLI `reindex` command in main.rs**

Replace the `Command::Reindex` stub in `main.rs`:

```rust
        Command::Reindex { full } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            if full {
                // Full rebuild — same as init --force.
                init::run_init(&repo_root, true)?;
            } else {
                // Send Reindex request to daemon.
                let request = daemon::DaemonRequest::Reindex;
                let stdout = std::io::stdout();
                let mut writer = StreamingWriter::new(stdout.lock());
                daemon::run_via_daemon(&repo_root, request, &mut writer).await?;
                eprintln!("Reindex triggered.");
            }
            Ok(ExitCode::Success)
        }
```

**Step 4: Add test for Reindex request serialization**

```rust
    #[test]
    fn test_request_roundtrip_reindex() {
        let req = DaemonRequest::Reindex;
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, DaemonRequest::Reindex));
    }
```

**Step 5: Add "no index" guard to `ensure_daemon` path**

In `main.rs`, before the `Command::Search` and `Command::Files` handlers call `run_via_daemon`, add a check:

```rust
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            if !repo_root.join(".ferret_index").join("segments").exists() {
                eprintln!("error: no index found. Run 'ferret init' first.");
                return Ok(ExitCode::Error);
            }
```

**Step 6: Run all tests**

Run: `cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: All pass.

**Step 7: Commit**

```bash
git add ferret-indexer-cli/src/daemon.rs ferret-indexer-cli/src/main.rs
git commit -m "feat(HHC-83): implement ferret reindex command and no-index guard"
```

---

## Task Dependency Summary

```
Task 1 (checkpoint) ──┬──> Task 3 (hash diff) ──> Task 5 (catch-up) ──> Task 6 (HybridDetector)
                       │                                                         │
                       └──> Task 4 (init) ──────────────────────────> Task 7 (reindex)
                       │
Task 2 (stale flag) ───┘
```

Tasks 1, 2 can be done in parallel. Tasks 3, 4 can be done in parallel after 1. Tasks 5 depends on 1+3. Task 6 depends on 5. Task 7 depends on 4+6.
