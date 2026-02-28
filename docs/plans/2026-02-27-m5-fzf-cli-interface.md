# M5: fzf CLI Interface — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the `indexrs files`, `indexrs search`, and `indexrs preview` subcommands with ANSI-colored, streaming, fzf-friendly output, plus SIGPIPE handling, an on-demand daemon, and shell integration recipes.

**Architecture:** The CLI (`indexrs-cli`) gets new modules for color formatting (`color.rs`), streaming output (`output.rs`), and repo/index discovery (`repo.rs`). Each subcommand lives in its own module (`files.rs`, `search_cmd.rs`, `preview.rs`). The daemon (`daemon.rs`) listens on a Unix domain socket, auto-starts on first CLI query, and idles out after 5 minutes. All commands work in both direct mode (no daemon) and daemon mode.

**Tech Stack:** Rust, clap 4, tokio, nu-ansi-term (ANSI colors), libc (SIGPIPE), globset (path filtering), syntect (preview fallback), serde_json (daemon protocol), indexrs-core (all indexing/search APIs).

**Linear Issues:** HHC-52, HHC-53, HHC-54, HHC-55, HHC-56, HHC-57

**Design Doc:** `docs/design/fzf-interface.md`

---

### Task 1: Add CLI Dependencies and Create Color Module (HHC-55)

**Files:**
- Modify: `indexrs-cli/Cargo.toml`
- Create: `indexrs-cli/src/color.rs`
- Modify: `indexrs-cli/src/main.rs` (add `mod color;`)

**Step 1: Write tests for the color module**

Create `indexrs-cli/src/color.rs` with tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_color_config_from_always() {
        let config = ColorConfig::new(true);
        assert!(config.enabled);
    }

    #[test]
    fn test_color_config_from_never() {
        let config = ColorConfig::new(false);
        assert!(!config.enabled);
    }

    #[test]
    fn test_format_file_path_no_color() {
        let config = ColorConfig::new(false);
        assert_eq!(config.format_file_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_format_file_path_with_color() {
        let config = ColorConfig::new(true);
        let result = config.format_file_path("src/main.rs");
        // Should contain ANSI escape codes
        assert!(result.contains("\x1b["));
        // Should still contain the path components
        assert!(result.contains("src/"));
        assert!(result.contains("main"));
        assert!(result.contains(".rs"));
    }

    #[test]
    fn test_format_file_path_no_extension() {
        let config = ColorConfig::new(true);
        let result = config.format_file_path("Makefile");
        assert!(result.contains("Makefile"));
    }

    #[test]
    fn test_format_file_path_no_directory() {
        let config = ColorConfig::new(true);
        let result = config.format_file_path("main.rs");
        assert!(result.contains("main"));
    }

    #[test]
    fn test_format_search_line_no_color() {
        let config = ColorConfig::new(false);
        let result = config.format_search_line("src/main.rs", 10, 5, "let x = 1;", &[]);
        assert_eq!(result, "src/main.rs:10:5:let x = 1;");
    }

    #[test]
    fn test_format_search_line_with_color() {
        let config = ColorConfig::new(true);
        let result = config.format_search_line("src/main.rs", 10, 5, "let x = 1;", &[(4, 5)]);
        assert!(result.contains("\x1b["));
    }

    #[test]
    fn test_highlight_ranges_in_content() {
        let config = ColorConfig::new(true);
        let result = config.highlight_ranges("hello world", &[(0, 5)]);
        assert!(result.contains("\x1b["));
        assert!(result.contains("hello"));
        assert!(result.contains("world"));
    }

    #[test]
    fn test_highlight_ranges_no_color() {
        let config = ColorConfig::new(false);
        let result = config.highlight_ranges("hello world", &[(0, 5)]);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_highlight_ranges_multiple() {
        let config = ColorConfig::new(true);
        let result = config.highlight_ranges("aXbXc", &[(1, 2), (3, 4)]);
        assert!(result.contains("a"));
        assert!(result.contains("b"));
        assert!(result.contains("c"));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p indexrs-cli -- color`
Expected: compilation error — `ColorConfig` not defined yet.

**Step 3: Add dependency and implement color module**

Add to `indexrs-cli/Cargo.toml`:

```toml
nu-ansi-term = "0.50"
```

Implement `indexrs-cli/src/color.rs`:

```rust
use nu_ansi_term::{Color, Style};

/// Configuration for ANSI color output.
pub struct ColorConfig {
    pub enabled: bool,
}

impl ColorConfig {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Format a file path with ANSI colors: dim directories, bold filename, cyan extension.
    pub fn format_file_path(&self, path: &str) -> String {
        if !self.enabled {
            return path.to_string();
        }

        // Split into directory and filename
        let (dir, file) = match path.rfind('/') {
            Some(pos) => (&path[..=pos], &path[pos + 1..]),
            None => ("", path),
        };

        // Split filename into stem and extension
        let (stem, ext) = match file.rfind('.') {
            Some(pos) => (&file[..pos], &file[pos..]), // includes the dot
            None => (file, ""),
        };

        let mut result = String::new();
        if !dir.is_empty() {
            result.push_str(&Style::new().dimmed().paint(dir).to_string());
        }
        result.push_str(&Style::new().bold().paint(stem).to_string());
        if !ext.is_empty() {
            result.push_str(&Color::Cyan.paint(ext).to_string());
        }
        result
    }

    /// Format a vimgrep-style search output line.
    ///
    /// Format: `file:line:col:content` with ANSI colors when enabled.
    /// Colors: magenta path, green line/col numbers, red bold match highlights.
    pub fn format_search_line(
        &self,
        path: &str,
        line: u32,
        col: usize,
        content: &str,
        ranges: &[(usize, usize)],
    ) -> String {
        if !self.enabled {
            return format!("{path}:{line}:{col}:{content}");
        }

        let colored_path = Color::Magenta.paint(path).to_string();
        let colored_line = Color::Green.paint(line.to_string()).to_string();
        let colored_col = Color::Green.paint(col.to_string()).to_string();
        let colored_content = self.highlight_ranges(content, ranges);

        format!("{colored_path}:{colored_line}:{colored_col}:{colored_content}")
    }

    /// Highlight byte ranges in content with red bold.
    pub fn highlight_ranges(&self, content: &str, ranges: &[(usize, usize)]) -> String {
        if !self.enabled || ranges.is_empty() {
            return content.to_string();
        }

        let mut result = String::new();
        let mut last_end = 0;
        let style = Style::new().bold().fg(Color::Red);

        for &(start, end) in ranges {
            let start = start.min(content.len());
            let end = end.min(content.len());
            if start > last_end {
                result.push_str(&content[last_end..start]);
            }
            result.push_str(&style.paint(&content[start..end]).to_string());
            last_end = end;
        }
        if last_end < content.len() {
            result.push_str(&content[last_end..]);
        }
        result
    }
}
```

Add `mod color;` to `main.rs`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p indexrs-cli -- color`
Expected: all tests PASS.

**Step 5: Commit**

```bash
git add indexrs-cli/src/color.rs indexrs-cli/src/main.rs indexrs-cli/Cargo.toml
git commit -m "feat(cli): add color module with ANSI formatting for files and search output (HHC-55)"
```

---

### Task 2: Streaming Writer, SIGPIPE Handling, and Exit Codes (HHC-55)

**Files:**
- Modify: `indexrs-cli/Cargo.toml` (add `libc`)
- Create: `indexrs-cli/src/output.rs`
- Modify: `indexrs-cli/src/main.rs` (add `mod output;`, SIGPIPE setup)

**Step 1: Write tests for StreamingWriter and ExitCode**

Create `indexrs-cli/src/output.rs` with tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_code_values() {
        assert_eq!(ExitCode::Success as i32, 0);
        assert_eq!(ExitCode::NoResults as i32, 1);
        assert_eq!(ExitCode::Error as i32, 2);
    }

    #[test]
    fn test_streaming_writer_to_vec() {
        let mut buf = Vec::new();
        {
            let mut writer = StreamingWriter::new(&mut buf);
            writer.write_line("hello").unwrap();
            writer.write_line("world").unwrap();
            writer.finish().unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "hello\nworld\n");
    }

    #[test]
    fn test_streaming_writer_count() {
        let mut buf = Vec::new();
        let mut writer = StreamingWriter::new(&mut buf);
        assert_eq!(writer.lines_written(), 0);
        writer.write_line("a").unwrap();
        assert_eq!(writer.lines_written(), 1);
        writer.write_line("b").unwrap();
        assert_eq!(writer.lines_written(), 2);
        writer.finish().unwrap();
    }

    #[test]
    fn test_streaming_writer_empty() {
        let mut buf = Vec::new();
        let mut writer = StreamingWriter::new(&mut buf);
        writer.finish().unwrap();
        assert!(buf.is_empty());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p indexrs-cli -- output`
Expected: compilation error.

**Step 3: Add libc dependency and implement output module**

Add to `indexrs-cli/Cargo.toml`:

```toml
libc = "0.2"
```

Implement `indexrs-cli/src/output.rs`:

```rust
use std::io::{self, BufWriter, Write};

/// Process exit codes per the fzf convention.
///
/// - 0: Results found
/// - 1: No results found (not an error)
/// - 2: Error occurred
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    NoResults = 1,
    Error = 2,
}

/// Streaming line writer with per-line flush for the first N lines,
/// then batch flush for performance.
///
/// Generic over `W: Write` for testability (use `Vec<u8>` in tests,
/// `Stdout` in production).
pub struct StreamingWriter<W: Write> {
    writer: BufWriter<W>,
    count: usize,
    flush_threshold: usize,
}

impl<W: Write> StreamingWriter<W> {
    /// Create a new streaming writer wrapping the given output.
    pub fn new(inner: W) -> Self {
        Self {
            writer: BufWriter::new(inner),
            count: 0,
            flush_threshold: 1000,
        }
    }

    /// Write a single line (appends newline) and flush if below threshold.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        writeln!(self.writer, "{line}")?;
        self.count += 1;
        if self.count <= self.flush_threshold {
            self.writer.flush()?;
        }
        Ok(())
    }

    /// Return the number of lines written so far.
    pub fn lines_written(&self) -> usize {
        self.count
    }

    /// Flush any remaining buffered output.
    pub fn finish(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Install the default SIGPIPE handler so broken pipes exit cleanly.
///
/// When fzf kills a reload process or the user pipes to `head`, Rust's
/// default SIGPIPE handler prints an error. This restores the OS default
/// (immediate termination) for clean fzf integration.
pub fn setup_sigpipe() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
```

Add `mod output;` to `main.rs` and call `output::setup_sigpipe()` as the first line of `main()`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p indexrs-cli -- output`
Expected: all tests PASS.

**Step 5: Commit**

```bash
git add indexrs-cli/src/output.rs indexrs-cli/src/main.rs indexrs-cli/Cargo.toml
git commit -m "feat(cli): add streaming writer, SIGPIPE handling, and exit codes (HHC-55)"
```

---

### Task 3: Update CLI Arguments to Match Design Doc (HHC-52, HHC-53, HHC-54)

**Files:**
- Modify: `indexrs-cli/src/args.rs`

**Step 1: Run existing CLI parse tests (if any) as baseline**

Run: `cargo test -p indexrs-cli`
Expected: PASS (existing stubs compile).

**Step 2: Update args.rs with complete CLI options**

Replace the `Command` enum and add `SortOrder`:

```rust
use std::path::PathBuf;
use clap::{Parser, Subcommand, ValueEnum};

/// Local code search index — fast grep, file, and symbol search for your repositories.
#[derive(Debug, Parser)]
#[command(name = "indexrs", version, about = "Local code search index")]
pub struct Cli {
    /// Color output mode
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, global = true)]
    pub color: ColorMode,

    /// Repository root path (default: auto-detect from cwd)
    #[arg(short = 'r', long, value_name = "PATH", global = true)]
    pub repo: Option<PathBuf>,

    /// Increase verbosity (can repeat: -vv for debug)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

/// Color output mode
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ColorMode {
    /// Automatic: color when stdout is a TTY
    Auto,
    /// Always emit color codes
    Always,
    /// Never emit color codes
    Never,
}

/// Sort order for file listing
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum SortOrder {
    /// Sort by file path (default)
    #[default]
    Path,
    /// Sort by modification time (newest first)
    Modified,
    /// Sort by file size (largest first)
    Size,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Search code in indexed files (vimgrep-compatible output)
    Search {
        /// Search query string
        query: String,

        /// Interpret query as a regex pattern
        #[arg(long)]
        regex: bool,

        /// Force case-sensitive matching
        #[arg(long, conflicts_with_all = ["ignore_case", "smart_case"])]
        case_sensitive: bool,

        /// Force case-insensitive matching
        #[arg(short = 'i', long, conflicts_with_all = ["case_sensitive", "smart_case"])]
        ignore_case: bool,

        /// Smart case: case-sensitive if query has uppercase (default)
        #[arg(short = 'S', long, conflicts_with_all = ["case_sensitive", "ignore_case"])]
        smart_case: bool,

        /// Filter by programming language
        #[arg(short = 'l', long, value_name = "LANG")]
        language: Option<String>,

        /// Filter by path glob pattern
        #[arg(short, long, value_name = "PATTERN")]
        path: Option<String>,

        /// Maximum number of results
        #[arg(short = 'n', long, default_value_t = 1000)]
        limit: usize,

        /// Lines of context around matches
        #[arg(short = 'C', long)]
        context: Option<usize>,

        /// Print match statistics to stderr
        #[arg(long)]
        stats: bool,
    },

    /// List indexed files (one path per line, fd-compatible)
    Files {
        /// Optional query to filter file names
        query: Option<String>,

        /// Filter by programming language
        #[arg(short = 'l', long, value_name = "LANG")]
        language: Option<String>,

        /// Filter by path glob pattern
        #[arg(short, long, value_name = "PATTERN")]
        path: Option<String>,

        /// Maximum number of results
        #[arg(short = 'n', long)]
        limit: Option<usize>,

        /// Sort order
        #[arg(long, value_enum, default_value_t = SortOrder::Path)]
        sort: SortOrder,
    },

    /// Search symbols (functions, types, constants)
    Symbols {
        /// Optional query to filter symbols
        query: Option<String>,

        /// Filter by symbol kind (fn, struct, trait, enum, etc.)
        #[arg(short = 'k', long, value_name = "KIND")]
        kind: Option<String>,

        /// Filter by programming language
        #[arg(short = 'l', long, value_name = "LANG")]
        language: Option<String>,

        /// Maximum number of results
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },

    /// Preview file contents with syntax highlighting for fzf
    Preview {
        /// File to preview
        file: PathBuf,

        /// Center preview on this line
        #[arg(long)]
        line: Option<usize>,

        /// Lines of context above/below
        #[arg(short = 'C', long)]
        context: Option<usize>,

        /// Highlight this specific line
        #[arg(long)]
        highlight_line: Option<usize>,
    },

    /// Show index status (file count, last update, etc.)
    Status,

    /// Trigger reindex of the repository
    Reindex {
        /// Perform a full reindex (default: incremental)
        #[arg(long)]
        full: bool,
    },
}
```

**Step 3: Update main.rs match arms to accept new fields**

Update the `match cli.command` block in `main.rs` to destructure the new fields (still as TODOs for now — later tasks fill them in). The key changes:
- `Search` gains `regex`, `case_sensitive`, `ignore_case`, `smart_case`, `context`, `stats`
- `Files` gains `path`, `sort`
- `Preview` gains `highlight_line`

**Step 4: Verify compilation**

Run: `cargo check -p indexrs-cli`
Expected: compiles with no errors.

**Step 5: Commit**

```bash
git add indexrs-cli/src/args.rs indexrs-cli/src/main.rs
git commit -m "feat(cli): update CLI args to match fzf interface design doc (HHC-52, HHC-53, HHC-54)"
```

---

### Task 4: Repo Discovery Module

**Files:**
- Create: `indexrs-cli/src/repo.rs`
- Modify: `indexrs-cli/src/main.rs` (add `mod repo;`)

**Step 1: Write tests for repo discovery**

Create `indexrs-cli/src/repo.rs` with tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_find_repo_root_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_repo_root(Some(dir.path()));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path());
    }

    #[test]
    fn test_find_repo_root_with_indexrs_dir() {
        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        fs::create_dir(&indexrs_dir).unwrap();

        let subdir = dir.path().join("src").join("deep");
        fs::create_dir_all(&subdir).unwrap();

        let result = find_repo_root_from(&subdir);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path());
    }

    #[test]
    fn test_find_repo_root_with_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir(&git_dir).unwrap();

        let subdir = dir.path().join("src");
        fs::create_dir_all(&subdir).unwrap();

        let result = find_repo_root_from(&subdir);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path());
    }

    #[test]
    fn test_find_repo_root_prefers_indexrs_over_git() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::create_dir(dir.path().join(".indexrs")).unwrap();

        let result = find_repo_root_from(dir.path());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path());
    }

    #[test]
    fn test_find_repo_root_not_found() {
        let dir = tempfile::tempdir().unwrap();
        // No .git or .indexrs anywhere
        let result = find_repo_root_from(dir.path());
        assert!(result.is_err());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p indexrs-cli -- repo`
Expected: compilation error.

**Step 3: Implement repo discovery**

```rust
use std::path::{Path, PathBuf};
use indexrs_core::error::IndexError;
use indexrs_core::SegmentManager;

/// Find the repository root directory.
///
/// If `repo_arg` is provided, uses that directly.
/// Otherwise, walks up from the current directory looking for `.indexrs/` or `.git/`.
pub fn find_repo_root(repo_arg: Option<&Path>) -> Result<PathBuf, IndexError> {
    if let Some(repo) = repo_arg {
        return Ok(repo.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(IndexError::Io)?;
    find_repo_root_from(&cwd)
}

/// Walk up from `start` looking for `.indexrs/` or `.git/`.
fn find_repo_root_from(start: &Path) -> Result<PathBuf, IndexError> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".indexrs").is_dir() || dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(IndexError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not inside a git repository or indexrs project",
            )));
        }
    }
}

/// Load a SegmentManager from the `.indexrs/` directory inside repo_root.
///
/// Creates `.indexrs/segments/` if it doesn't exist.
pub fn load_index(repo_root: &Path) -> Result<SegmentManager, IndexError> {
    let indexrs_dir = repo_root.join(".indexrs");
    SegmentManager::new(&indexrs_dir)
}
```

Add `mod repo;` to `main.rs`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p indexrs-cli -- repo`
Expected: all tests PASS.

**Step 5: Commit**

```bash
git add indexrs-cli/src/repo.rs indexrs-cli/src/main.rs
git commit -m "feat(cli): add repo discovery module for finding index root (HHC-55)"
```

---

### Task 5: `indexrs files` Command (HHC-52)

**Files:**
- Modify: `indexrs-cli/Cargo.toml` (add `globset`)
- Create: `indexrs-cli/src/files.rs`
- Modify: `indexrs-cli/src/main.rs` (add `mod files;`, wire up command)

**Step 1: Write tests for the files command**

Create `indexrs-cli/src/files.rs` with tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use indexrs_core::segment::{InputFile, SegmentWriter};
    use indexrs_core::types::SegmentId;
    use indexrs_core::SegmentManager;
    use std::sync::Arc;

    /// Build an index with test files and return the SegmentManager.
    fn build_test_index(dir: &Path) -> SegmentManager {
        let indexrs_dir = dir.join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        let manager = SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {}\n".to_vec(),
                    mtime: 200,
                },
                InputFile {
                    path: "src/lib.rs".to_string(),
                    content: b"pub fn hello() {}\n".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "README.md".to_string(),
                    content: b"# Hello\n".to_vec(),
                    mtime: 300,
                },
                InputFile {
                    path: "tests/test.py".to_string(),
                    content: b"def test(): pass\n".to_vec(),
                    mtime: 150,
                },
            ])
            .unwrap();
        manager
    }

    #[test]
    fn test_list_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let files = collect_files(&snapshot, &FilesFilter::default()).unwrap();
        assert_eq!(files.len(), 4);
    }

    #[test]
    fn test_list_files_filter_language() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let filter = FilesFilter {
            language: Some("rust".to_string()),
            ..Default::default()
        };
        let files = collect_files(&snapshot, &filter).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.path.ends_with(".rs")));
    }

    #[test]
    fn test_list_files_filter_path_glob() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let filter = FilesFilter {
            path_glob: Some("src/*".to_string()),
            ..Default::default()
        };
        let files = collect_files(&snapshot, &filter).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.path.starts_with("src/")));
    }

    #[test]
    fn test_list_files_sort_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let files = collect_files(&snapshot, &FilesFilter::default()).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn test_list_files_sort_by_modified() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let filter = FilesFilter {
            sort: SortOrder::Modified,
            ..Default::default()
        };
        let files = collect_files(&snapshot, &filter).unwrap();
        // Most recent first
        assert!(files[0].mtime_epoch_secs >= files[1].mtime_epoch_secs);
    }

    #[test]
    fn test_list_files_with_limit() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let filter = FilesFilter {
            limit: Some(2),
            ..Default::default()
        };
        let files = collect_files(&snapshot, &filter).unwrap();
        assert_eq!(files.len(), 2);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p indexrs-cli -- files`
Expected: compilation error.

**Step 3: Add globset dependency and implement files module**

Add to `indexrs-cli/Cargo.toml`:

```toml
globset = "0.4"
```

Implement `indexrs-cli/src/files.rs`:

```rust
use std::collections::HashMap;
use std::path::Path;

use globset::{Glob, GlobMatcher};
use indexrs_core::error::IndexError;
use indexrs_core::index_state::SegmentList;
use indexrs_core::metadata::FileMetadata;
use indexrs_core::types::SegmentId;

use crate::args::SortOrder;
use crate::color::ColorConfig;
use crate::output::{ExitCode, StreamingWriter};

/// Filter options for the files command.
#[derive(Default)]
pub struct FilesFilter {
    pub language: Option<String>,
    pub path_glob: Option<String>,
    pub sort: SortOrder,
    pub limit: Option<usize>,
}

/// Collect all indexed files from the snapshot, applying filters and sorting.
///
/// Handles tombstone filtering and cross-segment deduplication (newest segment wins).
pub fn collect_files(
    snapshot: &SegmentList,
    filter: &FilesFilter,
) -> Result<Vec<FileMetadata>, IndexError> {
    let glob_matcher: Option<GlobMatcher> = filter
        .path_glob
        .as_ref()
        .map(|g| Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;

    // Collect files across segments, dedup by path (newest segment wins)
    let mut seen: HashMap<String, (SegmentId, FileMetadata)> = HashMap::new();

    for segment in snapshot.iter() {
        let tombstones = segment.load_tombstones()?;
        let reader = segment.metadata_reader()?;
        let seg_id = segment.segment_id();

        for entry in reader.iter_all() {
            let entry = entry?;
            if tombstones.contains(entry.file_id) {
                continue;
            }

            // Language filter
            if let Some(ref lang) = filter.language {
                if !entry.language.to_string().eq_ignore_ascii_case(lang) {
                    continue;
                }
            }

            // Path glob filter
            if let Some(ref matcher) = glob_matcher {
                if !matcher.is_match(&entry.path) {
                    continue;
                }
            }

            // Dedup: keep newest segment
            match seen.get(&entry.path) {
                Some((existing_seg, _)) if *existing_seg >= seg_id => continue,
                _ => {
                    seen.insert(entry.path.clone(), (seg_id, entry));
                }
            }
        }
    }

    let mut files: Vec<FileMetadata> = seen.into_values().map(|(_, meta)| meta).collect();

    // Sort
    match filter.sort {
        SortOrder::Path => files.sort_by(|a, b| a.path.cmp(&b.path)),
        SortOrder::Modified => files.sort_by(|a, b| b.mtime_epoch_secs.cmp(&a.mtime_epoch_secs)),
        SortOrder::Size => files.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes)),
    }

    // Limit
    if let Some(limit) = filter.limit {
        files.truncate(limit);
    }

    Ok(files)
}

/// Run the files command: collect, format, and stream file paths to stdout.
pub fn run_files<W: std::io::Write>(
    snapshot: &SegmentList,
    filter: &FilesFilter,
    color: &ColorConfig,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
    let files = collect_files(snapshot, filter)?;

    if files.is_empty() {
        return Ok(ExitCode::NoResults);
    }

    for file in &files {
        let line = color.format_file_path(&file.path);
        if writer.write_line(&line).is_err() {
            // Broken pipe (SIGPIPE) — exit silently
            break;
        }
    }
    let _ = writer.finish();

    Ok(ExitCode::Success)
}
```

Wire up in `main.rs`: replace the `Command::Files` arm with a call to `files::run_files(...)`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p indexrs-cli -- files`
Expected: all tests PASS.

**Step 5: Run clippy**

Run: `cargo clippy -p indexrs-cli -- -D warnings`
Expected: no warnings.

**Step 6: Commit**

```bash
git add indexrs-cli/src/files.rs indexrs-cli/src/main.rs indexrs-cli/Cargo.toml
git commit -m "feat(cli): implement 'indexrs files' with filtering, sorting, and ANSI colors (HHC-52)"
```

---

### Task 6: Add `search_segments_with_pattern_and_options` to Core

**Files:**
- Modify: `indexrs-core/src/multi_search.rs`
- Modify: `indexrs-core/src/lib.rs` (re-export)

The existing `search_segments_with_pattern` doesn't accept `SearchOptions` (no context lines, no max_results). The CLI search command needs both pattern control (regex, case sensitivity) AND options (context, limits).

**Step 1: Write test for the new function**

Add to `indexrs-core/src/multi_search.rs` tests:

```rust
#[test]
fn test_search_segments_with_pattern_and_options() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".indexrs/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"line one\nline two\nfn hello() {}\nline four\nline five\n".to_vec(),
            mtime: 0,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);
    let pattern = MatchPattern::LiteralCaseInsensitive("hello".to_string());
    let options = SearchOptions {
        context_lines: 1,
        max_results: None,
    };
    let result = search_segments_with_pattern_and_options(&snapshot, &pattern, &options).unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].lines[0].line_number, 3);
    // Should have context
    assert!(!result.files[0].lines[0].context_before.is_empty());
    assert!(!result.files[0].lines[0].context_after.is_empty());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p indexrs-core -- test_search_segments_with_pattern_and_options`
Expected: compilation error — function doesn't exist.

**Step 3: Implement `search_segments_with_pattern_and_options`**

Add to `indexrs-core/src/multi_search.rs` (after `search_segments_with_pattern`):

Refactor `search_single_segment_with_pattern` to accept a `context_lines` parameter, and add the new public function:

```rust
/// Search across multiple segments using a `MatchPattern` with options.
///
/// Combines pattern-aware matching (regex, case-insensitive) with
/// search options (context lines, max results).
pub fn search_segments_with_pattern_and_options(
    snapshot: &SegmentList,
    pattern: &MatchPattern,
    options: &SearchOptions,
) -> Result<SearchResult, IndexError> {
    // Similar to search_segments_with_pattern but uses ContentVerifier with
    // context_lines from options, and respects max_results.
    // ...
}
```

The internal `search_single_segment_with_pattern` needs to accept `context_lines: u32` and pass it to `ContentVerifier::new(pattern.clone(), context_lines)`.

Add the new function to `indexrs-core/src/lib.rs` re-exports:

```rust
pub use multi_search::{
    search_segments, search_segments_with_options, search_segments_with_pattern,
    search_segments_with_pattern_and_options,
};
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p indexrs-core -- test_search_segments_with_pattern_and_options`
Expected: PASS.

**Step 5: Run full core test suite**

Run: `cargo test -p indexrs-core`
Expected: all tests PASS (no regressions).

**Step 6: Commit**

```bash
git add indexrs-core/src/multi_search.rs indexrs-core/src/lib.rs
git commit -m "feat(core): add search_segments_with_pattern_and_options for CLI search (HHC-53)"
```

---

### Task 7: `indexrs search` Command (HHC-53)

**Files:**
- Create: `indexrs-cli/src/search_cmd.rs`
- Modify: `indexrs-cli/src/main.rs` (add `mod search_cmd;`, wire up)

**Step 1: Write tests for search command**

Create `indexrs-cli/src/search_cmd.rs` with tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use indexrs_core::segment::InputFile;
    use indexrs_core::SegmentManager;

    fn build_test_index(dir: &Path) -> SegmentManager {
        let indexrs_dir = dir.join(".indexrs");
        std::fs::create_dir_all(indexrs_dir.join("segments")).unwrap();
        let manager = SegmentManager::new(&indexrs_dir).unwrap();
        manager
            .index_files(vec![
                InputFile {
                    path: "src/main.rs".to_string(),
                    content: b"fn main() {\n    println!(\"hello world\");\n}\n".to_vec(),
                    mtime: 100,
                },
                InputFile {
                    path: "src/lib.rs".to_string(),
                    content: b"pub fn greeting() -> &'static str {\n    \"hello\"\n}\n".to_vec(),
                    mtime: 200,
                },
            ])
            .unwrap();
        manager
    }

    #[test]
    fn test_resolve_match_pattern_literal() {
        let pattern = resolve_match_pattern("hello", false, false, true, false);
        assert!(matches!(pattern, MatchPattern::LiteralCaseInsensitive(_)));
    }

    #[test]
    fn test_resolve_match_pattern_case_sensitive() {
        let pattern = resolve_match_pattern("hello", false, true, false, false);
        assert!(matches!(pattern, MatchPattern::Literal(_)));
    }

    #[test]
    fn test_resolve_match_pattern_regex() {
        let pattern = resolve_match_pattern("fn\\s+", true, false, false, false);
        assert!(matches!(pattern, MatchPattern::Regex(_)));
    }

    #[test]
    fn test_resolve_match_pattern_smart_case_lower() {
        // All lowercase -> case insensitive
        let pattern = resolve_match_pattern("hello", false, false, false, true);
        assert!(matches!(pattern, MatchPattern::LiteralCaseInsensitive(_)));
    }

    #[test]
    fn test_resolve_match_pattern_smart_case_upper() {
        // Has uppercase -> case sensitive
        let pattern = resolve_match_pattern("Hello", false, false, false, true);
        assert!(matches!(pattern, MatchPattern::Literal(_)));
    }

    #[test]
    fn test_search_vimgrep_format() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let mut buf = Vec::new();
        let color = ColorConfig::new(false);
        let mut writer = StreamingWriter::new(&mut buf);

        let opts = SearchCmdOptions {
            query: "println".to_string(),
            pattern: MatchPattern::LiteralCaseInsensitive("println".to_string()),
            context_lines: 0,
            limit: 1000,
            language: None,
            path_glob: None,
            stats: false,
        };

        let exit = run_search(&snapshot, &opts, &color, &mut writer).unwrap();
        let output = String::from_utf8(buf).unwrap();

        // Should be vimgrep format: file:line:col:content
        assert!(output.contains("src/main.rs:2:"));
        assert!(output.contains("println"));
        assert!(matches!(exit, ExitCode::Success));
    }

    #[test]
    fn test_search_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let manager = build_test_index(dir.path());
        let snapshot = manager.snapshot();

        let mut buf = Vec::new();
        let color = ColorConfig::new(false);
        let mut writer = StreamingWriter::new(&mut buf);

        let opts = SearchCmdOptions {
            query: "nonexistent_string_xyz".to_string(),
            pattern: MatchPattern::LiteralCaseInsensitive("nonexistent_string_xyz".to_string()),
            context_lines: 0,
            limit: 1000,
            language: None,
            path_glob: None,
            stats: false,
        };

        let exit = run_search(&snapshot, &opts, &color, &mut writer).unwrap();
        assert!(matches!(exit, ExitCode::NoResults));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p indexrs-cli -- search_cmd`
Expected: compilation error.

**Step 3: Implement search command**

```rust
use std::path::Path;

use globset::{Glob, GlobMatcher};
use indexrs_core::error::IndexError;
use indexrs_core::index_state::SegmentList;
use indexrs_core::search::{MatchPattern, SearchOptions};
use indexrs_core::multi_search::search_segments_with_pattern_and_options;

use crate::color::ColorConfig;
use crate::output::{ExitCode, StreamingWriter};

pub struct SearchCmdOptions {
    pub query: String,
    pub pattern: MatchPattern,
    pub context_lines: usize,
    pub limit: usize,
    pub language: Option<String>,
    pub path_glob: Option<String>,
    pub stats: bool,
}

/// Resolve CLI flags into a MatchPattern.
///
/// Priority: --regex > --case-sensitive > --ignore-case > --smart-case (default).
/// Smart case: case-sensitive if query contains uppercase, else case-insensitive.
pub fn resolve_match_pattern(
    query: &str,
    regex: bool,
    case_sensitive: bool,
    ignore_case: bool,
    smart_case: bool,
) -> MatchPattern {
    if regex {
        MatchPattern::Regex(query.to_string())
    } else if case_sensitive {
        MatchPattern::Literal(query.to_string())
    } else if ignore_case {
        MatchPattern::LiteralCaseInsensitive(query.to_string())
    } else if smart_case || (!case_sensitive && !ignore_case) {
        // Smart case (default): uppercase present = case-sensitive
        if query.chars().any(|c| c.is_uppercase()) {
            MatchPattern::Literal(query.to_string())
        } else {
            MatchPattern::LiteralCaseInsensitive(query.to_string())
        }
    } else {
        MatchPattern::LiteralCaseInsensitive(query.to_string())
    }
}

/// Run the search command: search segments, format as vimgrep, stream to output.
pub fn run_search<W: std::io::Write>(
    snapshot: &SegmentList,
    opts: &SearchCmdOptions,
    color: &ColorConfig,
    writer: &mut StreamingWriter<W>,
) -> Result<ExitCode, IndexError> {
    let search_opts = SearchOptions {
        context_lines: opts.context_lines,
        max_results: Some(opts.limit),
    };

    let result = search_segments_with_pattern_and_options(snapshot, &opts.pattern, &search_opts)?;

    if result.files.is_empty() {
        return Ok(ExitCode::NoResults);
    }

    let glob_matcher: Option<GlobMatcher> = opts
        .path_glob
        .as_ref()
        .map(|g| Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| IndexError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;

    for file_match in &result.files {
        let path_str = file_match.path.to_string_lossy();

        // Path filter
        if let Some(ref matcher) = glob_matcher {
            if !matcher.is_match(path_str.as_ref()) {
                continue;
            }
        }

        // Language filter
        if let Some(ref lang) = opts.language {
            if !file_match.language.to_string().eq_ignore_ascii_case(lang) {
                continue;
            }
        }

        for line_match in &file_match.lines {
            let col = line_match
                .ranges
                .first()
                .map(|(start, _)| start + 1)
                .unwrap_or(1);

            let line = color.format_search_line(
                &path_str,
                line_match.line_number,
                col,
                &line_match.content,
                &line_match.ranges,
            );

            if writer.write_line(&line).is_err() {
                break; // Broken pipe
            }
        }
    }
    let _ = writer.finish();

    if opts.stats {
        eprintln!(
            "{} matches in {} files ({:.1?})",
            result.total_match_count, result.total_file_count, result.duration
        );
    }

    Ok(ExitCode::Success)
}
```

Wire up in `main.rs`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p indexrs-cli -- search_cmd`
Expected: all tests PASS.

**Step 5: Commit**

```bash
git add indexrs-cli/src/search_cmd.rs indexrs-cli/src/main.rs
git commit -m "feat(cli): implement 'indexrs search' with vimgrep output and ANSI colors (HHC-53)"
```

---

### Task 8: `indexrs preview` Command (HHC-54)

**Files:**
- Modify: `indexrs-cli/Cargo.toml` (add `syntect`)
- Create: `indexrs-cli/src/preview.rs`
- Modify: `indexrs-cli/src/main.rs` (add `mod preview;`, wire up)

**Step 1: Write tests for preview command**

Create `indexrs-cli/src/preview.rs` with tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_bat_available_detection() {
        // Just verify the function doesn't panic.
        let _ = is_bat_available();
    }

    #[test]
    fn test_render_preview_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        fs::write(&file, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

        let mut buf = Vec::new();
        let opts = PreviewOptions {
            file: file.clone(),
            line: Some(2),
            context: Some(5),
            highlight_line: Some(2),
            color_enabled: false,
        };

        render_builtin_preview(&opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        // Should contain line numbers and content
        assert!(output.contains("println"));
        assert!(output.contains("1"));
        assert!(output.contains("2"));
    }

    #[test]
    fn test_render_preview_centers_on_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        let content: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        fs::write(&file, &content).unwrap();

        let mut buf = Vec::new();
        let opts = PreviewOptions {
            file: file.clone(),
            line: Some(25),
            context: Some(3),
            highlight_line: None,
            color_enabled: false,
        };

        render_builtin_preview(&opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        // Should show lines around line 25
        assert!(output.contains("line 25"));
        assert!(output.contains("line 22"));
        assert!(output.contains("line 28"));
    }

    #[test]
    fn test_render_preview_file_not_found() {
        let mut buf = Vec::new();
        let opts = PreviewOptions {
            file: PathBuf::from("/nonexistent/file.rs"),
            line: None,
            context: None,
            highlight_line: None,
            color_enabled: false,
        };

        let result = render_builtin_preview(&opts, &mut buf);
        assert!(result.is_err());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p indexrs-cli -- preview`
Expected: compilation error.

**Step 3: Implement preview command**

Add to `indexrs-cli/Cargo.toml`:

```toml
syntect = { version = "5", default-features = false, features = ["default-syntaxes", "default-themes", "regex-onig"] }
```

Implement `indexrs-cli/src/preview.rs`:

```rust
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use indexrs_core::error::IndexError;

pub struct PreviewOptions {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub context: Option<usize>,
    pub highlight_line: Option<usize>,
    pub color_enabled: bool,
}

/// Check if `bat` is available in $PATH.
pub fn is_bat_available() -> bool {
    Command::new("bat")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run preview using `bat` (preferred path).
pub fn run_bat_preview(opts: &PreviewOptions) -> Result<(), IndexError> {
    let mut cmd = Command::new("bat");
    cmd.arg("--style=numbers,header")
        .arg("--color=always");

    if let Some(hl) = opts.highlight_line.or(opts.line) {
        cmd.arg(format!("--highlight-line={hl}"));
    }

    if let Some(line) = opts.line {
        let ctx = opts.context.unwrap_or(20);
        let start = line.saturating_sub(ctx);
        let end = line + ctx;
        cmd.arg(format!("--line-range={start}:{end}"));
    }

    cmd.arg("--").arg(&opts.file);

    let status = cmd.status().map_err(IndexError::Io)?;
    if !status.success() {
        return Err(IndexError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("bat exited with status {status}"),
        )));
    }
    Ok(())
}

/// Render a preview using the built-in renderer (line numbers, optional highlight).
///
/// This is the fallback when bat is not available. Reads the file directly
/// (not from the index) and shows lines centered on `opts.line` with
/// `opts.context` lines of surrounding context. Uses syntect for syntax
/// highlighting when color is enabled.
pub fn render_builtin_preview<W: Write>(
    opts: &PreviewOptions,
    out: &mut W,
) -> Result<(), IndexError> {
    let content = std::fs::read_to_string(&opts.file).map_err(IndexError::Io)?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    // Determine the range of lines to show.
    let preview_lines = opts
        .context
        .or_else(|| std::env::var("FZF_PREVIEW_LINES").ok().and_then(|v| v.parse().ok()))
        .unwrap_or(total_lines);

    let (start, end) = if let Some(center) = opts.line {
        let center = center.saturating_sub(1); // Convert to 0-indexed
        let half = preview_lines / 2;
        let start = center.saturating_sub(half);
        let end = (start + preview_lines).min(total_lines);
        (start, end)
    } else {
        (0, preview_lines.min(total_lines))
    };

    let line_num_width = format!("{}", end).len();

    for i in start..end {
        let line_num = i + 1; // 1-indexed display
        let is_highlighted = opts.highlight_line.is_some_and(|hl| hl == line_num);

        if is_highlighted && opts.color_enabled {
            // Reverse video for highlighted line
            write!(out, "\x1b[7m{line_num:>line_num_width$} {}\x1b[0m\n", lines[i])
                .map_err(IndexError::Io)?;
        } else {
            writeln!(out, "{line_num:>line_num_width$} {}", lines[i])
                .map_err(IndexError::Io)?;
        }
    }

    Ok(())
}

/// Run the preview command: delegate to bat or use built-in renderer.
pub fn run_preview(opts: &PreviewOptions) -> Result<(), IndexError> {
    if is_bat_available() {
        run_bat_preview(opts)
    } else {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        render_builtin_preview(opts, &mut out)
    }
}
```

Wire up in `main.rs`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p indexrs-cli -- preview`
Expected: all tests PASS.

**Step 5: Commit**

```bash
git add indexrs-cli/src/preview.rs indexrs-cli/src/main.rs indexrs-cli/Cargo.toml
git commit -m "feat(cli): implement 'indexrs preview' with bat delegation and built-in fallback (HHC-54)"
```

---

### Task 9: Wire Up main.rs and Integration Test (HHC-52, HHC-53, HHC-54, HHC-55)

**Files:**
- Modify: `indexrs-cli/src/main.rs` (complete wiring)

**Step 1: Implement the full main.rs dispatch**

Replace the stub `match` block with real dispatch. The main function should:

1. Call `output::setup_sigpipe()` first
2. Parse CLI args
3. Resolve color mode using `std::io::IsTerminal`
4. For `files`, `search`, `preview`: find repo root, load index, dispatch to module
5. Map `ExitCode` to `std::process::exit()`

```rust
mod args;
mod color;
mod files;
mod output;
mod preview;
mod repo;
mod search_cmd;

use std::io::IsTerminal;

use args::{Cli, ColorMode, Command};
use clap::Parser;
use color::ColorConfig;
use output::{ExitCode, StreamingWriter};

#[tokio::main]
async fn main() {
    output::setup_sigpipe();

    let cli = Cli::parse();

    let color_enabled = match cli.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => std::io::stdout().is_terminal(),
    };
    let color = ColorConfig::new(color_enabled);

    let exit_code = match run(cli, &color) {
        Ok(code) => code as i32,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::Error as i32
        }
    };

    std::process::exit(exit_code);
}

fn run(cli: Cli, color: &ColorConfig) -> Result<ExitCode, indexrs_core::IndexError> {
    match cli.command {
        Command::Search {
            query,
            regex,
            case_sensitive,
            ignore_case,
            smart_case,
            language,
            path,
            limit,
            context,
            stats,
        } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            let manager = repo::load_index(&repo_root)?;
            let snapshot = manager.snapshot();

            let pattern = search_cmd::resolve_match_pattern(
                &query,
                regex,
                case_sensitive,
                ignore_case,
                smart_case,
            );
            let opts = search_cmd::SearchCmdOptions {
                query,
                pattern,
                context_lines: context.unwrap_or(0),
                limit,
                language,
                path_glob: path,
                stats,
            };

            let stdout = std::io::stdout();
            let mut writer = StreamingWriter::new(stdout.lock());
            search_cmd::run_search(&snapshot, &opts, color, &mut writer)
        }
        Command::Files {
            query: _,
            language,
            path,
            limit,
            sort,
        } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            let manager = repo::load_index(&repo_root)?;
            let snapshot = manager.snapshot();

            let filter = files::FilesFilter {
                language,
                path_glob: path,
                sort,
                limit,
            };

            let stdout = std::io::stdout();
            let mut writer = StreamingWriter::new(stdout.lock());
            files::run_files(&snapshot, &filter, color, &mut writer)
        }
        Command::Preview {
            file,
            line,
            context,
            highlight_line,
        } => {
            let opts = preview::PreviewOptions {
                file,
                line,
                context,
                highlight_line,
                color_enabled: color.enabled,
            };
            preview::run_preview(&opts)?;
            Ok(ExitCode::Success)
        }
        Command::Symbols { .. } => {
            eprintln!("symbols: not yet implemented (post-v0.2)");
            Ok(ExitCode::Error)
        }
        Command::Status => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            let manager = repo::load_index(&repo_root)?;
            let snapshot = manager.snapshot();
            let file_count: usize = snapshot.iter().map(|s| s.entry_count() as usize).sum();
            println!("Segments: {}", snapshot.len());
            println!("Files: {file_count}");
            Ok(ExitCode::Success)
        }
        Command::Reindex { full: _ } => {
            eprintln!("reindex: not yet implemented");
            Ok(ExitCode::Error)
        }
    }
}
```

**Step 2: Verify compilation and tests**

Run: `cargo check -p indexrs-cli && cargo test -p indexrs-cli`
Expected: compiles and all tests PASS.

**Step 3: Run clippy and fmt**

Run: `cargo clippy -p indexrs-cli -- -D warnings && cargo fmt --all -- --check`
Expected: no issues.

**Step 4: Commit**

```bash
git add indexrs-cli/src/main.rs
git commit -m "feat(cli): wire up files, search, and preview commands in main (HHC-52, HHC-53, HHC-54)"
```

---

### Task 10: On-Demand Daemon via Unix Domain Socket (HHC-57)

**Files:**
- Modify: `indexrs-cli/Cargo.toml` (add `serde`, `serde_json`)
- Create: `indexrs-cli/src/daemon.rs`
- Modify: `indexrs-cli/src/main.rs` (add `mod daemon;`, add `--daemon` flag or auto-detect)

**Step 1: Write tests for daemon protocol**

Create `indexrs-cli/src/daemon.rs` with tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialize_search() {
        let req = DaemonRequest::Search {
            query: "hello".to_string(),
            regex: false,
            case_sensitive: false,
            ignore_case: true,
            limit: 1000,
            context_lines: 0,
            language: None,
            path_glob: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("hello"));
    }

    #[test]
    fn test_request_serialize_files() {
        let req = DaemonRequest::Files {
            language: Some("rust".to_string()),
            path_glob: None,
            sort: "path".to_string(),
            limit: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("rust"));
    }

    #[test]
    fn test_response_roundtrip() {
        let resp = DaemonResponse::Line("src/main.rs:10:5:hello".to_string());
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, DaemonResponse::Line(_)));
    }

    #[test]
    fn test_socket_path() {
        let root = PathBuf::from("/tmp/test-repo");
        let path = socket_path(&root);
        assert_eq!(path, PathBuf::from("/tmp/test-repo/.indexrs/sock"));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p indexrs-cli -- daemon`
Expected: compilation error.

**Step 3: Implement daemon module**

Add to `indexrs-cli/Cargo.toml`:

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

Implement `indexrs-cli/src/daemon.rs`:

```rust
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

use indexrs_core::error::IndexError;
use indexrs_core::SegmentManager;

/// Idle timeout before daemon self-terminates.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

/// Request from CLI client to daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRequest {
    Search {
        query: String,
        regex: bool,
        case_sensitive: bool,
        ignore_case: bool,
        limit: usize,
        context_lines: usize,
        language: Option<String>,
        path_glob: Option<String>,
    },
    Files {
        language: Option<String>,
        path_glob: Option<String>,
        sort: String,
        limit: Option<usize>,
    },
    Ping,
    Shutdown,
}

/// Response from daemon to CLI client, one JSON line per message.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonResponse {
    /// A single output line (file path or search match).
    Line(String),
    /// End of results with summary.
    Done { total: usize, duration_ms: u64 },
    /// Error message.
    Error { message: String },
    /// Ping response.
    Pong,
}

/// Return the Unix socket path for a given repo root.
pub fn socket_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".indexrs").join("sock")
}

/// Try to connect to a running daemon. Returns None if no daemon is running.
pub async fn try_connect(repo_root: &Path) -> Option<UnixStream> {
    let path = socket_path(repo_root);
    UnixStream::connect(&path).await.ok()
}

/// Start a daemon process in the background.
///
/// The daemon loads the index, listens on the Unix socket, and serves
/// requests until idle timeout.
pub async fn start_daemon(repo_root: &Path) -> Result<(), IndexError> {
    let sock_path = socket_path(repo_root);

    // Remove stale socket file
    let _ = std::fs::remove_file(&sock_path);

    let listener = UnixListener::bind(&sock_path).map_err(IndexError::Io)?;

    let indexrs_dir = repo_root.join(".indexrs");
    let manager = std::sync::Arc::new(SegmentManager::new(&indexrs_dir)?);

    loop {
        match timeout(IDLE_TIMEOUT, listener.accept()).await {
            Ok(Ok((stream, _))) => {
                let mgr = manager.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &mgr).await {
                        eprintln!("daemon: connection error: {e}");
                    }
                });
            }
            Ok(Err(e)) => {
                eprintln!("daemon: accept error: {e}");
            }
            Err(_) => {
                // Idle timeout — shut down
                let _ = std::fs::remove_file(&sock_path);
                return Ok(());
            }
        }
    }
}

/// Handle a single client connection.
async fn handle_connection(
    stream: UnixStream,
    manager: &SegmentManager,
) -> Result<(), IndexError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await.map_err(IndexError::Io)? > 0 {
        let request: DaemonRequest = match serde_json::from_str(line.trim()) {
            Ok(req) => req,
            Err(e) => {
                let resp = DaemonResponse::Error {
                    message: format!("invalid request: {e}"),
                };
                let json = serde_json::to_string(&resp).unwrap();
                writer
                    .write_all(format!("{json}\n").as_bytes())
                    .await
                    .map_err(IndexError::Io)?;
                line.clear();
                continue;
            }
        };

        match request {
            DaemonRequest::Ping => {
                let resp = serde_json::to_string(&DaemonResponse::Pong).unwrap();
                writer
                    .write_all(format!("{resp}\n").as_bytes())
                    .await
                    .map_err(IndexError::Io)?;
            }
            DaemonRequest::Shutdown => {
                return Ok(());
            }
            DaemonRequest::Search { .. } | DaemonRequest::Files { .. } => {
                // Execute the command using the pre-loaded index.
                // Serialize results as Line responses, then Done.
                // Implementation delegates to run_search/run_files with
                // a Vec<u8> writer, then sends each line as a DaemonResponse::Line.
                let snapshot = manager.snapshot();
                // ... (detailed implementation in the actual code)
                let resp = serde_json::to_string(&DaemonResponse::Done {
                    total: 0,
                    duration_ms: 0,
                })
                .unwrap();
                writer
                    .write_all(format!("{resp}\n").as_bytes())
                    .await
                    .map_err(IndexError::Io)?;
            }
        }

        line.clear();
    }

    Ok(())
}
```

Add `mod daemon;` to `main.rs`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p indexrs-cli -- daemon`
Expected: all tests PASS.

**Step 5: Implement daemon auto-start in main.rs**

Add logic to `run()`: before executing a command, check if a daemon is running via `try_connect()`. If so, send the request over the socket. If not, fall back to direct execution (current behavior). The daemon can be explicitly started with `indexrs daemon start`.

This is a larger change — connect the protocol to the actual command implementations.

**Step 6: Commit**

```bash
git add indexrs-cli/src/daemon.rs indexrs-cli/src/main.rs indexrs-cli/Cargo.toml
git commit -m "feat(cli): add on-demand daemon with Unix domain socket (HHC-57)"
```

---

### Task 11: Shell Functions and Editor Integration Recipes (HHC-56)

**Files:**
- Create: `docs/fzf-recipes.md`

**Step 1: Create the fzf recipes documentation**

This is a documentation-only task. Create `docs/fzf-recipes.md` with the shell functions from `docs/design/fzf-interface.md` sections 4–5, formatted as copy-paste-ready recipes:

- `ixf` — Interactive file finder
- `ixg` — Interactive grep/content search
- `ixs` — Interactive symbol search (placeholder until symbols are implemented)
- `ix` — Combined mode switcher
- Zsh/bash keybinding integration
- Tmux popup support
- Vim/Neovim fzf.vim commands
- VS Code terminal patterns

The content is already written in the design doc — this task extracts it into a standalone user-facing document.

**Step 2: Commit**

```bash
git add docs/fzf-recipes.md
git commit -m "docs: add fzf recipes with shell functions and editor integration (HHC-56)"
```

---

## Summary

| Task | Issue(s) | What it builds |
|------|----------|----------------|
| 1 | HHC-55 | Color module with ANSI formatting |
| 2 | HHC-55 | Streaming writer, SIGPIPE, exit codes |
| 3 | HHC-52/53/54 | Updated CLI argument definitions |
| 4 | HHC-55 | Repo/index discovery |
| 5 | HHC-52 | `indexrs files` with filtering and sorting |
| 6 | HHC-53 | Core: `search_segments_with_pattern_and_options` |
| 7 | HHC-53 | `indexrs search` with vimgrep output |
| 8 | HHC-54 | `indexrs preview` with bat delegation |
| 9 | HHC-52/53/54/55 | Main.rs wiring and integration |
| 10 | HHC-57 | On-demand daemon via Unix socket |
| 11 | HHC-56 | Shell functions and editor recipes docs |

**Dependencies:** Tasks 1–4 are independent and can be parallelized. Tasks 5 and 7 depend on 1–4. Task 6 is independent. Task 8 depends on 3. Task 9 depends on 5, 7, 8. Task 10 depends on 9. Task 11 is independent.
