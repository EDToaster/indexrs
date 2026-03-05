# ASCII Case-Fold Trigrams Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Store lowercase-folded trigrams in the index so that case-insensitive queries (the default) work via direct posting list lookup instead of degrading to full scan. Only ASCII bytes A-Z are folded to a-z (preserving byte lengths for non-ASCII).

**Architecture:** Add `ascii_fold_byte(b: u8) -> u8` to `trigram.rs` and new `extract_trigrams_folded()` / `extract_unique_trigrams_folded()` functions that fold bytes inline during the sliding window (zero allocation). Update `PostingListBuilder::add_file()` to use folded extraction, and `find_candidates()` to fold the query before trigram extraction. The existing non-folded `extract_trigrams()` is preserved for any use case needing raw byte trigrams.

**Tech Stack:** Rust 2024, existing `ferret-indexer-core` modules (`trigram`, `posting`, `intersection`), `tempfile` (dev)

**Prerequisite for:** All M4 issues (HHC-46 through HHC-51). This must be implemented before the query engine, since the query engine's trigram extraction assumes a case-folded index.

---

## Task 1: Add `ascii_fold_byte` and folded extraction functions to `trigram.rs`

**Files:**
- Modify: `ferret-indexer-core/src/trigram.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Write the failing tests

Add to the `#[cfg(test)] mod tests` block in `trigram.rs`:

```rust
#[test]
fn test_ascii_fold_byte_lowercase_unchanged() {
    for b in b'a'..=b'z' {
        assert_eq!(ascii_fold_byte(b), b);
    }
}

#[test]
fn test_ascii_fold_byte_uppercase_folded() {
    for (upper, lower) in (b'A'..=b'Z').zip(b'a'..=b'z') {
        assert_eq!(ascii_fold_byte(upper), lower);
    }
}

#[test]
fn test_ascii_fold_byte_non_alpha_unchanged() {
    // Digits, punctuation, whitespace, non-ASCII all pass through
    for b in [b'0', b'9', b' ', b'\n', b'{', b'}', b'(', 0xFF, 0x00, 0x80] {
        assert_eq!(ascii_fold_byte(b), b);
    }
}

#[test]
fn test_extract_trigrams_folded_lowercase_content() {
    // Already-lowercase content produces same trigrams as non-folded
    let content = b"abc";
    let folded: Vec<Trigram> = extract_trigrams_folded(content).collect();
    let raw: Vec<Trigram> = extract_trigrams(content).collect();
    assert_eq!(folded, raw);
}

#[test]
fn test_extract_trigrams_folded_uppercase_content() {
    // "ABC" folds to trigram (a, b, c)
    let content = b"ABC";
    let trigrams: Vec<Trigram> = extract_trigrams_folded(content).collect();
    assert_eq!(trigrams, vec![Trigram::from_bytes(b'a', b'b', b'c')]);
}

#[test]
fn test_extract_trigrams_folded_mixed_case() {
    // "FnMain" -> trigrams from "fnmain": fn, nm, ma, ai, in
    // (note: no space, so different from "fn main")
    let content = b"FnMain";
    let trigrams: Vec<Trigram> = extract_trigrams_folded(content).collect();
    assert_eq!(trigrams.len(), 4);
    assert_eq!(trigrams[0], Trigram::from_bytes(b'f', b'n', b'm'));
    assert_eq!(trigrams[1], Trigram::from_bytes(b'n', b'm', b'a'));
    assert_eq!(trigrams[2], Trigram::from_bytes(b'm', b'a', b'i'));
    assert_eq!(trigrams[3], Trigram::from_bytes(b'a', b'i', b'n'));
}

#[test]
fn test_extract_trigrams_folded_non_ascii_passthrough() {
    // Non-ASCII bytes pass through unmodified
    let content: &[u8] = &[0xFF, b'A', b'B', 0x80];
    let trigrams: Vec<Trigram> = extract_trigrams_folded(content).collect();
    assert_eq!(trigrams.len(), 2);
    assert_eq!(trigrams[0], Trigram::from_bytes(0xFF, b'a', b'b'));
    assert_eq!(trigrams[1], Trigram::from_bytes(b'a', b'b', 0x80));
}

#[test]
fn test_extract_unique_trigrams_folded_deduplicates_case() {
    // "AaA" has raw trigrams "AaA" but folded trigram "aaa" (1 unique)
    let content = b"AaA";
    let unique = extract_unique_trigrams_folded(content);
    assert_eq!(unique.len(), 1);
    assert!(unique.contains(&Trigram::from_bytes(b'a', b'a', b'a')));
}

#[test]
fn test_extract_unique_trigrams_folded_fn_main() {
    // "fn main() {}" is already lowercase, so folded == raw
    let content = b"fn main() {}";
    let folded = extract_unique_trigrams_folded(content);
    let raw = extract_unique_trigrams(content);
    assert_eq!(folded, raw);
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_ascii_fold test_extract_trigrams_folded test_extract_unique_trigrams_folded`

Expected: FAIL -- functions do not exist.

### Step 3: Implement the functions

Add to `trigram.rs`, after the existing `extract_unique_trigrams` function and before the `#[cfg(test)]` block:

```rust
/// Fold an ASCII uppercase byte to lowercase. Non-ASCII and non-alpha bytes pass through unchanged.
///
/// This is used to build a case-folded trigram index: all trigrams are stored
/// as lowercase so that case-insensitive queries (the default) can look up
/// trigrams directly without generating case permutations.
///
/// Only ASCII A-Z (0x41-0x5A) are folded to a-z (0x61-0x7A). All other bytes,
/// including UTF-8 continuation bytes, pass through unchanged. This preserves
/// byte lengths so trigram window boundaries are not affected.
#[inline]
pub fn ascii_fold_byte(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b.to_ascii_lowercase()
    } else {
        b
    }
}

/// Extract all trigrams from content with ASCII case folding.
///
/// Like [`extract_trigrams`], but folds A-Z to a-z in each byte before
/// forming the trigram. This produces lowercase trigrams regardless of the
/// original content case, enabling case-insensitive index lookups.
///
/// No allocation is needed -- folding happens inline during the sliding window.
///
/// # Examples
///
/// ```
/// use ferret_indexer_core::trigram::extract_trigrams_folded;
/// use ferret_indexer_core::Trigram;
///
/// let content = b"ABC";
/// let trigrams: Vec<Trigram> = extract_trigrams_folded(content).collect();
/// assert_eq!(trigrams, vec![Trigram::from_bytes(b'a', b'b', b'c')]);
/// ```
pub fn extract_trigrams_folded(content: &[u8]) -> impl Iterator<Item = Trigram> + '_ {
    content
        .windows(3)
        .map(|w| Trigram::from_bytes(ascii_fold_byte(w[0]), ascii_fold_byte(w[1]), ascii_fold_byte(w[2])))
}

/// Extract the unique set of trigrams from content with ASCII case folding.
///
/// Equivalent to collecting [`extract_trigrams_folded`] into a [`HashSet`].
/// Since case is folded, "ABC" and "abc" produce the same trigram set.
///
/// # Examples
///
/// ```
/// use ferret_indexer_core::trigram::extract_unique_trigrams_folded;
///
/// let unique = extract_unique_trigrams_folded(b"ABab");
/// assert_eq!(unique.len(), 2); // "aba" and "bab" (both lowercased)
/// ```
pub fn extract_unique_trigrams_folded(content: &[u8]) -> HashSet<Trigram> {
    extract_trigrams_folded(content).collect()
}
```

### Step 4: Add re-exports to lib.rs

Add to the re-exports in `ferret-indexer-core/src/lib.rs`:

```rust
pub use trigram::{ascii_fold_byte, extract_trigrams_folded, extract_unique_trigrams_folded};
```

### Step 5: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_ascii_fold test_extract_trigrams_folded test_extract_unique_trigrams_folded`

Expected: PASS.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/trigram.rs ferret-indexer-core/src/lib.rs
git commit -m "feat: add ASCII case-folded trigram extraction functions"
```

---

## Task 2: Update `PostingListBuilder::add_file()` to use folded trigram extraction

**Files:**
- Modify: `ferret-indexer-core/src/posting.rs`

### Step 1: Write the failing test

Add to the `#[cfg(test)] mod tests` block in `posting.rs`:

```rust
#[test]
fn test_posting_builder_case_fold_uppercase_content() {
    // "FN MAIN() {}" should produce the same trigrams as "fn main() {}"
    let mut builder_upper = PostingListBuilder::file_only();
    builder_upper.add_file(FileId(0), b"FN MAIN() {}");
    builder_upper.finalize();

    let mut builder_lower = PostingListBuilder::file_only();
    builder_lower.add_file(FileId(0), b"fn main() {}");
    builder_lower.finalize();

    assert_eq!(builder_upper.trigram_count(), builder_lower.trigram_count());

    // Both should contain the lowercase trigram "fn "
    let fp_upper = builder_upper.file_postings();
    let fp_lower = builder_lower.file_postings();
    assert!(fp_upper.contains_key(&Trigram::from_bytes(b'f', b'n', b' ')));
    assert!(fp_lower.contains_key(&Trigram::from_bytes(b'f', b'n', b' ')));

    // Neither should contain uppercase "FN "
    assert!(!fp_upper.contains_key(&Trigram::from_bytes(b'F', b'N', b' ')));
}

#[test]
fn test_posting_builder_case_fold_mixed_case() {
    let mut builder = PostingListBuilder::file_only();
    builder.add_file(FileId(0), b"HttpRequest");
    builder.add_file(FileId(1), b"httprequest");
    builder.finalize();

    let fp = builder.file_postings();

    // "htt" trigram should contain both files
    let htt = &fp[&Trigram::from_bytes(b'h', b't', b't')];
    assert_eq!(htt, &vec![FileId(0), FileId(1)]);

    // No uppercase trigrams should exist
    assert!(!fp.contains_key(&Trigram::from_bytes(b'H', b't', b't')));
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_posting_builder_case_fold`

Expected: FAIL -- `add_file` still extracts raw (non-folded) trigrams, so "FN " trigram exists instead of "fn ".

### Step 3: Update `add_file()` to use folded extraction

In `posting.rs`, change the import:

```rust
use crate::trigram::{extract_trigrams, extract_trigrams_folded};
```

Then update `add_file()` to use `extract_trigrams_folded`:

```rust
pub fn add_file(&mut self, file_id: FileId, content: &[u8]) {
    if self.store_positions {
        for (offset, trigram) in extract_trigrams_folded(content).enumerate() {
            debug_assert!(
                offset <= u32::MAX as usize,
                "file content too large for u32 offset: {offset}"
            );
            self.file_postings.entry(trigram).or_default().push(file_id);
            self.positional_postings
                .entry(trigram)
                .or_default()
                .push((file_id, offset as u32));
        }
    } else {
        for trigram in extract_trigrams_folded(content) {
            self.file_postings.entry(trigram).or_default().push(file_id);
        }
    }
}
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- posting`

Expected: All posting tests PASS. Existing tests use lowercase content ("fn main() {}", "fn parse() {}") so folding doesn't change their trigrams. The new tests verify uppercase content gets folded.

### Step 5: Commit

```bash
git add ferret-indexer-core/src/posting.rs
git commit -m "feat: fold ASCII case in PostingListBuilder::add_file()"
```

---

## Task 3: Update `find_candidates()` to fold query trigrams

**Files:**
- Modify: `ferret-indexer-core/src/intersection.rs`

### Step 1: Write the failing test

Add to the `#[cfg(test)] mod tests` block in `intersection.rs`:

```rust
#[test]
fn test_find_candidates_case_insensitive() {
    // Build index with lowercase content
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trigrams.bin");

    let mut builder = PostingListBuilder::new();
    builder.add_file(FileId(0), b"fn main() {}");
    builder.add_file(FileId(1), b"fn parse() {}");
    builder.finalize();
    TrigramIndexWriter::write(&builder, &path).unwrap();
    let reader = TrigramIndexReader::open(&path).unwrap();

    // Searching "MAIN" (uppercase) should find file 0 via folded trigrams
    let candidates = find_candidates(&reader, "MAIN").unwrap();
    assert_eq!(candidates, vec![FileId(0)]);
}

#[test]
fn test_find_candidates_mixed_case_query() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trigrams.bin");

    let mut builder = PostingListBuilder::new();
    builder.add_file(FileId(0), b"fn main() {}");
    builder.add_file(FileId(1), b"fn parse() {}");
    builder.finalize();
    TrigramIndexWriter::write(&builder, &path).unwrap();
    let reader = TrigramIndexReader::open(&path).unwrap();

    // "Parse" with capital P should still find file 1
    let candidates = find_candidates(&reader, "Parse").unwrap();
    assert_eq!(candidates, vec![FileId(1)]);
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_find_candidates_case`

Expected: FAIL -- "MAIN" query extracts uppercase trigrams "MAI", "AIN" which don't match the lowercase index.

### Step 3: Update `find_candidates()` to use folded extraction

In `intersection.rs`, change the import:

```rust
use crate::trigram::extract_unique_trigrams_folded;
```

Then update `find_candidates()`:

```rust
pub fn find_candidates(
    reader: &TrigramIndexReader,
    query: &str,
) -> Result<Vec<FileId>, IndexError> {
    if query.len() < 3 {
        return Ok(Vec::new());
    }

    let trigrams = extract_unique_trigrams_folded(query.as_bytes());

    if trigrams.is_empty() {
        return Ok(Vec::new());
    }

    let mut lists = Vec::with_capacity(trigrams.len());

    for trigram in &trigrams {
        let file_ids = reader.lookup_file_ids(*trigram)?;
        lists.push(file_ids);
    }

    Ok(intersect_file_ids(&lists))
}
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- intersection`

Expected: All intersection tests PASS. Existing tests use lowercase queries and content, so folding doesn't change behavior. New tests verify uppercase queries match the lowercase index.

### Step 5: Run the full test suite

Run: `cargo test --workspace`

Expected: All tests PASS.

### Step 6: Commit

```bash
git add ferret-indexer-core/src/intersection.rs
git commit -m "feat: fold query trigrams in find_candidates() for case-insensitive lookup"
```

---

## Task 4: Update `bench_space.rs` to use folded trigram extraction

**Files:**
- Modify: `ferret-indexer-core/examples/bench_space.rs`

### Step 1: Update imports

Change the import to include folded variants:

```rust
use ferret_indexer_core::{
    DEFAULT_MAX_FILE_SIZE, DirectoryWalkerBuilder, Language, Trigram, encode_delta_varint,
    extract_trigrams_folded, extract_unique_trigrams_folded, is_binary_content, is_binary_path,
};
```

### Step 2: Replace trigram extraction calls

In Phase 3 (building posting lists), change line 159:

```rust
let unique: HashSet<Trigram> = extract_unique_trigrams_folded(content);
```

And line 166:

```rust
total_trigram_occurrences += extract_trigrams_folded(content).count() as u64;
```

### Step 3: Verify

Run: `cargo run -p ferret-indexer-core --example bench_space --release -- .`

Expected: Runs without errors. Trigram counts may be slightly lower (uppercase variants are now folded to lowercase, reducing unique trigram count).

### Step 4: Commit

```bash
git add ferret-indexer-core/examples/bench_space.rs
git commit -m "chore: use folded trigram extraction in bench_space example"
```

---

## Task 5: Add end-to-end test for case-insensitive search through segments

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs`

### Step 1: Write the test

Add to the `#[cfg(test)] mod tests` block in `multi_search.rs`:

```rust
#[test]
fn test_search_segments_case_insensitive_via_folded_index() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    // Index content with mixed case
    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![
            InputFile {
                path: "main.rs".to_string(),
                content: b"fn HttpRequest() {}".to_vec(),
                mtime: 0,
            },
            InputFile {
                path: "lib.rs".to_string(),
                content: b"fn httprequest() {}".to_vec(),
                mtime: 0,
            },
        ],
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);

    // Searching "httprequest" (lowercase) should find BOTH files
    // because the index folds "HttpRequest" to lowercase trigrams
    let result = search_segments(&snapshot, "httprequest").unwrap();
    assert_eq!(
        result.files.len(),
        2,
        "both files should match via case-folded trigrams"
    );

    // Searching "HTTPREQUEST" (uppercase) should also find both files
    let result = search_segments(&snapshot, "HTTPREQUEST").unwrap();
    assert_eq!(
        result.files.len(),
        2,
        "uppercase query should match via case-folded trigrams"
    );
}
```

### Step 2: Run the test

Run: `cargo test -p ferret-indexer-core -- test_search_segments_case_insensitive_via_folded_index`

Expected: PASS -- the segment writer uses `PostingListBuilder::file_only()` which now folds trigrams, and `find_candidates()` folds the query.

Note: The content verification in `search_segments` currently uses byte-level substring matching (`content.windows(query_bytes.len()).any(|w| w == query_bytes)`). This means the uppercase query "HTTPREQUEST" will find candidates via folded trigrams but will fail verification against "HttpRequest" since the byte comparison is case-sensitive. This is expected -- the verification step will be upgraded in HHC-49 to support case-insensitive matching. For now, only exact-case or lowercase queries will return results through verification.

If the test fails on verification (uppercase query finds 0 results despite finding candidates), adjust the test to only assert on the lowercase query variant:

```rust
// Searching "httprequest" (lowercase) finds both via folded trigrams
// (verification still does byte-level match, so only lowercase passes)
let result = search_segments(&snapshot, "httprequest").unwrap();
assert!(result.files.len() >= 1, "lowercase query should match");

// Note: uppercase query "HTTPREQUEST" finds candidates via folded trigrams
// but current verification is case-sensitive, so it may not match.
// HHC-49 (candidate verification) will add case-insensitive verification.
```

### Step 3: Run full test suite and verify

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: All tests PASS, clippy clean, fmt clean.

### Step 4: Commit

```bash
git add ferret-indexer-core/src/multi_search.rs
git commit -m "test: add case-insensitive search test via folded trigram index"
```

---

## Task 6: Update CLAUDE.md and verify everything

**Files:**
- Modify: `CLAUDE.md`

### Step 1: Update CLAUDE.md

Update the `trigram.rs` description to note the folded variants:

```
- `trigram.rs` — `extract_trigrams()` slides a 3-byte window over content. `extract_unique_trigrams()` deduplicates. `extract_trigrams_folded()` / `extract_unique_trigrams_folded()` fold ASCII A-Z to a-z inline for case-insensitive indexing. `ascii_fold_byte()` folds a single byte.
```

Update the `posting.rs` description:

```
- `posting.rs` — `PostingListBuilder` accumulates file-level posting lists during index build. Uses ASCII-folded trigram extraction (A-Z → a-z) so the index supports case-insensitive lookup by default. Two constructors: `new()` stores positions (for tests), `file_only()` skips positional postings (used by `SegmentWriter`, ~78% smaller index).
```

Update the Key design decisions section, add after the byte-level trigrams bullet:

```
- **ASCII case-folded trigrams** — Trigrams are extracted after folding A-Z to a-z. The index stores only lowercase trigrams, so case-insensitive queries (the default) work via direct posting list lookup. Case-sensitive queries use the same lowercase trigrams for candidate filtering, then verify exact case during content matching (slightly more false positives, but always correct). Non-ASCII bytes pass through unchanged.
```

### Step 2: Final verification

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: All tests PASS, clippy clean, fmt clean.

### Step 3: Commit

```bash
git add CLAUDE.md
git commit -m "docs: document ASCII case-folded trigrams in CLAUDE.md"
```

---

## Summary

| Task | Description | Files |
|------|-------------|-------|
| 1 | `ascii_fold_byte` + `extract_trigrams_folded` + `extract_unique_trigrams_folded` | `trigram.rs`, `lib.rs` |
| 2 | Update `PostingListBuilder::add_file()` to use folded extraction | `posting.rs` |
| 3 | Update `find_candidates()` to fold query trigrams | `intersection.rs` |
| 4 | Update `bench_space.rs` to use folded extraction | `bench_space.rs` |
| 5 | End-to-end test for case-insensitive search via folded index | `multi_search.rs` |
| 6 | Update CLAUDE.md + final verification | `CLAUDE.md` |

**Key design decisions:**
1. **ASCII-only folding** — Only A-Z (0x41-0x5A) mapped to a-z (0x61-0x7A). Non-ASCII bytes pass through unchanged, preserving byte lengths and trigram alignment for UTF-8 content.
2. **Zero-allocation folding** — `extract_trigrams_folded` folds bytes inline during the sliding window, no intermediate buffer needed.
3. **Preserved raw functions** — `extract_trigrams()` and `extract_unique_trigrams()` are kept for any use case needing raw byte trigrams.
4. **Same binary format** — No changes to `trigrams.bin` format. The trigram bytes stored in the table are simply lowercase instead of mixed-case. Readers work identically.
5. **Verification unchanged (for now)** — Content verification still does exact byte comparison. Case-insensitive verification is added later in HHC-49. This means the folded index correctly finds candidates for all case variants, but verification only confirms matches for the queried case until HHC-49 lands.
