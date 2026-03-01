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
//! use indexrs_core::walker::DirectoryWalkerBuilder;
//!
//! let files = DirectoryWalkerBuilder::new("/path/to/repo")
//!     .build()
//!     .run()
//!     .unwrap();
//!
//! for f in &files {
//!     println!("{}", f.path.display());
//! }
//! ```

use std::fs::Metadata;
use std::path::PathBuf;
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
        builder.follow_links(false);

        // Honour .indexrsignore files.
        builder.add_custom_ignore_filename(".indexrsignore");

        // Thread count for parallel mode.
        builder.threads(self.threads);

        // Hard-skip .git/ and .indexrs/ directories, plus apply extra excludes.
        let extra = self.extra_ignore_patterns.clone();
        builder.filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().is_some_and(|ft| ft.is_dir())
                && (name == ".git" || name == ".indexrs")
            {
                return false;
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
///
/// Uses a linear-time two-pointer algorithm instead of O(p*t) DP.
fn simple_glob(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0usize);

    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Configured directory walker, ready to execute.
pub struct Walker {
    builder: WalkBuilder,
}

impl Walker {
    /// Walk the directory tree sequentially, returning all discovered files.
    pub fn run(self) -> Result<Vec<WalkedFile>> {
        self.run_with_progress(|_| {})
    }

    /// Walk the directory tree sequentially, calling `on_file(count)` after
    /// each file is discovered (where `count` is the running total).
    pub fn run_with_progress<F: FnMut(usize)>(self, mut on_file: F) -> Result<Vec<WalkedFile>> {
        let mut files = Vec::new();
        for entry in self.builder.build() {
            let entry = entry.map_err(|e| IndexError::Walk(e.to_string()))?;
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|e| IndexError::Walk(e.to_string()))?;
            files.push(WalkedFile {
                path: entry.into_path(),
                metadata,
            });
            on_file(files.len());
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
    use std::path::Path;
    use tempfile::TempDir;

    /// Helper: create a file with content, creating parent directories as needed.
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
