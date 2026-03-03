use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::Instant;

use indexrs_core::checkpoint::{Checkpoint, read_checkpoint, write_checkpoint};
use indexrs_core::error::IndexError;
use indexrs_core::git_diff::GitChangeDetector;
use indexrs_core::segment::InputFile;
use indexrs_core::walker::DirectoryWalkerBuilder;
use indexrs_core::{DEFAULT_MAX_FILE_SIZE, SegmentManager, should_index_file};

/// Format a number with comma separators (e.g. 1234567 -> "1,234,567").
fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Format bytes in human-readable form (e.g. 1048576 -> "1.0 MB").
fn fmt_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// In-place progress line on stderr (uses `\r` when stderr is a terminal).
struct ProgressLine {
    pub is_tty: bool,
    last_len: usize,
}

impl ProgressLine {
    fn new() -> Self {
        Self {
            is_tty: std::io::stderr().is_terminal(),
            last_len: 0,
        }
    }

    /// Print a progress message, overwriting the previous line if on a TTY.
    fn update(&mut self, msg: &str) {
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        if self.is_tty {
            // Pad with spaces to clear any leftover characters from previous line.
            let padding = if msg.len() < self.last_len {
                self.last_len - msg.len()
            } else {
                0
            };
            let _ = write!(handle, "\r{msg}{:padding$}", "");
            let _ = handle.flush();
            self.last_len = msg.len();
        } else {
            let _ = writeln!(handle, "{msg}");
        }
    }

    /// Finish the current line (prints a newline on TTY).
    fn finish(&mut self, msg: &str) {
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        if self.is_tty {
            let padding = if msg.len() < self.last_len {
                self.last_len - msg.len()
            } else {
                0
            };
            let _ = writeln!(handle, "\r{msg}{:padding$}", "");
            self.last_len = 0;
        } else {
            let _ = writeln!(handle, "{msg}");
        }
    }
}

/// Run the `indexrs init` command.
///
/// Walks the repo tree, builds the full index, and writes a checkpoint.
/// If `force` is false and an index already exists, returns an error.
pub fn run_init(repo_root: &Path, force: bool) -> Result<(), IndexError> {
    let indexrs_dir = repo_root.join(".indexrs");

    // Check for existing index unless --force.
    if !force {
        match read_checkpoint(&indexrs_dir) {
            Ok(Some(_)) => {
                return Err(IndexError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "index already exists. Use --force to rebuild.",
                )));
            }
            Err(e) => return Err(e),
            Ok(None) => {} // No checkpoint — proceed with init.
        }
    }

    // If forcing, remove existing segments and stale checkpoint.
    if force {
        let segments_dir = indexrs_dir.join("segments");
        if segments_dir.exists() {
            eprintln!("Removing existing index...");
            std::fs::remove_dir_all(&segments_dir)?;
        }
        let checkpoint_path = indexrs_dir.join("checkpoint.json");
        if checkpoint_path.exists() {
            std::fs::remove_file(&checkpoint_path)?;
        }
    }

    let start = Instant::now();
    let mut progress = ProgressLine::new();
    let step = if progress.is_tty { 100 } else { 5_000 };

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
            if current.is_multiple_of(step) || current == total_walked {
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
            if indexrs_core::is_binary_path(&wf.path) {
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

    let filter_elapsed = filter_start.elapsed();
    let total_skipped = skipped_size + skipped_binary + skipped_content + skipped_read_err;
    progress.finish(&format!(
        "Filtering files... {} indexable, {} skipped ({:.1}s)",
        fmt_count(files.len()),
        fmt_count(total_skipped),
        filter_elapsed.as_secs_f64()
    ));

    // Print skip breakdown if anything was skipped.
    if total_skipped > 0 {
        let mut reasons = Vec::new();
        if skipped_binary > 0 {
            reasons.push(format!("{} binary", fmt_count(skipped_binary)));
        }
        if skipped_size > 0 {
            reasons.push(format!("{} too large", fmt_count(skipped_size)));
        }
        if skipped_content > 0 {
            reasons.push(format!("{} filtered", fmt_count(skipped_content)));
        }
        if skipped_read_err > 0 {
            reasons.push(format!("{} read errors", fmt_count(skipped_read_err)));
        }
        eprintln!("  Skipped: {}", reasons.join(", "));
    }

    let file_count = files.len() as u64;

    if file_count == 0 {
        eprintln!("No indexable files found.");
        return Ok(());
    }

    // ── Phase 3: Build the index ─────────────────────────────────────
    let index_start = Instant::now();
    let total_files = files.len();
    progress.update(&format!(
        "Building index... 0/{} (0%) — {}",
        fmt_count(total_files),
        fmt_bytes(total_content_bytes),
    ));

    let manager = SegmentManager::new(&indexrs_dir)?;
    let progress = std::sync::Mutex::new(progress);
    manager.index_files_with_progress(files, |done, total| {
        if done % step == 0 || done == total {
            let pct = (done as f64 / total as f64 * 100.0) as u32;
            progress.lock().unwrap().update(&format!(
                "Building index... {}/{} ({pct}%)",
                fmt_count(done),
                fmt_count(total),
            ));
        }
    })?;
    let mut progress = progress.into_inner().unwrap();

    let index_elapsed = index_start.elapsed();
    progress.finish(&format!(
        "Building index... {}/{} (100%) ({:.1}s)",
        fmt_count(total_files),
        fmt_count(total_files),
        index_elapsed.as_secs_f64()
    ));

    // ── Phase 4: Write checkpoint ────────────────────────────────────
    eprintln!("Writing checkpoint...");
    let git = GitChangeDetector::new(repo_root.to_path_buf());
    let git_commit = git.get_head_sha().ok();
    let checkpoint = Checkpoint::new(git_commit, file_count);
    write_checkpoint(&indexrs_dir, &checkpoint)?;

    // ── Summary ──────────────────────────────────────────────────────
    let elapsed = start.elapsed();
    eprintln!(
        "Done. Indexed {} files ({}) in {:.1}s.",
        fmt_count(total_files),
        fmt_bytes(total_content_bytes),
        elapsed.as_secs_f64()
    );

    // ── Phase 5: Auto-register in repo registry ──────────────────────
    use indexrs_core::registry::{add_repo, config_file_path, load_config, save_config};

    match load_config() {
        Ok(mut config) => {
            if add_repo(&mut config, repo_root.to_path_buf(), None) {
                if let Err(e) = save_config(&config) {
                    eprintln!("Warning: could not save registry: {e}");
                } else {
                    let name = config.repo.last().unwrap().effective_name();
                    eprintln!(
                        "Registered repo \"{name}\" in {}",
                        config_file_path().display()
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("Warning: could not load registry: {e}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_count() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(1), "1");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1,000");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }

    #[test]
    fn test_fmt_bytes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1_048_576), "1.0 MB");
        assert_eq!(fmt_bytes(1_073_741_824), "1.0 GB");
    }
}
