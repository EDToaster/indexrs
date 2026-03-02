# Search Hot Path P1 Performance Improvements

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Eliminate per-candidate overhead in the search hot path — cached MetadataReader, SIMD newline scanning, shared LineIndex, pre-allocated decompression buffers, and hoisted per-search constants.

**Architecture:** Five independent optimizations targeting repeated work inside the per-candidate verification loop. Each change is self-contained with no cross-dependencies, so they can be committed independently. All changes are internal (no public API changes).

**Tech Stack:** Rust, memchr crate (SIMD-accelerated byte search), zstd

---

### Task 1: Add `memchr` dependency

**Files:**
- Modify: `indexrs-core/Cargo.toml`

**Step 1: Add the memchr dependency**

Add `memchr = "2"` to `[dependencies]` in `indexrs-core/Cargo.toml`, after the `blake3` line:

```toml
memchr = "2"
```

**Step 2: Verify it compiles**

Run: `cargo check -p indexrs-core`
Expected: compiles successfully

**Step 3: Commit**

```bash
git add indexrs-core/Cargo.toml
git commit -m "chore: add memchr dependency for SIMD-accelerated byte search"
```

---

### Task 2: Use `memchr::memchr_iter` for newline scanning in LineIndex

The `LineIndex::new()` in `verify.rs:23-29` uses `.iter().enumerate().filter().map().collect()` to find newlines — generic iterator overhead. `memchr::memchr_iter(b'\n', content)` uses SIMD (AVX2/NEON) and is significantly faster.

**Files:**
- Modify: `indexrs-core/src/verify.rs:1-34`
- Test: existing tests in `indexrs-core/src/verify.rs` (no new tests needed — behavior is identical)

**Step 1: Write a micro-benchmark test to confirm behavior parity**

Add this test at the bottom of the `mod tests` block in `verify.rs`:

```rust
#[test]
fn test_line_index_memchr_parity() {
    // Verify LineIndex produces the same results for varied content
    let cases: &[&[u8]] = &[
        b"",
        b"no newlines",
        b"\n",
        b"\n\n\n",
        b"line1\nline2\nline3\n",
        b"line1\nline2\nline3",
        b"\nleading\ntrailing\n",
    ];
    for content in cases {
        let idx = LineIndex::new(content);
        // Manually compute expected offsets
        let expected: Vec<usize> = content
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'\n')
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            idx.newline_offsets, expected,
            "mismatch for content: {:?}",
            String::from_utf8_lossy(content)
        );
    }
}
```

**Step 2: Run the test to verify it passes with current code**

Run: `cargo test -p indexrs-core -- test_line_index_memchr_parity`
Expected: PASS

**Step 3: Replace the iterator chain with memchr**

In `verify.rs`, add the import at the top (after the existing `use regex` line):

```rust
use memchr::memchr_iter;
```

Then replace the `LineIndex::new()` body (lines 23-34):

```rust
    fn new(content: &[u8]) -> Self {
        let newline_offsets: Vec<usize> = memchr_iter(b'\n', content).collect();
        LineIndex {
            newline_offsets,
            content_len: content.len(),
        }
    }
```

**Step 4: Run all verify tests**

Run: `cargo test -p indexrs-core -- verify`
Expected: all PASS

**Step 5: Commit**

```bash
git add indexrs-core/src/verify.rs
git commit -m "perf: use memchr SIMD for newline scanning in LineIndex"
```

---

### Task 3: Eliminate double LineIndex construction in `verify_with_context()`

Currently `verify()` builds a `LineIndex` (line 145), then `verify_with_context()` calls `verify()` and builds a **second** `LineIndex` (line 259). Fix: extract `verify_inner()` that returns both the matches and the `LineIndex`, share it between the two methods.

**Files:**
- Modify: `indexrs-core/src/verify.rs:140-326`
- Test: existing tests cover this (behavior is identical)

**Step 1: Refactor verify() to share the LineIndex**

Replace `verify()` and `verify_with_context()` with this structure:

First, add a private method `verify_inner` that returns both the line matches and the line index:

```rust
    /// Inner verify that returns both matches and the LineIndex for reuse.
    fn verify_inner(&self, content: &[u8]) -> (Vec<LineMatch>, LineIndex) {
        if content.is_empty() {
            return (Vec::new(), LineIndex::new(content));
        }

        let line_index = LineIndex::new(content);
        let text = String::from_utf8_lossy(content);

        let matches = match &self.pattern {
            MatchPattern::Literal(lit) => self.verify_literal(&text, &line_index, lit.as_bytes()),
            MatchPattern::Regex(_) | MatchPattern::LiteralCaseInsensitive(_) => {
                self.verify_regex(&text, &line_index)
            }
        };

        (matches, line_index)
    }
```

Then simplify `verify()` to delegate:

```rust
    pub fn verify(&self, content: &[u8]) -> Vec<LineMatch> {
        self.verify_inner(content).0
    }
```

Then update `verify_with_context()` to use `verify_inner()` instead of calling `verify()` + building a second `LineIndex`:

```rust
    pub fn verify_with_context(&self, content: &[u8]) -> Vec<ContextBlock> {
        let (line_matches, line_index) = self.verify_inner(content);
        if line_matches.is_empty() {
            return Vec::new();
        }

        let total_lines = line_index.line_count() as u32;

        // ... rest of the method is identical, but remove the line:
        //   let line_index = LineIndex::new(content);
        //   let total_lines = line_index.line_count() as u32;
        // since we already have both from verify_inner()
```

Keep the rest of `verify_with_context` unchanged — just delete the two lines that reconstruct `LineIndex` and `total_lines`.

**Step 2: Run all verify tests**

Run: `cargo test -p indexrs-core -- verify`
Expected: all PASS

**Step 3: Run all multi_search tests too (they use ContentVerifier)**

Run: `cargo test -p indexrs-core -- multi_search`
Expected: all PASS

**Step 4: Commit**

```bash
git add indexrs-core/src/verify.rs
git commit -m "perf: eliminate double LineIndex construction in verify_with_context"
```

---

### Task 4: Cache MetadataReader in Segment (eliminate per-lookup header validation)

Every call to `Segment::get_metadata()` and `get_size_bytes()` creates a new `MetadataReader` that re-validates the magic number, version, and size. During `sort_candidates_by_size()` with N candidates, that's N header validations. Fix: validate once in `Segment::open()` and store the validated `entry_count` (already done), then add methods that skip re-validation by constructing a `MetadataReader` from pre-validated fields.

The simplest approach: add a `metadata_reader_unchecked()` private helper that builds a `MetadataReader` without validation (since we already validated in `open()`), then use it in `get_metadata()` and `get_size_bytes()`.

**Files:**
- Modify: `indexrs-core/src/metadata.rs:212-253` — add `new_unchecked()` constructor
- Modify: `indexrs-core/src/segment.rs:147-171` — use unchecked reader
- Test: existing tests cover this (behavior is identical)

**Step 1: Add `MetadataReader::new_unchecked()` constructor**

Add this method to `impl<'a> MetadataReader<'a>` in `metadata.rs`, right after `new()`:

```rust
    /// Create a reader without re-validating the header.
    ///
    /// # Safety (logical, not memory)
    ///
    /// The caller must guarantee that `meta_data` has already been validated
    /// by a prior call to [`MetadataReader::new()`]. This is the case for
    /// `Segment`, which validates during `open()` and stores the mmaps.
    pub(crate) fn new_unchecked(meta_data: &'a [u8], paths_data: &'a [u8], entry_count: u32) -> Self {
        MetadataReader {
            data: meta_data,
            paths: paths_data,
            entry_count,
        }
    }
```

**Step 2: Update Segment to use new_unchecked in get_metadata and get_size_bytes**

In `segment.rs`, replace the bodies of `get_metadata()` and `get_size_bytes()`:

```rust
    pub fn get_metadata(&self, file_id: FileId) -> Result<Option<FileMetadata>, IndexError> {
        let reader = MetadataReader::new_unchecked(&self.meta_mmap, &self.paths_mmap, self.entry_count);
        reader.get(file_id)
    }

    pub fn get_size_bytes(&self, file_id: FileId) -> Result<Option<u32>, IndexError> {
        let reader = MetadataReader::new_unchecked(&self.meta_mmap, &self.paths_mmap, self.entry_count);
        reader.get_size_bytes(file_id)
    }
```

Also update `metadata_reader()`:

```rust
    pub fn metadata_reader(&self) -> MetadataReader<'_> {
        MetadataReader::new_unchecked(&self.meta_mmap, &self.paths_mmap, self.entry_count)
    }
```

Note: `metadata_reader()` return type changes from `Result<MetadataReader<'_>, IndexError>` to `MetadataReader<'_>` since it can no longer fail. Update all call sites — search for `.metadata_reader().unwrap()` or `.metadata_reader()?` patterns:

- `segment_manager.rs` uses `segment.metadata_reader()?` in `compact_with_budget()` and `find_file_in_segments()` — remove the `?`
- `recovery.rs` may use it — check and update
- Tests in `segment.rs` — `segment.metadata_reader().unwrap()` becomes `segment.metadata_reader()`

**Step 3: Run all tests**

Run: `cargo test --workspace`
Expected: all PASS

**Step 4: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings

**Step 5: Commit**

```bash
git add indexrs-core/src/metadata.rs indexrs-core/src/segment.rs indexrs-core/src/segment_manager.rs indexrs-core/src/recovery.rs
git commit -m "perf: skip MetadataReader header re-validation on every lookup"
```

---

### Task 5: Pre-allocate zstd decompression buffer using known file size

`ContentStoreReader::read_content()` starts with `Vec::new()` which grows dynamically during decompression. The `size_bytes` field from metadata is the original uncompressed size — use it as a capacity hint to eliminate reallocation.

**Files:**
- Modify: `indexrs-core/src/content.rs:181-212` — add `read_content_with_hint()`
- Modify: `indexrs-core/src/multi_search.rs` — pass size hint at call sites
- Test: existing tests cover this (behavior is identical)

**Step 1: Add `read_content_with_size_hint()` method**

Add this method to `impl ContentStoreReader` in `content.rs`, after `read_content()`:

```rust
    /// Read and decompress content with a pre-allocation hint.
    ///
    /// Like [`read_content()`](Self::read_content) but pre-allocates the output
    /// buffer to `size_hint` bytes, avoiding reallocation during decompression.
    /// The `size_hint` should be the original uncompressed file size from metadata.
    pub fn read_content_with_size_hint(
        &self,
        offset: u64,
        compressed_len: u32,
        size_hint: usize,
    ) -> crate::Result<Vec<u8>> {
        let start = usize::try_from(offset).map_err(|_| {
            IndexError::IndexCorruption(format!("content offset {offset} exceeds address space"))
        })?;
        let clen = compressed_len as usize;
        let end = start.checked_add(clen).ok_or_else(|| {
            IndexError::IndexCorruption(format!("content range overflow: {start} + {clen}"))
        })?;

        if end > self.mmap.len() {
            return Err(IndexError::IndexCorruption(format!(
                "content read out of bounds: offset={offset}, len={compressed_len}, \
                 store size={}",
                self.mmap.len()
            )));
        }

        let compressed = &self.mmap[start..end];
        let decoder = zstd::stream::Decoder::new(compressed)
            .map_err(|e| IndexError::IndexCorruption(format!("zstd decoder init failed: {e}")))?;
        // Pre-allocate to avoid reallocation; cap at MAX_DECOMPRESSED_SIZE
        let capacity = size_hint.min(MAX_DECOMPRESSED_SIZE);
        let mut output = Vec::with_capacity(capacity);
        let bytes_read = decoder
            .take(MAX_DECOMPRESSED_SIZE as u64 + 1)
            .read_to_end(&mut output)
            .map_err(|e| IndexError::IndexCorruption(format!("zstd decompression failed: {e}")))?;
        if bytes_read > MAX_DECOMPRESSED_SIZE {
            return Err(IndexError::IndexCorruption(format!(
                "decompressed content exceeds size limit of {MAX_DECOMPRESSED_SIZE} bytes"
            )));
        }
        Ok(output)
    }
```

**Step 2: Use `read_content_with_size_hint` in multi_search.rs**

In `multi_search.rs`, update all content read call sites where metadata is available. There are 4 locations:

1. In `search_single_segment_with_context` parallel path (~line 356-359):
```rust
let content = segment
    .content_reader()
    .read_content_with_size_hint(meta.content_offset, meta.content_len, meta.size_bytes as usize)
    .ok()?;
```

2. In `search_single_segment_with_context_seq` (~line 428-430): same change.

3. In `verify_candidate_with_pattern` (~line 523-526): same change.

**Step 3: Run all tests**

Run: `cargo test --workspace`
Expected: all PASS

**Step 4: Commit**

```bash
git add indexrs-core/src/content.rs indexrs-core/src/multi_search.rs
git commit -m "perf: pre-allocate zstd decompression buffer from known file size"
```

---

### Task 6: Hoist SystemTime::now() and RankingConfig::default() out of per-file loops

Both are computed inside the per-candidate loop. `SystemTime::now()` is a syscall per file; `RankingConfig::default()` allocates per file. Fix: compute both once and pass them through.

**Files:**
- Modify: `indexrs-core/src/multi_search.rs` — hoist to call sites, pass through

**Step 1: Hoist in `search_single_segment_with_context` (parallel path)**

Before the `candidates.par_iter()` call (~line 340), add:

```rust
    let now_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ranking_config = RankingConfig::default();
```

Then inside the closure, replace:
- The `let now = SystemTime::now()...` block (~lines 377-380) → use `now_epoch_secs`
- The `let config = RankingConfig::default();` (~line 390) → use `&ranking_config`

**Step 2: Hoist in `search_single_segment_with_context_seq`**

At the top of the function (~line 416), add the same two lets. Then replace the per-file `now` and `config` inside the loop body.

**Step 3: Hoist in `verify_candidate_with_pattern`**

This function is called per-candidate from both the pattern search paths. Change its signature to accept `now_epoch_secs: u64` and `ranking_config: &RankingConfig` as parameters:

```rust
fn verify_candidate_with_pattern(
    segment: &Segment,
    file_id: FileId,
    pattern: &MatchPattern,
    verifier: &ContentVerifier,
    context_lines: usize,
    now_epoch_secs: u64,
    ranking_config: &RankingConfig,
) -> Option<FileMatch> {
```

Remove the `SystemTime::now()` and `RankingConfig::default()` calls inside (~lines 564-567, 586). Use the parameters instead.

Then update all callers:
- `search_single_segment_with_pattern` (parallel path, ~line 654-655): compute once before `par_iter()`, pass through
- `search_single_segment_with_pattern_seq` (~line 692-693): compute once at top, pass through

**Step 4: Run all tests**

Run: `cargo test --workspace`
Expected: all PASS

**Step 5: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings

**Step 6: Commit**

```bash
git add indexrs-core/src/multi_search.rs
git commit -m "perf: hoist SystemTime::now() and RankingConfig out of per-file loops"
```

---

### Task 7: Use memchr::memmem for substring search in verify.rs and multi_search.rs

Replace both `find_substring()` implementations (naive `windows().position()`) with `memchr::memmem::find()` which uses SIMD-accelerated search (Two-Way + SSE/AVX2/NEON). This is the single highest-impact change: 10-100x faster for the verification hot path.

**Files:**
- Modify: `indexrs-core/src/verify.rs:330-337` — replace `find_substring()`
- Modify: `indexrs-core/src/multi_search.rs:136-144` — replace `find_substring()`
- Test: existing tests cover both (behavior is identical)

**Step 1: Replace `find_substring` in `verify.rs`**

At the top of `verify.rs`, add import (combine with existing memchr import):

```rust
use memchr::{memchr_iter, memmem};
```

Replace the `find_substring` function at the bottom of `verify.rs`:

```rust
fn find_substring(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memmem::find(haystack, needle)
}
```

**Step 2: Replace `find_substring` in `multi_search.rs`**

Add import at the top of `multi_search.rs`:

```rust
use memchr::memmem;
```

Replace the `find_substring` function:

```rust
fn find_substring(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memmem::find(haystack, needle)
}
```

**Step 3: Run all tests**

Run: `cargo test --workspace`
Expected: all PASS

**Step 4: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings

**Step 5: Commit**

```bash
git add indexrs-core/src/verify.rs indexrs-core/src/multi_search.rs
git commit -m "perf: use memchr::memmem SIMD for substring search (10-100x faster)"
```

---

### Task 8: Use `memchr::memmem::Finder` for repeated searches in multi_search.rs

The `verify_content_matches()` function in `multi_search.rs` calls `find_substring()` in a loop per line. Each call to `memmem::find()` re-initializes the searcher. Pre-building a `memmem::Finder` once per query amortizes the setup cost across all lines.

**Files:**
- Modify: `indexrs-core/src/multi_search.rs:42-134` — use `Finder` in `verify_content_matches`

**Step 1: Refactor verify_content_matches to use Finder**

Replace lines 42-134 of `verify_content_matches`:

Change the inner loop to use a pre-built `Finder`:

```rust
fn verify_content_matches(content: &[u8], query: &str, context_lines: usize) -> Vec<LineMatch> {
    if query.is_empty() || content.is_empty() {
        return Vec::new();
    }

    // Fold query to lowercase for case-insensitive matching.
    let folded_query: Vec<u8> = query.bytes().map(crate::trigram::ascii_fold_byte).collect();
    let finder = memmem::Finder::new(&folded_query);
    let text = String::from_utf8_lossy(content);
    let all_lines: Vec<&str> = text.split('\n').collect();

    // First pass: find matching line indices and their ranges
    let mut match_indices: Vec<(usize, Vec<(usize, usize)>)> = Vec::new();

    for (line_idx, line) in all_lines.iter().enumerate() {
        // Skip empty trailing line from trailing newline
        if line.is_empty() && line_idx > 0 && line_idx == all_lines.len() - 1 {
            continue;
        }

        let line_bytes = line.as_bytes();
        // Fold the line bytes for searching.
        let folded_line: Vec<u8> = line_bytes
            .iter()
            .map(|&b| crate::trigram::ascii_fold_byte(b))
            .collect();
        let mut ranges = Vec::new();
        let mut search_start = 0;

        while search_start + folded_query.len() <= folded_line.len() {
            if let Some(pos) = finder.find(&folded_line[search_start..]) {
                let abs_start = search_start + pos;
                let abs_end = abs_start + folded_query.len();
                ranges.push((abs_start, abs_end));
                search_start = abs_start + 1; // advance past start to find overlapping matches
            } else {
                break;
            }
        }

        if !ranges.is_empty() {
            match_indices.push((line_idx, ranges));
        }
    }

    // Second pass: build LineMatch with context (unchanged)
    // ... rest is identical
```

**Step 2: Run all tests**

Run: `cargo test --workspace`
Expected: all PASS

**Step 3: Commit**

```bash
git add indexrs-core/src/multi_search.rs
git commit -m "perf: pre-build memmem::Finder for amortized SIMD search setup"
```

---

### Task 9: Final verification

**Step 1: Run full test suite**

Run: `cargo test --workspace`
Expected: all PASS

**Step 2: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings

**Step 3: Check formatting**

Run: `cargo fmt --all -- --check`
Expected: no changes needed

**Step 4: Run the demo to smoke-test**

Run: `cargo run -p indexrs-core --example demo -- indexrs-core/src "find_substring"`
Expected: search results appear, no errors
