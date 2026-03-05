# Crash Recovery: Detect and Clean Up Incomplete Segments

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** On startup, scan the `.ferret_index/segments/` directory, clean up temp directories left by crashed builds, validate each segment's files and headers, skip corrupted segments with a warning, remove stale lock files, and return the set of valid segments sorted by ID.

**Architecture:** A new `recovery.rs` module in `ferret-indexer-core` containing `recover_segments(base_dir) -> Result<Vec<Segment>>`. It scans the segments directory, deletes any entries matching the temp-dir naming pattern (`.seg_NNNN_tmp_*`), parses segment IDs from `seg_NNNN` directory names, validates that all required files exist and that `trigrams.bin` and `meta.bin` have correct magic numbers and version fields. Invalid segments are logged via `tracing::warn!` and skipped. A separate `cleanup_lock_file(ferret_dir)` function handles stale `.ferret_index/lock` removal. The module re-uses `Segment::open()` for the final load step, which already validates headers internally. A lightweight pre-check for file existence avoids partial mmapping of incomplete segments.

**Tech Stack:** Rust 2024, memmap2 (via existing Segment::open), tracing (for warn! logging), tempfile (dev), existing segment/index_writer/metadata modules

---

## Task 1: Make TRIG_MAGIC, TRIG_VERSION, META_MAGIC, META_VERSION accessible for header validation

**Files:**
- Modify: `ferret-indexer-core/src/index_writer.rs`
- Modify: `ferret-indexer-core/src/metadata.rs`

The recovery module needs to read the first 6 bytes of `trigrams.bin` and `meta.bin` to validate magic numbers and versions without fully opening the segment. Currently `META_MAGIC` and `META_VERSION` in `metadata.rs` are private (`const`), while `TRIG_MAGIC` and `TRIG_VERSION` in `index_writer.rs` are `pub(crate)`. We need to make the metadata constants `pub(crate)` too.

### Step 1: Change metadata.rs constants visibility

In `ferret-indexer-core/src/metadata.rs`, change the two constants from `const` to `pub(crate) const`:

```rust
/// Magic number for meta.bin header: "META" in ASCII as little-endian u32.
pub(crate) const META_MAGIC: u32 = 0x4D45_5441;

/// Current format version.
pub(crate) const META_VERSION: u16 = 1;
```

### Step 2: Verify nothing breaks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings. The change only widens visibility within the crate.

### Step 3: Commit

```bash
git add ferret-indexer-core/src/metadata.rs
git commit -m "refactor(metadata): make META_MAGIC and META_VERSION pub(crate) for recovery module"
```

---

## Task 2: Add recovery module skeleton with temp directory cleanup

**Files:**
- Create: `ferret-indexer-core/src/recovery.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Write the failing test

Create `ferret-indexer-core/src/recovery.rs` with the test for temp directory cleanup:

```rust
//! Crash recovery: detect and clean up incomplete segments on startup.
//!
//! When the indexer process crashes mid-build, it can leave behind:
//! - Temp directories from `SegmentWriter` (named `.seg_NNNN_tmp_<pid>`)
//! - A stale lock file (`.ferret_index/lock`)
//! - Partially-written segment directories with missing or corrupt files
//!
//! The [`recover_segments`] function scans the segments directory, cleans up
//! temp directories, validates each segment, and returns the set of valid
//! segments sorted by [`SegmentId`].

use std::fs;
use std::path::Path;

use crate::error::IndexError;
use crate::segment::Segment;
use crate::types::SegmentId;

/// Scan the segments directory, clean up incomplete state, and load valid segments.
///
/// This function:
/// 1. Deletes temp directories left by crashed `SegmentWriter` builds
///    (names starting with `.seg_` and containing `_tmp_`)
/// 2. Parses segment IDs from `seg_NNNN` directory names
/// 3. Validates each segment: checks required files exist, validates headers
/// 4. Opens valid segments via `Segment::open()`
/// 5. Logs warnings for invalid segments and skips them
///
/// Returns valid segments sorted by `SegmentId` (ascending).
///
/// # Arguments
///
/// * `segments_dir` - Path to the segments directory (e.g. `.ferret_index/segments/`).
///
/// # Errors
///
/// Returns `IndexError::Io` if the segments directory cannot be read.
/// Individual segment failures are logged and skipped, not propagated.
pub fn recover_segments(segments_dir: &Path) -> Result<Vec<Segment>, IndexError> {
    todo!()
}

/// Remove a stale lock file if it exists.
///
/// In a single-process use case, any lock file present at startup is stale
/// (the previous process must have crashed without cleaning up). This function
/// simply deletes `.ferret_index/lock` if it exists.
///
/// # Arguments
///
/// * `ferret_dir` - Path to the `.ferret_index/` directory.
///
/// # Errors
///
/// Returns `IndexError::Io` if the lock file exists but cannot be deleted.
pub fn cleanup_lock_file(ferret_dir: &Path) -> Result<(), IndexError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup_temp_directories() {
        let dir = tempfile::tempdir().unwrap();
        let segments_dir = dir.path().join("segments");
        fs::create_dir_all(&segments_dir).unwrap();

        // Create some temp directories that simulate crashed builds
        fs::create_dir_all(segments_dir.join(".seg_0000_tmp_12345")).unwrap();
        fs::create_dir_all(segments_dir.join(".seg_0001_tmp_99999")).unwrap();

        // Also create a valid-looking segment dir (but empty, so it will fail validation)
        // We'll test valid segments in a later task

        let segments = recover_segments(&segments_dir).unwrap();
        assert!(segments.is_empty());

        // Temp directories should be deleted
        assert!(!segments_dir.join(".seg_0000_tmp_12345").exists());
        assert!(!segments_dir.join(".seg_0001_tmp_99999").exists());
    }
}
```

### Step 2: Register the module in lib.rs

Add to `ferret-indexer-core/src/lib.rs` after the existing module declarations:

```rust
pub mod recovery;
```

And add re-exports:

```rust
pub use recovery::{cleanup_lock_file, recover_segments};
```

### Step 3: Run test to verify it fails

Run: `cargo test -p ferret-indexer-core -- test_cleanup_temp_directories -v`

Expected: FAIL with `not yet implemented` (from `todo!()`)

### Step 4: Implement temp directory cleanup and the skeleton of recover_segments

Replace the `todo!()` in `recover_segments` with:

```rust
pub fn recover_segments(segments_dir: &Path) -> Result<Vec<Segment>, IndexError> {
    if !segments_dir.exists() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(segments_dir)?;

    let mut segments = Vec::new();

    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Step 1: Delete temp directories from crashed builds.
        // SegmentWriter names them: .seg_NNNN_tmp_<pid>
        if name.starts_with(".seg_") && name.contains("_tmp_") {
            tracing::warn!(path = %entry.path().display(), "removing leftover temp directory");
            if let Err(e) = fs::remove_dir_all(entry.path()) {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "failed to remove temp directory"
                );
            }
            continue;
        }

        // Step 2: Parse segment ID from directory name (seg_NNNN).
        let segment_id = match parse_segment_id(&name) {
            Some(id) => id,
            None => continue, // Skip non-segment entries silently
        };

        let seg_path = entry.path();

        // Step 3: Validate required files exist.
        if let Err(reason) = validate_segment_files(&seg_path) {
            tracing::warn!(
                segment = %name,
                reason = %reason,
                "skipping invalid segment"
            );
            continue;
        }

        // Step 4: Validate headers (magic numbers and versions).
        if let Err(reason) = validate_segment_headers(&seg_path) {
            tracing::warn!(
                segment = %name,
                reason = %reason,
                "skipping segment with invalid headers"
            );
            continue;
        }

        // Step 5: Open the segment.
        match Segment::open(&seg_path, segment_id) {
            Ok(segment) => segments.push(segment),
            Err(e) => {
                tracing::warn!(
                    segment = %name,
                    error = %e,
                    "skipping segment that failed to open"
                );
            }
        }
    }

    // Sort by SegmentId ascending.
    segments.sort_by_key(|s| s.segment_id());

    Ok(segments)
}
```

Also add the helper functions (stubs for now -- we'll implement them in the next tasks):

```rust
/// Parse a segment ID from a directory name like "seg_0001".
///
/// Returns `None` if the name doesn't match the expected pattern.
fn parse_segment_id(name: &str) -> Option<SegmentId> {
    let suffix = name.strip_prefix("seg_")?;
    let id: u32 = suffix.parse().ok()?;
    Some(SegmentId(id))
}

/// Check that all required files exist in a segment directory.
///
/// Required files: trigrams.bin, meta.bin, paths.bin, content.zst
fn validate_segment_files(seg_dir: &Path) -> Result<(), String> {
    const REQUIRED_FILES: &[&str] = &["trigrams.bin", "meta.bin", "paths.bin", "content.zst"];

    for file_name in REQUIRED_FILES {
        if !seg_dir.join(file_name).exists() {
            return Err(format!("missing required file: {file_name}"));
        }
    }
    Ok(())
}

/// Validate the magic numbers and version fields in trigrams.bin and meta.bin.
fn validate_segment_headers(seg_dir: &Path) -> Result<(), String> {
    validate_trigram_header(&seg_dir.join("trigrams.bin"))?;
    validate_meta_header(&seg_dir.join("meta.bin"))?;
    Ok(())
}

/// Read and validate the first 6 bytes of trigrams.bin (magic + version).
fn validate_trigram_header(path: &Path) -> Result<(), String> {
    let data = fs::read(path).map_err(|e| format!("cannot read trigrams.bin: {e}"))?;
    if data.len() < 6 {
        return Err(format!("trigrams.bin too small: {} bytes", data.len()));
    }

    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if magic != crate::index_writer::TRIG_MAGIC {
        return Err(format!(
            "trigrams.bin bad magic: expected 0x{:08X}, got 0x{magic:08X}",
            crate::index_writer::TRIG_MAGIC
        ));
    }

    let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
    if version != crate::index_writer::TRIG_VERSION {
        return Err(format!(
            "trigrams.bin unsupported version: expected {}, got {version}",
            crate::index_writer::TRIG_VERSION
        ));
    }

    Ok(())
}

/// Read and validate the first 6 bytes of meta.bin (magic + version).
fn validate_meta_header(path: &Path) -> Result<(), String> {
    let data = fs::read(path).map_err(|e| format!("cannot read meta.bin: {e}"))?;
    if data.len() < 6 {
        return Err(format!("meta.bin too small: {} bytes", data.len()));
    }

    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if magic != crate::metadata::META_MAGIC {
        return Err(format!(
            "meta.bin bad magic: expected 0x{:08X}, got 0x{magic:08X}",
            crate::metadata::META_MAGIC
        ));
    }

    let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
    if version != crate::metadata::META_VERSION {
        return Err(format!(
            "meta.bin unsupported version: expected {}, got {version}",
            crate::metadata::META_VERSION
        ));
    }

    Ok(())
}
```

And implement `cleanup_lock_file`:

```rust
pub fn cleanup_lock_file(ferret_dir: &Path) -> Result<(), IndexError> {
    let lock_path = ferret_dir.join("lock");
    if lock_path.exists() {
        tracing::warn!(path = %lock_path.display(), "removing stale lock file");
        fs::remove_file(&lock_path)?;
    }
    Ok(())
}
```

### Step 5: Run test to verify it passes

Run: `cargo test -p ferret-indexer-core -- test_cleanup_temp_directories -v`

Expected: PASS

### Step 6: Run full workspace checks

Run: `cargo check --workspace && cargo clippy --workspace -- -D warnings`

Expected: No errors or warnings.

### Step 7: Commit

```bash
git add ferret-indexer-core/src/recovery.rs ferret-indexer-core/src/lib.rs
git commit -m "feat(recovery): add recovery module with temp directory cleanup"
```

---

## Task 3: Add test for recover_segments with valid segments

**Files:**
- Modify: `ferret-indexer-core/src/recovery.rs`

### Step 1: Write the test

Add to the `tests` module in `recovery.rs`:

```rust
use crate::segment::{InputFile, SegmentWriter};

#[test]
fn test_recover_valid_segments() {
    let dir = tempfile::tempdir().unwrap();
    let segments_dir = dir.path().join("segments");
    fs::create_dir_all(&segments_dir).unwrap();

    // Build two valid segments
    let writer0 = SegmentWriter::new(&segments_dir, SegmentId(0));
    writer0
        .build(vec![InputFile {
            path: "a.rs".to_string(),
            content: b"fn a() { let x = 1; }".to_vec(),
            mtime: 100,
        }])
        .unwrap();

    let writer1 = SegmentWriter::new(&segments_dir, SegmentId(1));
    writer1
        .build(vec![InputFile {
            path: "b.rs".to_string(),
            content: b"fn b() { let y = 2; }".to_vec(),
            mtime: 200,
        }])
        .unwrap();

    // Recover
    let segments = recover_segments(&segments_dir).unwrap();

    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].segment_id(), SegmentId(0));
    assert_eq!(segments[1].segment_id(), SegmentId(1));
    assert_eq!(segments[0].entry_count(), 1);
    assert_eq!(segments[1].entry_count(), 1);
}
```

### Step 2: Run the test

Run: `cargo test -p ferret-indexer-core -- test_recover_valid_segments -v`

Expected: PASS (the implementation from Task 2 already handles this case).

### Step 3: Commit

```bash
git add ferret-indexer-core/src/recovery.rs
git commit -m "test(recovery): add test for recovering valid segments"
```

---

## Task 4: Add test for recover_segments sorted order and non-sequential IDs

**Files:**
- Modify: `ferret-indexer-core/src/recovery.rs`

### Step 1: Write the test

Add to the `tests` module:

```rust
#[test]
fn test_recover_segments_sorted_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let segments_dir = dir.path().join("segments");
    fs::create_dir_all(&segments_dir).unwrap();

    // Build segments with non-sequential IDs, in reverse order
    for &id in &[5u32, 2, 8, 0] {
        let writer = SegmentWriter::new(&segments_dir, SegmentId(id));
        writer
            .build(vec![InputFile {
                path: format!("file_{id}.rs"),
                content: format!("fn f{id}() {{ let x = {id}; }}").into_bytes(),
                mtime: id as u64,
            }])
            .unwrap();
    }

    let segments = recover_segments(&segments_dir).unwrap();

    assert_eq!(segments.len(), 4);
    let ids: Vec<u32> = segments.iter().map(|s| s.segment_id().0).collect();
    assert_eq!(ids, vec![0, 2, 5, 8]);
}
```

### Step 2: Run the test

Run: `cargo test -p ferret-indexer-core -- test_recover_segments_sorted_by_id -v`

Expected: PASS

### Step 3: Commit

```bash
git add ferret-indexer-core/src/recovery.rs
git commit -m "test(recovery): add test for segment ID sorting"
```

---

## Task 5: Add test for skipping segments with missing files

**Files:**
- Modify: `ferret-indexer-core/src/recovery.rs`

### Step 1: Write the test

Add to the `tests` module:

```rust
#[test]
fn test_recover_skips_segment_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let segments_dir = dir.path().join("segments");
    fs::create_dir_all(&segments_dir).unwrap();

    // Build a valid segment
    let writer = SegmentWriter::new(&segments_dir, SegmentId(0));
    writer
        .build(vec![InputFile {
            path: "good.rs".to_string(),
            content: b"fn good() { let x = 1; }".to_vec(),
            mtime: 100,
        }])
        .unwrap();

    // Create an incomplete segment directory (missing most files)
    let bad_seg = segments_dir.join("seg_0001");
    fs::create_dir_all(&bad_seg).unwrap();
    fs::write(bad_seg.join("trigrams.bin"), b"incomplete").unwrap();
    // Missing: meta.bin, paths.bin, content.zst

    let segments = recover_segments(&segments_dir).unwrap();

    // Only the valid segment should be loaded
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].segment_id(), SegmentId(0));

    // The bad segment directory should still exist (we skip, not delete)
    assert!(bad_seg.exists());
}
```

### Step 2: Run the test

Run: `cargo test -p ferret-indexer-core -- test_recover_skips_segment_missing_files -v`

Expected: PASS

### Step 3: Commit

```bash
git add ferret-indexer-core/src/recovery.rs
git commit -m "test(recovery): add test for skipping segments with missing files"
```

---

## Task 6: Add test for skipping segments with corrupted headers

**Files:**
- Modify: `ferret-indexer-core/src/recovery.rs`

### Step 1: Write the test

Add to the `tests` module:

```rust
#[test]
fn test_recover_skips_segment_bad_magic() {
    let dir = tempfile::tempdir().unwrap();
    let segments_dir = dir.path().join("segments");
    fs::create_dir_all(&segments_dir).unwrap();

    // Build a valid segment
    let writer = SegmentWriter::new(&segments_dir, SegmentId(0));
    writer
        .build(vec![InputFile {
            path: "good.rs".to_string(),
            content: b"fn good() { let x = 1; }".to_vec(),
            mtime: 100,
        }])
        .unwrap();

    // Create a segment with all required files but bad magic in trigrams.bin
    let bad_seg = segments_dir.join("seg_0001");
    fs::create_dir_all(&bad_seg).unwrap();

    // Write trigrams.bin with wrong magic number
    let mut bad_trig = vec![0u8; 10];
    bad_trig[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // bad magic
    bad_trig[4..6].copy_from_slice(&1u16.to_le_bytes()); // version 1
    bad_trig[6..10].copy_from_slice(&0u32.to_le_bytes()); // 0 trigrams
    fs::write(bad_seg.join("trigrams.bin"), &bad_trig).unwrap();

    // Write a valid-looking meta.bin header (but minimal)
    let mut meta = vec![0u8; 10];
    meta[0..4].copy_from_slice(&0x4D45_5441u32.to_le_bytes()); // META magic
    meta[4..6].copy_from_slice(&1u16.to_le_bytes()); // version 1
    meta[6..10].copy_from_slice(&0u32.to_le_bytes()); // 0 entries
    fs::write(bad_seg.join("meta.bin"), &meta).unwrap();

    fs::write(bad_seg.join("paths.bin"), b"").unwrap();
    fs::write(bad_seg.join("content.zst"), b"").unwrap();

    let segments = recover_segments(&segments_dir).unwrap();

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].segment_id(), SegmentId(0));
}

#[test]
fn test_recover_skips_segment_bad_meta_magic() {
    let dir = tempfile::tempdir().unwrap();
    let segments_dir = dir.path().join("segments");
    fs::create_dir_all(&segments_dir).unwrap();

    // Create a segment with valid trigrams.bin but bad meta.bin magic
    let bad_seg = segments_dir.join("seg_0000");
    fs::create_dir_all(&bad_seg).unwrap();

    // Valid trigrams.bin header
    let mut trig = vec![0u8; 10];
    trig[0..4].copy_from_slice(&0x5452_4947u32.to_le_bytes()); // TRIG magic
    trig[4..6].copy_from_slice(&1u16.to_le_bytes());
    trig[6..10].copy_from_slice(&0u32.to_le_bytes());
    fs::write(bad_seg.join("trigrams.bin"), &trig).unwrap();

    // Bad meta.bin magic
    let mut meta = vec![0u8; 10];
    meta[0..4].copy_from_slice(&0xBAD_F00Du32.to_le_bytes()); // wrong magic
    meta[4..6].copy_from_slice(&1u16.to_le_bytes());
    meta[6..10].copy_from_slice(&0u32.to_le_bytes());
    fs::write(bad_seg.join("meta.bin"), &meta).unwrap();

    fs::write(bad_seg.join("paths.bin"), b"").unwrap();
    fs::write(bad_seg.join("content.zst"), b"").unwrap();

    let segments = recover_segments(&segments_dir).unwrap();

    assert!(segments.is_empty());
}
```

### Step 2: Run the tests

Run: `cargo test -p ferret-indexer-core -- test_recover_skips_segment_bad -v`

Expected: PASS

### Step 3: Commit

```bash
git add ferret-indexer-core/src/recovery.rs
git commit -m "test(recovery): add tests for skipping segments with corrupted headers"
```

---

## Task 7: Add test for lock file cleanup

**Files:**
- Modify: `ferret-indexer-core/src/recovery.rs`

### Step 1: Write the tests

Add to the `tests` module:

```rust
#[test]
fn test_cleanup_lock_file_removes_stale_lock() {
    let dir = tempfile::tempdir().unwrap();
    let ferret_dir = dir.path().join(".ferret_index");
    fs::create_dir_all(&ferret_dir).unwrap();

    // Create a stale lock file
    fs::write(ferret_dir.join("lock"), b"12345").unwrap();
    assert!(ferret_dir.join("lock").exists());

    cleanup_lock_file(&ferret_dir).unwrap();

    assert!(!ferret_dir.join("lock").exists());
}

#[test]
fn test_cleanup_lock_file_no_lock() {
    let dir = tempfile::tempdir().unwrap();
    let ferret_dir = dir.path().join(".ferret_index");
    fs::create_dir_all(&ferret_dir).unwrap();

    // No lock file exists — should succeed without error
    cleanup_lock_file(&ferret_dir).unwrap();
}

#[test]
fn test_cleanup_lock_file_dir_not_exist() {
    let dir = tempfile::tempdir().unwrap();
    let ferret_dir = dir.path().join(".ferret_index");
    // Directory doesn't exist — lock file can't exist, should succeed

    cleanup_lock_file(&ferret_dir).unwrap();
}
```

### Step 2: Run the tests

Run: `cargo test -p ferret-indexer-core -- test_cleanup_lock_file -v`

Expected: PASS

### Step 3: Commit

```bash
git add ferret-indexer-core/src/recovery.rs
git commit -m "test(recovery): add lock file cleanup tests"
```

---

## Task 8: Add test for nonexistent segments directory

**Files:**
- Modify: `ferret-indexer-core/src/recovery.rs`

### Step 1: Write the test

Add to the `tests` module:

```rust
#[test]
fn test_recover_nonexistent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let segments_dir = dir.path().join("nonexistent/segments");

    // Should return empty vec, not error
    let segments = recover_segments(&segments_dir).unwrap();
    assert!(segments.is_empty());
}
```

### Step 2: Run the test

Run: `cargo test -p ferret-indexer-core -- test_recover_nonexistent_dir -v`

Expected: PASS

### Step 3: Commit

```bash
git add ferret-indexer-core/src/recovery.rs
git commit -m "test(recovery): add test for nonexistent segments directory"
```

---

## Task 9: Add test for mixed temp dirs, valid segments, and invalid segments

**Files:**
- Modify: `ferret-indexer-core/src/recovery.rs`

### Step 1: Write the integration-style test

Add to the `tests` module:

```rust
#[test]
fn test_recover_mixed_state() {
    let dir = tempfile::tempdir().unwrap();
    let segments_dir = dir.path().join("segments");
    fs::create_dir_all(&segments_dir).unwrap();

    // 1. A leftover temp directory
    fs::create_dir_all(segments_dir.join(".seg_0000_tmp_42")).unwrap();
    fs::write(
        segments_dir.join(".seg_0000_tmp_42/trigrams.bin"),
        b"partial",
    )
    .unwrap();

    // 2. A valid segment at ID 1
    let writer1 = SegmentWriter::new(&segments_dir, SegmentId(1));
    writer1
        .build(vec![InputFile {
            path: "hello.rs".to_string(),
            content: b"fn hello() { let msg = \"hi\"; }".to_vec(),
            mtime: 100,
        }])
        .unwrap();

    // 3. An invalid segment at ID 2 (empty directory, missing all files)
    fs::create_dir_all(segments_dir.join("seg_0002")).unwrap();

    // 4. A valid segment at ID 3
    let writer3 = SegmentWriter::new(&segments_dir, SegmentId(3));
    writer3
        .build(vec![InputFile {
            path: "world.rs".to_string(),
            content: b"fn world() { let w = true; }".to_vec(),
            mtime: 200,
        }])
        .unwrap();

    // 5. A random non-segment file (should be ignored)
    fs::write(segments_dir.join("README.txt"), b"ignore me").unwrap();

    let segments = recover_segments(&segments_dir).unwrap();

    // Should recover exactly 2 valid segments, sorted by ID
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].segment_id(), SegmentId(1));
    assert_eq!(segments[1].segment_id(), SegmentId(3));

    // Temp directory should be cleaned up
    assert!(!segments_dir.join(".seg_0000_tmp_42").exists());

    // Invalid segment dir and non-segment file should still exist
    assert!(segments_dir.join("seg_0002").exists());
    assert!(segments_dir.join("README.txt").exists());
}
```

### Step 2: Run the test

Run: `cargo test -p ferret-indexer-core -- test_recover_mixed_state -v`

Expected: PASS

### Step 3: Commit

```bash
git add ferret-indexer-core/src/recovery.rs
git commit -m "test(recovery): add integration test for mixed startup state"
```

---

## Task 10: Add test for parse_segment_id edge cases

**Files:**
- Modify: `ferret-indexer-core/src/recovery.rs`

### Step 1: Write the tests

Add to the `tests` module:

```rust
#[test]
fn test_parse_segment_id_valid() {
    assert_eq!(parse_segment_id("seg_0000"), Some(SegmentId(0)));
    assert_eq!(parse_segment_id("seg_0001"), Some(SegmentId(1)));
    assert_eq!(parse_segment_id("seg_0042"), Some(SegmentId(42)));
    assert_eq!(parse_segment_id("seg_9999"), Some(SegmentId(9999)));
    // Larger IDs work too (not zero-padded to 4)
    assert_eq!(parse_segment_id("seg_12345"), Some(SegmentId(12345)));
}

#[test]
fn test_parse_segment_id_invalid() {
    assert_eq!(parse_segment_id("not_a_segment"), None);
    assert_eq!(parse_segment_id("seg_"), None);
    assert_eq!(parse_segment_id("seg_abc"), None);
    assert_eq!(parse_segment_id("segment_0001"), None);
    assert_eq!(parse_segment_id(""), None);
    assert_eq!(parse_segment_id("seg_-1"), None);
    assert_eq!(parse_segment_id(".seg_0000_tmp_123"), None);
    assert_eq!(parse_segment_id("README.txt"), None);
}
```

### Step 2: Run the tests

Run: `cargo test -p ferret-indexer-core -- test_parse_segment_id -v`

Expected: PASS

### Step 3: Commit

```bash
git add ferret-indexer-core/src/recovery.rs
git commit -m "test(recovery): add parse_segment_id edge case tests"
```

---

## Task 11: Add test for empty segments directory

**Files:**
- Modify: `ferret-indexer-core/src/recovery.rs`

### Step 1: Write the test

Add to the `tests` module:

```rust
#[test]
fn test_recover_empty_segments_dir() {
    let dir = tempfile::tempdir().unwrap();
    let segments_dir = dir.path().join("segments");
    fs::create_dir_all(&segments_dir).unwrap();

    let segments = recover_segments(&segments_dir).unwrap();
    assert!(segments.is_empty());
}
```

### Step 2: Run the test

Run: `cargo test -p ferret-indexer-core -- test_recover_empty_segments_dir -v`

Expected: PASS

### Step 3: Commit

```bash
git add ferret-indexer-core/src/recovery.rs
git commit -m "test(recovery): add test for empty segments directory"
```

---

## Task 12: Final verification

### Step 1: Run full test suite

Run: `cargo test --workspace`

Expected: All tests pass (existing + new recovery tests).

### Step 2: Run lints and formatting

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: No warnings, formatting OK.

### Step 3: Verify the module structure

At this point, `ferret-indexer-core/src/recovery.rs` should contain:

**Public functions:**
- `recover_segments(segments_dir: &Path) -> Result<Vec<Segment>, IndexError>` -- scans segments dir, cleans temp dirs, validates and loads segments, returns sorted by ID
- `cleanup_lock_file(ferret_dir: &Path) -> Result<(), IndexError>` -- removes stale `.ferret_index/lock`

**Private helpers:**
- `parse_segment_id(name: &str) -> Option<SegmentId>` -- parses `seg_NNNN` -> `SegmentId`
- `validate_segment_files(seg_dir: &Path) -> Result<(), String>` -- checks required files exist
- `validate_segment_headers(seg_dir: &Path) -> Result<(), String>` -- validates magic + version in trigrams.bin and meta.bin
- `validate_trigram_header(path: &Path) -> Result<(), String>` -- reads first 6 bytes of trigrams.bin
- `validate_meta_header(path: &Path) -> Result<(), String>` -- reads first 6 bytes of meta.bin

And `lib.rs` should re-export: `recover_segments`, `cleanup_lock_file`.

### Step 4: Commit if any cleanup was needed

```bash
git add -A
git commit -m "chore(recovery): final cleanup for crash recovery implementation"
```

---

## Reference: Temp Directory Naming Convention

The `SegmentWriter::build()` method in `segment.rs` (line 197-199) creates temp directories with this pattern:

```rust
let temp_dir = self.base_dir.join(format!(".{seg_name}_tmp_{}", std::process::id()));
// Example: .seg_0000_tmp_12345
```

The recovery module detects these by checking: `name.starts_with(".seg_") && name.contains("_tmp_")`.

## Reference: Required Segment Files

A valid segment directory must contain:
- `trigrams.bin` -- magic: `0x54524947` ("TRIG"), version: `1`
- `meta.bin` -- magic: `0x4D455441` ("META"), version: `1`
- `paths.bin` -- path string pool (no magic header)
- `content.zst` -- zstd compressed content (no magic header)
- `tombstones.bin` -- optional (may be empty or absent for new segments)

## Reference: On-Disk Layout

```
.ferret_index/
  lock                    # PID lock file (stale if process crashed)
  segments/
    seg_0000/             # Valid segment
      trigrams.bin
      meta.bin
      paths.bin
      content.zst
      tombstones.bin
    seg_0001/             # Valid segment
      ...
    .seg_0002_tmp_12345/  # Temp dir from crashed build (should be cleaned up)
      trigrams.bin
      meta.bin            # Possibly incomplete
```
