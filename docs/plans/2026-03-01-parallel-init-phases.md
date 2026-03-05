# Parallel Init Phases Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Parallelize Phase 1 (file tree walk) and Phase 2 (filter & load file contents) of `ferret init` to reduce wall-clock time on large repos.

**Architecture:** Phase 1 adds a `run_parallel_with_progress` method to `Walker` that uses `ignore`'s `build_parallel()` with an `AtomicUsize` file counter and a `Fn + Sync` callback. Phase 2 replaces the sequential filter-and-read loop in `init.rs` with a rayon `par_iter` over the walked files, using atomic counters for skip stats and progress. All existing output (progress lines, skip breakdown, summary) is preserved identically.

**Tech Stack:** Rust, rayon (already in `ferret-indexer-core`), `ignore` crate's parallel walker, `std::sync::atomic`

---

### Task 1: Add `run_parallel_with_progress` to Walker

**Files:**
- Modify: `ferret-indexer-core/src/walker.rs:190-229`
- Test: `ferret-indexer-core/src/walker.rs` (inline tests)

**Step 1: Write the failing test**

Add a test in the `walker.rs` inline test module that calls `run_parallel_with_progress` (which doesn't exist yet):

```rust
#[test]
fn test_parallel_walk_with_progress_reports_count() {
    let tmp = TempDir::new().unwrap();
    create_file(tmp.path(), "a.rs", "fn a() {}");
    create_file(tmp.path(), "b.rs", "fn b() {}");
    create_file(tmp.path(), "sub/c.rs", "fn c() {}");

    let max_count = std::sync::atomic::AtomicUsize::new(0);
    let files = DirectoryWalkerBuilder::new(tmp.path())
        .threads(2)
        .build()
        .run_parallel_with_progress(|count| {
            max_count.store(count, std::sync::atomic::Ordering::Relaxed);
        })
        .unwrap();

    assert_eq!(files.len(), 3);
    assert_eq!(max_count.load(std::sync::atomic::Ordering::Relaxed), 3);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_parallel_walk_with_progress_reports_count`
Expected: compile error — `run_parallel_with_progress` does not exist

**Step 3: Implement `run_parallel_with_progress`**

Add this method to the `impl Walker` block, after `run_parallel`:

```rust
/// Walk the directory tree in parallel, calling `on_file(count)` after
/// each file is discovered (where `count` is the running total).
///
/// Like [`run_parallel`](Self::run_parallel) but with a progress callback.
/// The callback receives the approximate running file count and may be
/// called from multiple threads concurrently, so it must be `Fn + Sync`.
/// Files are returned in arbitrary order (non-deterministic).
pub fn run_parallel_with_progress<F: Fn(usize) + Sync>(
    self,
    on_file: F,
) -> Result<Vec<WalkedFile>> {
    let files: Mutex<Vec<WalkedFile>> = Mutex::new(Vec::new());
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let count = std::sync::atomic::AtomicUsize::new(0);

    self.builder.build_parallel().run(|| {
        Box::new(|entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    errors.lock().unwrap().push(e.to_string());
                    return WalkState::Continue;
                }
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    errors.lock().unwrap().push(e.to_string());
                    return WalkState::Continue;
                }
            };
            files.lock().unwrap().push(WalkedFile {
                path: entry.into_path(),
                metadata,
            });
            let current = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            on_file(current);
            WalkState::Continue
        })
    });

    let errs = errors.into_inner().unwrap();
    if !errs.is_empty() {
        return Err(IndexError::Walk(errs.join("; ")));
    }
    Ok(files.into_inner().unwrap())
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-indexer-core -- test_parallel_walk_with_progress`
Expected: PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/walker.rs
git commit -m "feat: add run_parallel_with_progress to Walker"
```

---

### Task 2: Switch init Phase 1 to parallel walk with progress

**Files:**
- Modify: `ferret-indexer-cli/src/init.rs:130-149`

**Step 1: Update Phase 1 to use `run_parallel_with_progress`**

Replace the Phase 1 block (lines 130–149) in `run_init`. The progress callback must be `Fn + Sync`, so wrap `progress` in a `Mutex` for this phase (same pattern as Phase 3):

```rust
// ── Phase 1: Walk the file tree ──────────────────────────────────
let walk_start = Instant::now();
progress.update("Walking file tree...");

let walker = DirectoryWalkerBuilder::new(repo_root).build();
let progress = std::sync::Mutex::new(progress);
let walked = walker.run_parallel_with_progress(|count| {
    if count % step == 0 {
        progress.lock().unwrap().update(&format!(
            "Walking file tree... {} files found",
            fmt_count(count)
        ));
    }
})?;
let mut progress = progress.into_inner().unwrap();

let walk_elapsed = walk_start.elapsed();
progress.finish(&format!(
    "Walking file tree... {} files found ({:.1}s)",
    fmt_count(walked.len()),
    walk_elapsed.as_secs_f64()
));
```

**Step 2: Run clippy and tests**

Run: `cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: clean clippy, all tests pass

**Step 3: Commit**

```bash
git add ferret-indexer-cli/src/init.rs
git commit -m "perf: parallelize init Phase 1 file tree walk"
```

---

### Task 3: Parallelize init Phase 2 (filter & load)

**Files:**
- Modify: `ferret-indexer-cli/Cargo.toml` (add `rayon` dep)
- Modify: `ferret-indexer-cli/src/init.rs:151-211`

**Step 1: Add rayon dependency to ferret-indexer-cli**

In `ferret-indexer-cli/Cargo.toml`, add under `[dependencies]`:

```toml
rayon = "1"
```

**Step 2: Replace the sequential Phase 2 loop**

Replace the Phase 2 block (lines 151–211 of the original file, between the walk finish and the skip-breakdown printing) with a parallel version. Use `rayon::par_iter` + `filter_map` + atomic counters:

```rust
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

let progress = std::sync::Mutex::new(progress);
let files: Vec<InputFile> = walked
    .par_iter()
    .filter_map(|wf| {
        let current = filter_done.fetch_add(1, Ordering::Relaxed) + 1;
        if current % step == 0 || current == total_walked {
            let pct = (current as f64 / total_walked as f64 * 100.0) as u32;
            progress.lock().unwrap().update(&format!(
                "Filtering files... {}/{} ({pct}%)",
                fmt_count(current),
                fmt_count(total_walked),
            ));
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
let mut progress = progress.into_inner().unwrap();

let skipped_size = skipped_size.load(Ordering::Relaxed);
let skipped_binary = skipped_binary.load(Ordering::Relaxed);
let skipped_content = skipped_content.load(Ordering::Relaxed);
let skipped_read_err = skipped_read_err.load(Ordering::Relaxed);
let total_content_bytes = total_content_bytes.load(Ordering::Relaxed);
```

The rest of the function (skip breakdown printing, Phase 3, Phase 4, summary) stays exactly the same — those lines already reference `skipped_size`, `skipped_binary`, etc. by value, which now come from the atomic loads above.

**Step 3: Run clippy and tests**

Run: `cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: clean clippy, all tests pass

**Step 4: Commit**

```bash
git add ferret-indexer-cli/Cargo.toml ferret-indexer-cli/src/init.rs
git commit -m "perf: parallelize init Phase 2 file filtering and loading"
```

---

### Task 4: End-to-end manual verification

**Step 1: Build in release mode**

Run: `cargo build --workspace --release`

**Step 2: Test on a real repo**

Run `ferret init` on a real codebase and verify:
1. Phase 1 shows live file count updating during walk
2. Phase 2 shows percentage progress during filtering
3. Phase 3 shows percentage progress during index build (already working)
4. Skip breakdown prints correctly
5. Summary line prints correct totals

Run:
```bash
cargo run -p ferret-indexer-cli --release -- init --force
```

**Step 3: Verify non-TTY output**

Pipe through cat to test non-TTY mode:
```bash
cargo run -p ferret-indexer-cli --release -- init --force 2>&1 | cat
```

Verify each phase prints separate lines instead of `\r`-overwriting.

**Step 4: Final clippy + test + fmt**

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```
