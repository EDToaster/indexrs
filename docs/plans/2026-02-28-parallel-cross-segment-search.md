# Parallel Cross-Segment Search Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Parallelize the outer segment loop in `search_segments_with_options()` and `search_segments_with_pattern_and_options()` so that multiple segments are searched concurrently via rayon, reducing search latency proportional to segment count.

**Architecture:** The current code iterates over segments sequentially in a `for` loop, searching each one and merging results into a `HashMap`. We switch to `rayon::par_iter()` over segments, collecting per-segment results into a `Vec`, then merge/dedup in a single-threaded post-pass. For `max_results` budget, we use a shared `AtomicUsize` counter that segments decrement as they produce matches, providing approximate early termination. The streaming search function (`search_segments_streaming`) is left sequential because it must preserve ordering for incremental output.

**Tech Stack:** rayon (already a dependency), `std::sync::atomic::AtomicUsize`

---

### Task 1: Parallelize `search_segments_with_options`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs:175-252` (the `search_segments_with_options` function)

**Step 1: Rewrite `search_segments_with_options` to use `par_iter`**

Replace the sequential `for segment in snapshot.iter()` loop with `rayon::par_iter()`. Each segment search produces a `Vec<(SegmentId, FileMatch)>`. After all segments finish, merge results in a single-threaded dedup pass (newest segment wins per path).

For `max_results` budget: use a shared `AtomicUsize` initialized to `max_results.unwrap_or(usize::MAX)`. Each segment decrements it as it produces matches. When the counter hits 0, segments skip remaining work. This is approximate (may slightly overshoot) but avoids contention.

The key change:

```rust
pub fn search_segments_with_options(
    snapshot: &SegmentList,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResult, IndexError> {
    let start = Instant::now();

    if snapshot.is_empty() || query.len() < 3 {
        return Ok(SearchResult {
            total_match_count: 0,
            total_file_count: 0,
            files: Vec::new(),
            duration: start.elapsed(),
        });
    }

    // Shared budget for approximate early termination across segments
    let budget = AtomicUsize::new(options.max_results.unwrap_or(usize::MAX));

    // Search all segments in parallel, collecting tagged results
    let per_segment_results: Vec<Result<Vec<(SegmentId, FileMatch)>, IndexError>> = snapshot
        .par_iter()
        .map(|segment| {
            // Check budget before doing expensive work
            if budget.load(Ordering::Relaxed) == 0 {
                return Ok(Vec::new());
            }

            let tombstones = segment.load_tombstones()?;
            let remaining = budget.load(Ordering::Relaxed);
            let segment_budget = if options.max_results.is_some() {
                Some(remaining)
            } else {
                None
            };

            let file_matches = search_single_segment_with_context(
                segment,
                query,
                &tombstones,
                options.context_lines,
                segment_budget,
            )?;

            let seg_id = segment.segment_id();
            let tagged: Vec<(SegmentId, FileMatch)> = file_matches
                .into_iter()
                .filter_map(|fm| {
                    // Decrement global budget
                    if options.max_results.is_some() {
                        let prev = budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                            if b > 0 { Some(b - 1) } else { None }
                        });
                        if prev.is_err() {
                            return None;
                        }
                    }
                    Some((seg_id, fm))
                })
                .collect();

            Ok(tagged)
        })
        .collect();

    // Merge results: dedup by path, newest segment wins
    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();
    for result in per_segment_results {
        for (seg_id, fm) in result? {
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {}
                _ => {
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
        }
    }

    // Sort by score descending, then path for stability
    let mut files: Vec<FileMatch> = merged.into_values().map(|(_, fm)| fm).collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    // Trim to max_results (parallel search may slightly overshoot)
    if let Some(max) = options.max_results {
        files.truncate(max);
    }

    let total_file_count = files.len();
    let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_match_count,
        total_file_count,
        files,
        duration: start.elapsed(),
    })
}
```

**Step 2: Run tests to verify**

Run: `cargo test -p ferret-indexer-core --lib -- multi_search`
Expected: All existing tests pass (the behavior is identical, just parallelized).

---

### Task 2: Parallelize `search_segments_with_pattern_and_options`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs:698-769` (the `search_segments_with_pattern_and_options` function)

**Step 1: Apply the same parallel pattern to the pattern-based search**

Same approach: `par_iter` over segments, collect tagged results, merge/dedup, trim.

```rust
pub fn search_segments_with_pattern_and_options(
    snapshot: &SegmentList,
    pattern: &MatchPattern,
    options: &SearchOptions,
) -> Result<SearchResult, IndexError> {
    let start = Instant::now();

    if snapshot.is_empty() {
        return Ok(SearchResult {
            total_match_count: 0,
            total_file_count: 0,
            files: Vec::new(),
            duration: start.elapsed(),
        });
    }

    // Shared budget for approximate early termination across segments
    let budget = AtomicUsize::new(options.max_results.unwrap_or(usize::MAX));

    // Search all segments in parallel
    let per_segment_results: Vec<Result<Vec<(SegmentId, FileMatch)>, IndexError>> = snapshot
        .par_iter()
        .map(|segment| {
            if budget.load(Ordering::Relaxed) == 0 {
                return Ok(Vec::new());
            }

            let tombstones = segment.load_tombstones()?;
            let remaining = budget.load(Ordering::Relaxed);
            let segment_budget = if options.max_results.is_some() {
                Some(remaining)
            } else {
                None
            };

            let file_matches = search_single_segment_with_pattern(
                segment,
                pattern,
                &tombstones,
                options.context_lines,
                segment_budget,
            )?;

            let seg_id = segment.segment_id();
            let tagged: Vec<(SegmentId, FileMatch)> = file_matches
                .into_iter()
                .filter_map(|fm| {
                    if options.max_results.is_some() {
                        let prev = budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                            if b > 0 { Some(b - 1) } else { None }
                        });
                        if prev.is_err() {
                            return None;
                        }
                    }
                    Some((seg_id, fm))
                })
                .collect();

            Ok(tagged)
        })
        .collect();

    // Merge: dedup by path, newest segment wins
    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();
    for result in per_segment_results {
        for (seg_id, fm) in result? {
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {}
                _ => {
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
        }
    }

    let mut files: Vec<FileMatch> = merged.into_values().map(|(_, fm)| fm).collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    if let Some(max) = options.max_results {
        files.truncate(max);
    }

    let total_file_count = files.len();
    let total_match_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_match_count,
        total_file_count,
        files,
        duration: start.elapsed(),
    })
}
```

**Step 2: Run all tests**

Run: `cargo test -p ferret-indexer-core --lib -- multi_search`
Expected: All existing tests pass.

---

### Task 3: Remove unused import, run CI checks, commit

**Step 1: Check for unused imports**

The `mpsc` import is only used by `search_segments_streaming` which stays sequential. Verify it is still needed. The `AtomicUsize` and `Ordering` imports are already present (line 9). No new imports needed.

**Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: No warnings.

**Step 3: Run fmt check**

Run: `cargo fmt --all -- --check`
Expected: No formatting issues.

**Step 4: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "perf: parallelize cross-segment search with rayon par_iter

Switch search_segments_with_options and search_segments_with_pattern_and_options
from sequential segment iteration to rayon par_iter. Each segment is searched
independently in parallel, results are collected, then merged/deduped in a
single-threaded post-pass (newest segment wins per path).

Uses a shared AtomicUsize budget counter for approximate early termination
when max_results is set. The streaming search function remains sequential
to preserve incremental output ordering."
```
