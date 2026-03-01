use std::path::Path;
use std::time::Instant;

use indexrs_core::checkpoint::{read_checkpoint, write_checkpoint, Checkpoint};
use indexrs_core::error::IndexError;
use indexrs_core::git_diff::GitChangeDetector;
use indexrs_core::segment::InputFile;
use indexrs_core::walker::DirectoryWalkerBuilder;
use indexrs_core::{should_index_file, SegmentManager, DEFAULT_MAX_FILE_SIZE};

/// Run the `indexrs init` command.
///
/// Walks the repo tree, builds the full index, and writes a checkpoint.
/// If `force` is false and an index already exists, returns an error.
pub fn run_init(repo_root: &Path, force: bool) -> Result<(), IndexError> {
    let indexrs_dir = repo_root.join(".indexrs");

    // Check for existing index unless --force.
    if !force
        && let Ok(Some(_)) = read_checkpoint(&indexrs_dir)
    {
        return Err(IndexError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "index already exists. Use --force to rebuild.",
        )));
    }

    // If forcing, remove existing segments.
    if force {
        let segments_dir = indexrs_dir.join("segments");
        if segments_dir.exists() {
            eprintln!("Removing existing index...");
            std::fs::remove_dir_all(&segments_dir)?;
        }
    }

    let start = Instant::now();
    eprintln!("Walking file tree...");

    // Walk the tree.
    let walker = DirectoryWalkerBuilder::new(repo_root).build();
    let walked = walker.run()?;

    // Collect indexable files.
    let mut files = Vec::new();
    for wf in &walked {
        let content = match std::fs::read(&wf.path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !should_index_file(&wf.path, &content, DEFAULT_MAX_FILE_SIZE) {
            continue;
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
        files.push(InputFile {
            path: rel_path,
            content,
            mtime,
        });
    }

    let file_count = files.len() as u64;
    eprintln!("Indexing {file_count} files...");

    // Build the index.
    let manager = SegmentManager::new(&indexrs_dir)?;
    manager.index_files(files)?;

    // Write checkpoint.
    let git = GitChangeDetector::new(repo_root.to_path_buf());
    let git_commit = git.get_head_sha().ok();
    let checkpoint = Checkpoint::new(git_commit, file_count);
    write_checkpoint(&indexrs_dir, &checkpoint)?;

    let elapsed = start.elapsed();
    eprintln!(
        "Done. Indexed {} files in {:.1}s.",
        file_count,
        elapsed.as_secs_f64()
    );
    Ok(())
}
