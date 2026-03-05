# HHC-81: Parallel Candidate Verification with Rayon

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Parallelize candidate verification in the single-segment search functions using `rayon::par_iter()` to utilize multiple CPU cores during content decompression and matching.

**Architecture:** The current search loop in `search_single_segment_with_context` and `search_single_segment_with_pattern` processes candidates sequentially. Each candidate requires zstd decompression + substring/regex matching -- CPU-bound work that parallelizes well. We add `rayon` as a dependency to `ferret-indexer-core`, convert the candidate verification loops to use `rayon::par_iter()`, and use `AtomicUsize` for the result budget so early termination still works across threads. The mmap-backed readers (`ContentStoreReader`, `TrigramIndexReader`, `MetadataReader`) are `Send + Sync` since `Mmap` is `Send + Sync`, so concurrent access is safe.

**Tech Stack:** Rust, `rayon` crate. Modifies `ferret-indexer-core` only.

**Key Design Decisions:**
1. **AtomicUsize for budget:** Replace the sequential `max_file_results` counter with an `AtomicUsize` that threads decrement atomically. Threads check the budget before starting verification and skip work when exhausted.
2. **Parallel collect then sequential merge:** Use `par_iter().filter_map().collect()` to produce a `Vec<FileMatch>` in parallel, then truncate to budget if overshot (race between check and decrement is benign -- we may get a few extra results).
3. **No parallelism for small candidate sets:** The overhead of rayon's work-stealing is not worth it for very few candidates. We use a threshold (e.g., 64 candidates) below which we fall back to sequential iteration.

---

### Task 1: Add `rayon` dependency to `ferret-indexer-core`

**Files:**
- Modify: `ferret-indexer-core/Cargo.toml`

**Steps:**
1. Add `rayon = "1"` to `[dependencies]` in `ferret-indexer-core/Cargo.toml`.
2. Run `cargo check --workspace` to confirm it compiles.

---

### Task 2: Parallelize `search_single_segment_with_context`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs`

**Step 1: Add imports**

At the top of `multi_search.rs`, add:
```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use rayon::prelude::*;
```

**Step 2: Rewrite `search_single_segment_with_context`**

Replace the sequential candidate loop with a parallel one. The function currently iterates over `candidates`, skips tombstoned entries, reads metadata, decompresses content, verifies matches, scores, and collects results with early termination.

New approach:
```rust
fn search_single_segment_with_context(
    segment: &Segment,
    query: &str,
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
) -> Result<Vec<FileMatch>, IndexError> {
    let candidates = find_candidates(segment.trigram_reader(), query)?;

    // For small candidate sets, skip rayon overhead
    const PAR_THRESHOLD: usize = 64;

    if candidates.len() < PAR_THRESHOLD {
        return search_single_segment_with_context_seq(
            segment, query, tombstones, context_lines, max_file_results, candidates,
        );
    }

    // Atomic budget counter for cross-thread early termination
    let budget = AtomicUsize::new(max_file_results.unwrap_or(usize::MAX));

    let file_matches: Vec<FileMatch> = candidates
        .par_iter()
        .filter_map(|&file_id| {
            // Check budget before doing expensive work
            if budget.load(Ordering::Relaxed) == 0 {
                return None;
            }

            if tombstones.contains(file_id) {
                return None;
            }

            let meta = segment.get_metadata(file_id).ok()??;

            let content = segment
                .content_reader()
                .read_content(meta.content_offset, meta.content_len)
                .ok()?;

            let line_matches = verify_content_matches(&content, query, context_lines);
            if line_matches.is_empty() {
                return None;
            }

            // Decrement budget; if already 0, discard this result
            if budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                if b > 0 { Some(b - 1) } else { None }
            }).is_err() {
                return None;
            }

            // Compute relevance score
            let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let input = ScoringInput {
                path: &meta.path,
                query,
                match_type: MatchType::Substring,
                match_count: total_match_ranges,
                line_count: meta.line_count,
                mtime_epoch_secs: meta.mtime_epoch_secs,
                now_epoch_secs: now,
            };
            let config = RankingConfig::default();
            let score = score_file_match(&input, &config);

            Some(FileMatch {
                file_id,
                path: PathBuf::from(&meta.path),
                language: meta.language,
                lines: line_matches,
                score,
            })
        })
        .collect();

    Ok(file_matches)
}
```

**Step 3: Extract sequential fallback**

Extract the old sequential loop into a private helper `search_single_segment_with_context_seq` that takes the candidates vector. This is called for small candidate sets (< PAR_THRESHOLD).

```rust
fn search_single_segment_with_context_seq(
    segment: &Segment,
    query: &str,
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
    candidates: Vec<FileId>,
) -> Result<Vec<FileMatch>, IndexError> {
    // ... existing sequential code ...
}
```

**Step 4: Verify tests pass**

Run: `cargo test -p ferret-indexer-core -- search_single_segment`

---

### Task 3: Parallelize `search_single_segment_with_pattern`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs`

**Steps:**

Apply the same parallel pattern to `search_single_segment_with_pattern`. This function uses `ContentVerifier` instead of `verify_content_matches`. Key difference: `ContentVerifier` contains a compiled `Regex` which is `Send + Sync` (the `regex` crate's `Regex` is thread-safe).

The approach is identical to Task 2:
1. Compute candidates via trigram lookup (or full scan for regex with short prefix).
2. If candidates < PAR_THRESHOLD, call sequential fallback.
3. Otherwise, use `par_iter()` with `AtomicUsize` budget.
4. Note: `ContentVerifier` needs to be shared across threads. Since it is `Send + Sync` (its fields are `MatchPattern` which is `Clone + Send + Sync`, a `u32`, and `Option<Regex>` which is `Send + Sync`), we can use a `&ContentVerifier` reference directly.

```rust
fn search_single_segment_with_pattern(
    segment: &Segment,
    pattern: &MatchPattern,
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
) -> Result<Vec<FileMatch>, IndexError> {
    // ... trigram query and candidate computation (unchanged) ...

    let verifier = ContentVerifier::new(pattern.clone(), context_lines as u32);

    const PAR_THRESHOLD: usize = 64;

    if candidates.len() < PAR_THRESHOLD {
        return search_single_segment_with_pattern_seq(
            segment, pattern, &verifier, tombstones, context_lines, max_file_results, candidates,
        );
    }

    let budget = AtomicUsize::new(max_file_results.unwrap_or(usize::MAX));

    let file_matches: Vec<FileMatch> = candidates
        .par_iter()
        .filter_map(|&file_id| {
            if budget.load(Ordering::Relaxed) == 0 {
                return None;
            }

            if tombstones.contains(file_id) {
                return None;
            }

            let meta = segment.get_metadata(file_id).ok()??;
            let content = segment
                .content_reader()
                .read_content(meta.content_offset, meta.content_len)
                .ok()?;

            let line_matches = if context_lines > 0 {
                let blocks = verifier.verify_with_context(&content);
                if blocks.is_empty() { return None; }
                // Flatten ContextBlocks into LineMatches (same as current code)
                // ...
            } else {
                let matches = verifier.verify(&content);
                if matches.is_empty() { return None; }
                matches
            };

            if budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                if b > 0 { Some(b - 1) } else { None }
            }).is_err() {
                return None;
            }

            // ... scoring code (same as current) ...
            Some(file_match)
        })
        .collect();

    Ok(file_matches)
}
```

**Verify:** `cargo test -p ferret-indexer-core -- search_single_segment_with_pattern`

---

### Task 4: Add parallel-specific tests

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs` (test module)

**Tests to add:**

1. **`test_parallel_search_many_candidates`**: Create a segment with 200+ files all containing the search term. Verify that the parallel path is exercised (candidates > PAR_THRESHOLD) and all matches are found.

2. **`test_parallel_search_with_budget`**: Same as above but with `max_file_results = Some(10)`. Verify that at most 10 results are returned despite 200+ matching candidates.

3. **`test_parallel_search_pattern_many_candidates`**: Same as test 1 but using `search_segments_with_pattern` to exercise the pattern path.

```rust
#[test]
fn test_parallel_search_many_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let files: Vec<InputFile> = (0..200)
        .map(|i| InputFile {
            path: format!("file_{i:03}.rs"),
            content: format!("fn func_{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
            mtime: 1700000000 + i as u64,
        })
        .collect();

    let seg = build_segment(&base_dir, SegmentId(0), files);
    let snapshot: SegmentList = Arc::new(vec![seg]);
    let result = search_segments(&snapshot, "println").unwrap();
    assert_eq!(result.total_file_count, 200);
}

#[test]
fn test_parallel_search_with_budget() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let files: Vec<InputFile> = (0..200)
        .map(|i| InputFile {
            path: format!("file_{i:03}.rs"),
            content: format!("fn func_{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
            mtime: 1700000000 + i as u64,
        })
        .collect();

    let seg = build_segment(&base_dir, SegmentId(0), files);
    let snapshot: SegmentList = Arc::new(vec![seg]);
    let options = SearchOptions {
        context_lines: 0,
        max_results: Some(10),
    };
    let result = search_segments_with_options(&snapshot, "println", &options).unwrap();
    assert!(result.total_file_count <= 10);
}
```

---

### Task 5: Run full validation

**Steps:**
1. `cargo check --workspace` -- type-check all crates
2. `cargo test --workspace` -- all tests pass
3. `cargo clippy --workspace -- -D warnings` -- no lint warnings
4. `cargo fmt --all -- --check` -- formatting correct

---

### Risk Assessment

- **Thread safety:** `Mmap` is `Send + Sync`, so `ContentStoreReader`, `TrigramIndexReader`, and `Segment` can be safely shared across rayon threads. `ContentVerifier` holds `MatchPattern` (enum of Strings, all `Send + Sync`) and `Option<Regex>` (`Regex` is `Send + Sync`).
- **Budget overshoot:** The `AtomicUsize` budget may allow a few extra results due to the race between the budget check and the decrement. This is benign -- the multi-segment merge layer already handles truncation.
- **Error handling:** In the parallel path, `get_metadata` and `read_content` errors are silently skipped (mapped to `None`). This matches the current behavior for corrupted entries and is acceptable since these are rare edge cases. If we wanted strict error propagation, we could collect `Result<Option<FileMatch>>` and check for errors, but this adds complexity for minimal benefit.
- **PAR_THRESHOLD:** Set to 64 to avoid rayon overhead for small searches. This is a conservative default that can be tuned later.
