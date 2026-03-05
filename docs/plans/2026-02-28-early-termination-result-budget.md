# Early Termination with Result Budget Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Propagate `max_results` into candidate verification loops so search stops as soon as enough results are found, instead of verifying all candidates and truncating afterward.

**Architecture:** The `SearchOptions.max_results` field already exists but is treated as post-hoc truncation. We'll thread a "remaining budget" through the two single-segment search functions (`search_single_segment_with_context` and `search_single_segment_with_pattern`) so they break out of their candidate loops early. Multi-segment functions will subtract found results from the budget before searching the next segment. Default remains `None` (unlimited) so CLI/daemon behavior is unchanged unless callers explicitly set a limit.

**Tech Stack:** Rust, existing `ferret-indexer-core` crate. No new dependencies.

---

### Task 1: Add early termination to `search_single_segment_with_context`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs:231-292` (the `search_single_segment_with_context` fn)

**Step 1: Write the failing test**

Add to the `tests` module in `multi_search.rs`:

```rust
#[test]
fn test_search_single_segment_early_termination() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // Build a segment with 5 files, all containing "println"
    let files: Vec<InputFile> = (0..5)
        .map(|i| InputFile {
            path: format!("file_{i}.rs"),
            content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
            mtime: 0,
        })
        .collect();

    let seg = build_segment(&base_dir, SegmentId(0), files);
    let tombstones = TombstoneSet::new();

    // Without limit: should find all 5
    let all = search_single_segment_with_context(&seg, "println", &tombstones, 0, None).unwrap();
    assert_eq!(all.len(), 5);

    // With limit=2: should find exactly 2
    let limited =
        search_single_segment_with_context(&seg, "println", &tombstones, 0, Some(2)).unwrap();
    assert_eq!(limited.len(), 2);
}
```

**Step 2: Run the test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_search_single_segment_early_termination -v`
Expected: FAIL — `search_single_segment_with_context` doesn't accept 5 args yet.

**Step 3: Write minimal implementation**

Change the signature of `search_single_segment_with_context` from:

```rust
fn search_single_segment_with_context(
    segment: &Segment,
    query: &str,
    tombstones: &TombstoneSet,
    context_lines: usize,
) -> Result<Vec<FileMatch>, IndexError> {
```

to:

```rust
fn search_single_segment_with_context(
    segment: &Segment,
    query: &str,
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
) -> Result<Vec<FileMatch>, IndexError> {
```

Add a budget check inside the candidate loop, right after `file_matches.push(...)` (after line 288):

```rust
        file_matches.push(FileMatch { /* ... existing code ... */ });

        // Early termination: stop once we have enough file results
        if let Some(max) = max_file_results {
            if file_matches.len() >= max {
                break;
            }
        }
```

Update the two call sites in the same file:
- `search_segments_with_options` (line 189): pass `None` for now (wired in Task 3)
- `test_search_single_segment_basic` etc: add `None` as the 5th argument to all existing test calls to `search_single_segment_with_context`

**Step 4: Run the test to verify it passes**

Run: `cargo test -p ferret-indexer-core -- test_search_single_segment -v`
Expected: ALL `search_single_segment` tests PASS, including the new early termination test.

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "feat(multi_search): add early termination to search_single_segment_with_context"
```

---

### Task 2: Add early termination to `search_single_segment_with_pattern`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs:335-450` (the `search_single_segment_with_pattern` fn)

**Step 1: Write the failing test**

Add to the `tests` module in `multi_search.rs`:

```rust
#[test]
fn test_search_single_segment_pattern_early_termination() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let files: Vec<InputFile> = (0..5)
        .map(|i| InputFile {
            path: format!("file_{i}.rs"),
            content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
            mtime: 0,
        })
        .collect();

    let seg = build_segment(&base_dir, SegmentId(0), files);
    let tombstones = TombstoneSet::new();
    let pattern = MatchPattern::Literal("println".to_string());

    // Without limit
    let all = search_single_segment_with_pattern(&seg, &pattern, &tombstones, 0, None).unwrap();
    assert_eq!(all.len(), 5);

    // With limit=3
    let limited =
        search_single_segment_with_pattern(&seg, &pattern, &tombstones, 0, Some(3)).unwrap();
    assert_eq!(limited.len(), 3);
}
```

**Step 2: Run the test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_search_single_segment_pattern_early_termination -v`
Expected: FAIL — wrong number of arguments.

**Step 3: Write minimal implementation**

Change the signature of `search_single_segment_with_pattern` from:

```rust
fn search_single_segment_with_pattern(
    segment: &Segment,
    pattern: &MatchPattern,
    tombstones: &TombstoneSet,
    context_lines: usize,
) -> Result<Vec<FileMatch>, IndexError> {
```

to:

```rust
fn search_single_segment_with_pattern(
    segment: &Segment,
    pattern: &MatchPattern,
    tombstones: &TombstoneSet,
    context_lines: usize,
    max_file_results: Option<usize>,
) -> Result<Vec<FileMatch>, IndexError> {
```

Add budget check after `file_matches.push(...)` (after line 446):

```rust
        file_matches.push(FileMatch { /* ... existing code ... */ });

        if let Some(max) = max_file_results {
            if file_matches.len() >= max {
                break;
            }
        }
```

Update all call sites to pass `None`:
- `search_segments_with_pattern` (line 479): pass `None` for now
- `search_segments_with_pattern_and_options` (line 535): pass `None` for now (wired in Task 4)

**Step 4: Run the test to verify it passes**

Run: `cargo test -p ferret-indexer-core -- test_search_single_segment_pattern -v`
Expected: ALL pattern-related tests PASS.

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "feat(multi_search): add early termination to search_single_segment_with_pattern"
```

---

### Task 3: Wire budget through `search_segments_with_options`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs:166-228` (the `search_segments_with_options` fn)

**Step 1: Write the failing test**

```rust
#[test]
fn test_search_segments_with_options_max_results_early_termination() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // Segment 0: 3 files with "println"
    let seg0 = build_segment(
        &base_dir,
        SegmentId(0),
        (0..3)
            .map(|i| InputFile {
                path: format!("a/file_{i}.rs"),
                content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 0,
            })
            .collect(),
    );

    // Segment 1: 3 more files with "println" (different paths)
    let seg1 = build_segment(
        &base_dir,
        SegmentId(1),
        (0..3)
            .map(|i| InputFile {
                path: format!("b/file_{i}.rs"),
                content: format!("fn g{i}() {{ println!(\"world\"); }}\n").into_bytes(),
                mtime: 0,
            })
            .collect(),
    );

    let snapshot: SegmentList = Arc::new(vec![seg0, seg1]);

    // Without limit: all 6
    let all = search_segments_with_options(
        &snapshot,
        "println",
        &SearchOptions {
            context_lines: 0,
            max_results: None,
        },
    )
    .unwrap();
    assert_eq!(all.total_file_count, 6);

    // With limit=2: exactly 2 files returned
    let limited = search_segments_with_options(
        &snapshot,
        "println",
        &SearchOptions {
            context_lines: 0,
            max_results: Some(2),
        },
    )
    .unwrap();
    assert_eq!(limited.files.len(), 2);
    assert_eq!(limited.total_file_count, 2);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_search_segments_with_options_max_results_early_termination -v`
Expected: FAIL — `total_file_count` will be 6 not 2 (budget not wired).

Note: this test may actually pass at the merge level since `search_segments_with_options` already has a `break` at line 202-206. But the budget isn't passed into the per-segment function, so it still does too much work. The test needs to verify via `total_file_count` reflecting the early-stopped count (not all 6).

**Step 3: Wire the budget**

In `search_segments_with_options`, replace the per-segment call and merge logic:

```rust
    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();

    for segment in snapshot.iter() {
        // Compute remaining budget for this segment
        let segment_budget = options
            .max_results
            .map(|max| max.saturating_sub(merged.len()));

        // Skip this segment entirely if budget is exhausted
        if segment_budget == Some(0) {
            break;
        }

        let tombstones = segment.load_tombstones()?;
        let file_matches = search_single_segment_with_context(
            segment,
            query,
            &tombstones,
            options.context_lines,
            segment_budget,
        )?;

        for fm in file_matches {
            let seg_id = segment.segment_id();
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {}
                _ => {
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
            if let Some(max) = options.max_results {
                if merged.len() >= max {
                    break;
                }
            }
        }
    }
```

The `total_file_count` and `total_match_count` are computed from the `files` vec, which now only contains the early-terminated set. This is correct — with a budget, these counts represent "results found" not "total matches in index."

**Step 4: Run tests**

Run: `cargo test -p ferret-indexer-core -- test_search_segments -v`
Expected: ALL `search_segments` tests PASS.

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "feat(multi_search): wire result budget through search_segments_with_options"
```

---

### Task 4: Wire budget through pattern-aware multi-segment functions

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs:460-575` (`search_segments_with_pattern` and `search_segments_with_pattern_and_options`)

**Step 1: Write the failing test**

```rust
#[test]
fn test_search_segments_with_pattern_and_options_early_termination() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        (0..5)
            .map(|i| InputFile {
                path: format!("file_{i}.rs"),
                content: format!("fn f{i}() {{ println!(\"hello\"); }}\n").into_bytes(),
                mtime: 0,
            })
            .collect(),
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);
    let pattern = MatchPattern::Literal("println".to_string());

    // Without limit
    let all = search_segments_with_pattern_and_options(
        &snapshot,
        &pattern,
        &SearchOptions {
            context_lines: 0,
            max_results: None,
        },
    )
    .unwrap();
    assert_eq!(all.total_file_count, 5);

    // With limit=2
    let limited = search_segments_with_pattern_and_options(
        &snapshot,
        &pattern,
        &SearchOptions {
            context_lines: 0,
            max_results: Some(2),
        },
    )
    .unwrap();
    assert_eq!(limited.files.len(), 2);
    assert_eq!(limited.total_file_count, 2);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core -- test_search_segments_with_pattern_and_options_early_termination -v`
Expected: FAIL — `total_file_count` is 5, not 2 (currently uses post-hoc truncation).

**Step 3: Wire the budget**

In `search_segments_with_pattern_and_options`, apply the same budget-threading pattern as Task 3:

```rust
    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();

    for segment in snapshot.iter() {
        let segment_budget = options
            .max_results
            .map(|max| max.saturating_sub(merged.len()));

        if segment_budget == Some(0) {
            break;
        }

        let tombstones = segment.load_tombstones()?;
        let file_matches = search_single_segment_with_pattern(
            segment,
            pattern,
            &tombstones,
            options.context_lines,
            segment_budget,
        )?;

        for fm in file_matches {
            let seg_id = segment.segment_id();
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {}
                _ => {
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
            if let Some(max) = options.max_results {
                if merged.len() >= max {
                    break;
                }
            }
        }
    }
```

Remove the old post-hoc truncation (delete lines 564-567):

```rust
    // DELETE THIS:
    // if let Some(max) = options.max_results {
    //     files.truncate(max);
    // }
```

Also make `search_segments_with_pattern` delegate to `search_segments_with_pattern_and_options` with defaults (DRY cleanup):

```rust
pub fn search_segments_with_pattern(
    snapshot: &SegmentList,
    pattern: &MatchPattern,
) -> Result<SearchResult, IndexError> {
    search_segments_with_pattern_and_options(snapshot, pattern, &SearchOptions::default())
}
```

This replaces the entire existing body of `search_segments_with_pattern` (lines 460-509).

**Step 4: Run all tests**

Run: `cargo test -p ferret-indexer-core -- multi_search -v`
Expected: ALL tests PASS.

**Step 5: Run clippy and fmt**

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`
Expected: Clean.

**Step 6: Commit**

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "feat(multi_search): wire result budget through pattern-aware search functions"
```

---

### Task 5: Full-suite verification

**Step 1: Run all workspace tests**

Run: `cargo test --workspace`
Expected: ALL pass.

**Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: Clean, no warnings.

**Step 3: Run fmt check**

Run: `cargo fmt --all -- --check`
Expected: No formatting issues.

**Step 4: Commit (if any fixups needed)**

```bash
git add -A
git commit -m "chore: fixups from full-suite verification"
```
