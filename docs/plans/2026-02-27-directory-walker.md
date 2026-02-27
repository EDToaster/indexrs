# Directory Walker with .gitignore Support Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create a configurable, parallel directory walker that respects `.gitignore` rules and returns `(PathBuf, std::fs::Metadata)` pairs for every discovered file.

**Architecture:** Single module `walker.rs` exposes a `WalkerBuilder` (config) and a `Walker` (iterator). `WalkerBuilder` wraps `ignore::WalkBuilder`, adding `.indexrsignore` as a custom ignore filename and hard-skipping `.git/` and `.indexrs/` directories. The `Walker::run` method collects entries sequentially; `Walker::run_parallel` uses `ignore::WalkParallel` for multi-threaded traversal. Both return `Vec<WalkedFile>` where `WalkedFile { path, metadata }`.

**Tech Stack:** Rust 2024, `ignore` 0.4 (already in Cargo.toml), `thiserror` (existing `IndexError`), `tempfile` (dev-dependency, already present)

---

## Task 1: Add `WalkError` variant to `IndexError`

**Files:**
- Modify: `indexrs-core/src/error.rs`

**Step 1: Add the new variant**

In `indexrs-core/src/error.rs`, add a `Walk` variant to `IndexError` that wraps a `String`:

```rust
/// An error occurred while walking the directory tree.
#[error("walk error: {0}")]
Walk(String),
```

Add it after the existing `SegmentNotFound` variant.

**Step 2: Verify compilation**

Run: `cargo check -p indexrs-core`
Expected: success

**Step 3: Commit**

```bash
git add indexrs-core/src/error.rs
git commit -m "feat(walker): add Walk variant to IndexError"
```

---

## Task 2: Create `walker.rs` with types and `WalkerBuilder`

**Files:**
- Create: `indexrs-core/src/walker.rs`
- Modify: `indexrs-core/src/lib.rs`

**Step 1: Create walker.rs with core types and builder**

Create `indexrs-core/src/walker.rs`:

```rust
//! Directory walker with `.gitignore` support.
//!
//! Uses the [`ignore`] crate (from the ripgrep ecosystem) to recursively walk a
//! repository tree while respecting `.gitignore`, `.git/info/exclude`, global
//! gitignore, negation patterns, directory-only patterns, and nested `.gitignore`
//! files.  An additional `.indexrsignore` file is also honoured.
//!
//! # Quick start
//!
//! ```no_run
//! use indexrs_core::walker::WalkerBuilder;
//!
//! let files = WalkerBuilder::new("/path/to/repo")
//!     .build()
//!     .run()
//!     .unwrap();
//!
//! for f in &files {
//!     println!("{}", f.path.display());
//! }
//! ```

use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ignore::WalkBuilder;
use ignore::WalkState;

use crate::error::{IndexError, Result};

/// A file discovered by the directory walker.
#[derive(Debug)]
pub struct WalkedFile {
    /// Absolute or relative path to the file (as returned by the walker).
    pub path: PathBuf,
    /// Filesystem metadata (`len`, `modified`, `is_file`, etc.).
    pub metadata: Metadata,
}

/// Builder for configuring a directory [`Walker`].
///
/// Wraps [`ignore::WalkBuilder`] with sensible defaults for indexrs:
/// - `.git/` and `.indexrs/` directories are always skipped
/// - `.indexrsignore` files are honoured (highest precedence among ignore files)
/// - All standard gitignore sources are enabled
pub struct DirectoryWalkerBuilder {
    root: PathBuf,
    extra_ignore_patterns: Vec<String>,
    threads: usize,
}

impl DirectoryWalkerBuilder {
    /// Create a new walker builder rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            extra_ignore_patterns: Vec::new(),
            threads: 0, // 0 = automatic (uses available cores)
        }
    }

    /// Add an extra glob pattern to exclude (e.g. `"*.log"`).
    pub fn add_exclude(&mut self, pattern: impl Into<String>) -> &mut Self {
        self.extra_ignore_patterns.push(pattern.into());
        self
    }

    /// Set the number of threads for parallel walking.
    /// `0` means automatic (one per CPU core).
    pub fn threads(&mut self, n: usize) -> &mut Self {
        self.threads = n;
        self
    }

    /// Build a [`Walker`] from the current configuration.
    pub fn build(&self) -> Walker {
        let mut builder = WalkBuilder::new(&self.root);

        // Enable all standard gitignore sources (these are on by default,
        // but be explicit for clarity).
        builder.git_ignore(true);
        builder.git_global(true);
        builder.git_exclude(true);
        builder.hidden(true);

        // Honour .indexrsignore files.
        builder.add_custom_ignore_filename(".indexrsignore");

        // Thread count for parallel mode.
        builder.threads(self.threads);

        // Hard-skip .git/ and .indexrs/ directories, plus apply extra excludes.
        let extra = self.extra_ignore_patterns.clone();
        builder.filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                if name == ".git" || name == ".indexrs" {
                    return false;
                }
            }
            // Apply extra exclude patterns (simple glob matching).
            if !extra.is_empty() {
                let path_str = entry.path().to_string_lossy();
                for pattern in &extra {
                    if glob_matches(pattern, &path_str, &name) {
                        return false;
                    }
                }
            }
            true
        });

        Walker { builder }
    }
}

/// Simple glob matcher supporting `*` (any chars) and `?` (single char) patterns.
///
/// Matches against both the full path and the filename component.
fn glob_matches(pattern: &str, path: &str, filename: &str) -> bool {
    simple_glob(pattern, filename) || simple_glob(pattern, path)
}

/// Minimal glob matching: `*` matches any sequence, `?` matches one char.
fn simple_glob(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (plen, tlen) = (pattern.len(), text.len());
    // dp[i][j] = pattern[..i] matches text[..j]
    let mut dp = vec![vec![false; tlen + 1]; plen + 1];
    dp[0][0] = true;
    for i in 1..=plen {
        if pattern[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=plen {
        for j in 1..=tlen {
            if pattern[i - 1] == '*' {
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            } else if pattern[i - 1] == '?' || pattern[i - 1] == text[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }
    dp[plen][tlen]
}

/// Configured directory walker, ready to execute.
pub struct Walker {
    builder: WalkBuilder,
}

impl Walker {
    /// Walk the directory tree sequentially, returning all discovered files.
    pub fn run(self) -> Result<Vec<WalkedFile>> {
        let mut files = Vec::new();
        for entry in self.builder.build() {
            let entry = entry.map_err(|e| IndexError::Walk(e.to_string()))?;
            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|e| IndexError::Walk(e.to_string()))?;
            files.push(WalkedFile {
                path: entry.into_path(),
                metadata,
            });
        }
        Ok(files)
    }

    /// Walk the directory tree in parallel, returning all discovered files.
    ///
    /// Files are returned in arbitrary order (non-deterministic).
    pub fn run_parallel(self) -> Result<Vec<WalkedFile>> {
        let files: Mutex<Vec<WalkedFile>> = Mutex::new(Vec::new());
        let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());

        self.builder.build_parallel().run(|| {
            Box::new(|entry| {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        errors.lock().unwrap().push(e.to_string());
                        return WalkState::Continue;
                    }
                };
                if !entry.file_type().map_or(false, |ft| ft.is_file()) {
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
                WalkState::Continue
            })
        });

        let errs = errors.into_inner().unwrap();
        if !errs.is_empty() {
            return Err(IndexError::Walk(errs.join("; ")));
        }
        Ok(files.into_inner().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: create a file with content.
    fn create_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }

    // ---------------------------------------------------------------
    // Basic walking
    // ---------------------------------------------------------------

    #[test]
    fn test_walk_basic_files() {
        let tmp = TempDir::new().unwrap();
        create_file(tmp.path(), "a.rs", "fn main() {}");
        create_file(tmp.path(), "b.txt", "hello");
        create_file(tmp.path(), "sub/c.rs", "mod c;");

        let files = DirectoryWalkerBuilder::new(tmp.path())
            .build()
            .run()
            .unwrap();

        let mut names: Vec<String> = files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.rs", "b.txt", "sub/c.rs"]);
    }

    #[test]
    fn test_walk_returns_metadata() {
        let tmp = TempDir::new().unwrap();
        create_file(tmp.path(), "hello.txt", "12345");

        let files = DirectoryWalkerBuilder::new(tmp.path())
            .build()
            .run()
            .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].metadata.len(), 5);
        assert!(files[0].metadata.is_file());
    }

    // ---------------------------------------------------------------
    // .gitignore support
    // ---------------------------------------------------------------

    #[test]
    fn test_gitignore_respected() {
        let tmp = TempDir::new().unwrap();

        // Initialize a git repo so .gitignore is actually honoured.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        create_file(tmp.path(), ".gitignore", "*.log\n");
        create_file(tmp.path(), "keep.rs", "fn main() {}");
        create_file(tmp.path(), "debug.log", "should be ignored");

        let files = DirectoryWalkerBuilder::new(tmp.path())
            .build()
            .run()
            .unwrap();

        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert!(names.contains(&"keep.rs".to_string()));
        assert!(
            !names.contains(&"debug.log".to_string()),
            "debug.log should be ignored by .gitignore"
        );
    }

    // ---------------------------------------------------------------
    // .git/ and .indexrs/ skipping
    // ---------------------------------------------------------------

    #[test]
    fn test_git_dir_skipped() {
        let tmp = TempDir::new().unwrap();
        create_file(tmp.path(), "keep.rs", "fn main() {}");
        create_file(tmp.path(), ".git/config", "[core]");
        create_file(tmp.path(), ".git/HEAD", "ref: refs/heads/main");

        let files = DirectoryWalkerBuilder::new(tmp.path())
            .build()
            .run()
            .unwrap();

        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["keep.rs"]);
    }

    #[test]
    fn test_indexrs_dir_skipped() {
        let tmp = TempDir::new().unwrap();
        create_file(tmp.path(), "keep.rs", "fn main() {}");
        create_file(tmp.path(), ".indexrs/index.dat", "binary data");

        let files = DirectoryWalkerBuilder::new(tmp.path())
            .build()
            .run()
            .unwrap();

        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["keep.rs"]);
    }

    // ---------------------------------------------------------------
    // .indexrsignore support
    // ---------------------------------------------------------------

    #[test]
    fn test_indexrsignore_respected() {
        let tmp = TempDir::new().unwrap();
        create_file(tmp.path(), ".indexrsignore", "*.dat\n");
        create_file(tmp.path(), "keep.rs", "fn main() {}");
        create_file(tmp.path(), "data.dat", "should be ignored");

        let files = DirectoryWalkerBuilder::new(tmp.path())
            .build()
            .run()
            .unwrap();

        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert!(names.contains(&"keep.rs".to_string()));
        assert!(
            !names.contains(&"data.dat".to_string()),
            "data.dat should be ignored by .indexrsignore"
        );
    }

    // ---------------------------------------------------------------
    // Extra exclude patterns
    // ---------------------------------------------------------------

    #[test]
    fn test_extra_exclude_patterns() {
        let tmp = TempDir::new().unwrap();
        create_file(tmp.path(), "keep.rs", "fn main() {}");
        create_file(tmp.path(), "debug.log", "log line");
        create_file(tmp.path(), "output.tmp", "temp");

        let files = DirectoryWalkerBuilder::new(tmp.path())
            .add_exclude("*.log")
            .add_exclude("*.tmp")
            .build()
            .run()
            .unwrap();

        let names: Vec<String> = files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["keep.rs"]);
    }

    // ---------------------------------------------------------------
    // Parallel walking
    // ---------------------------------------------------------------

    #[test]
    fn test_parallel_walk_finds_all_files() {
        let tmp = TempDir::new().unwrap();
        create_file(tmp.path(), "a.rs", "fn a() {}");
        create_file(tmp.path(), "b.rs", "fn b() {}");
        create_file(tmp.path(), "sub/c.rs", "fn c() {}");

        let files = DirectoryWalkerBuilder::new(tmp.path())
            .threads(2)
            .build()
            .run_parallel()
            .unwrap();

        let mut names: Vec<String> = files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.rs", "b.rs", "sub/c.rs"]);
    }

    // ---------------------------------------------------------------
    // Empty directory
    // ---------------------------------------------------------------

    #[test]
    fn test_empty_directory() {
        let tmp = TempDir::new().unwrap();

        let files = DirectoryWalkerBuilder::new(tmp.path())
            .build()
            .run()
            .unwrap();

        assert!(files.is_empty());
    }

    // ---------------------------------------------------------------
    // simple_glob unit tests
    // ---------------------------------------------------------------

    #[test]
    fn test_simple_glob_star() {
        assert!(simple_glob("*.rs", "main.rs"));
        assert!(!simple_glob("*.rs", "main.py"));
        assert!(simple_glob("*", "anything"));
    }

    #[test]
    fn test_simple_glob_question() {
        assert!(simple_glob("?.rs", "a.rs"));
        assert!(!simple_glob("?.rs", "ab.rs"));
    }

    #[test]
    fn test_simple_glob_exact() {
        assert!(simple_glob("foo.txt", "foo.txt"));
        assert!(!simple_glob("foo.txt", "bar.txt"));
    }
}
```

**Step 2: Register the module in lib.rs**

Add `pub mod walker;` to `indexrs-core/src/lib.rs` and add a re-export:

```rust
pub mod walker;

// In the re-exports section:
pub use walker::{DirectoryWalkerBuilder, WalkedFile, Walker};
```

**Step 3: Verify compilation**

Run: `cargo check -p indexrs-core`
Expected: success

**Step 4: Run tests**

Run: `cargo test -p indexrs-core -- walker`
Expected: all tests pass

**Step 5: Run clippy**

Run: `cargo clippy -p indexrs-core -- -D warnings`
Expected: no warnings

**Step 6: Commit**

```bash
git add indexrs-core/src/walker.rs indexrs-core/src/lib.rs indexrs-core/src/error.rs
git commit -m "feat(walker): add directory walker with .gitignore support

Adds DirectoryWalkerBuilder / Walker that wraps the ignore crate to walk
a repository tree while respecting .gitignore, .git/info/exclude, global
gitignore, and a custom .indexrsignore file.  Supports both sequential
and parallel traversal.  Skips .git/ and .indexrs/ directories.

Ref: HHC-35"
```

---

## Summary

| # | What | Files |
|---|------|-------|
| 1 | Add `Walk` error variant | `error.rs` |
| 2 | Implement `walker.rs`, register in `lib.rs`, tests | `walker.rs`, `lib.rs` |
