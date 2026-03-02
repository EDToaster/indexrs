//! Build or update an index for a directory, then print stats.
//!
//! If `.indexrs/` already exists under the target directory, detects changes
//! via `git diff` and applies an incremental update. Otherwise, performs a
//! full index build. Prints segment and file stats on completion.
//!
//! Usage:
//!   cargo run -p indexrs-core --example build_index -- <directory>
//!   cargo run -p indexrs-core --example build_index --release -- <directory>
//!
//! Examples:
//!   cargo run -p indexrs-core --example build_index --release -- .
//!   cargo run -p indexrs-core --example build_index --release -- ~/src/my-repo

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use indexrs_core::{
    DEFAULT_MAX_FILE_SIZE, DirectoryWalkerBuilder, GitChangeDetector, InputFile, Language,
    SegmentManager, is_binary_content, is_binary_path, search_segments,
};

fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn walk_and_collect(dir: &PathBuf) -> Result<Vec<InputFile>, Box<dyn std::error::Error>> {
    let walked = DirectoryWalkerBuilder::new(dir).build().run()?;
    let total = walked.len();
    let mut files = Vec::new();

    for (idx, w) in walked.iter().enumerate() {
        if idx % 100 == 0 || idx + 1 == total {
            let pct = (idx + 1) * 100 / total;
            eprint!(
                "\x1b[2K\r  Filtering files... {pct}% ({}/{})",
                idx + 1,
                total
            );
            let _ = std::io::stderr().flush();
        }
        if is_binary_path(&w.path) {
            continue;
        }
        if w.metadata.len() > DEFAULT_MAX_FILE_SIZE {
            continue;
        }
        let content = match std::fs::read(&w.path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if is_binary_content(&content) {
            continue;
        }
        let rel_path = w.path.strip_prefix(dir).unwrap_or(&w.path);
        let mtime = w
            .metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        files.push(InputFile {
            path: rel_path.to_string_lossy().to_string(),
            content,
            mtime,
        });
    }
    eprintln!();

    Ok(files)
}

fn full_build(dir: &PathBuf, manager: &SegmentManager) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("  Walking directory...");
    let files = walk_and_collect(dir)?;
    let file_count = files.len();
    let total_bytes: u64 = files.iter().map(|f| f.content.len() as u64).sum();
    eprintln!(
        "  Found {} indexable files ({})",
        file_count,
        human_bytes(total_bytes)
    );

    let t_build = Instant::now();
    manager.index_files_with_progress(files, |done, total| {
        if done % 100 == 0 || done == total {
            let pct = done * 100 / total;
            eprint!("\x1b[2K\r  Building segments... {pct}% ({done}/{total})");
            let _ = std::io::stderr().flush();
        }
    })?;
    eprintln!();

    let snap = manager.snapshot();
    eprintln!(
        "  Built {} segment(s) in {:.1?}",
        snap.len(),
        t_build.elapsed()
    );

    Ok(())
}

fn incremental_update(
    dir: &PathBuf,
    manager: &SegmentManager,
) -> Result<usize, Box<dyn std::error::Error>> {
    let abs_dir = std::fs::canonicalize(dir)?;
    let detector = GitChangeDetector::new(abs_dir.clone());

    let changes = detector.detect_changes()?;
    let change_count = changes.len();

    if changes.is_empty() {
        return Ok(0);
    }

    eprintln!("  Applying {} change(s)...", change_count);
    for c in &changes {
        eprintln!("    {:?}: {}", c.kind, c.path.display());
    }

    manager.apply_changes(&abs_dir, &changes)?;

    Ok(change_count)
}

fn print_stats(
    manager: &SegmentManager,
    index_dir: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let snap = manager.snapshot();

    // Segment summary
    let seg_count = snap.len();
    let mut total_files: u64 = 0;
    let mut total_tombstoned: u64 = 0;
    let mut total_live: u64 = 0;

    eprintln!();
    eprintln!("=== Index Stats ===");
    eprintln!("  Segments: {seg_count}");

    for seg in snap.iter() {
        let entry_count = seg.entry_count() as u64;
        let tombstones = seg.load_tombstones().unwrap_or_default();
        let tombstone_count = tombstones.len() as u64;
        let live = entry_count - tombstone_count;

        total_files += entry_count;
        total_tombstoned += tombstone_count;
        total_live += live;

        eprintln!(
            "    seg_{:04}: {} entries, {} tombstoned, {} live",
            seg.segment_id().0,
            entry_count,
            tombstone_count,
            live
        );
    }

    eprintln!();
    eprintln!("  Total entries:    {total_files}");
    eprintln!("  Total tombstoned: {total_tombstoned}");
    eprintln!("  Total live:       {total_live}");

    if manager.should_compact() {
        eprintln!("  Compaction:       RECOMMENDED (>10 segments or >30% tombstones)");
    } else {
        eprintln!("  Compaction:       not needed");
    }

    // Language breakdown
    let mut lang_counts: std::collections::HashMap<Language, u64> =
        std::collections::HashMap::new();
    let mut total_content_bytes: u64 = 0;
    let mut total_lines: u64 = 0;

    for seg in snap.iter() {
        let tombstones = seg.load_tombstones().unwrap_or_default();
        let reader = seg.metadata_reader();
        for entry in reader.iter_all() {
            let entry = entry?;
            if tombstones.contains(entry.file_id) {
                continue;
            }
            *lang_counts.entry(entry.language).or_default() += 1;
            total_content_bytes += entry.size_bytes as u64;
            total_lines += entry.line_count as u64;
        }
    }

    let mut lang_vec: Vec<_> = lang_counts.into_iter().collect();
    lang_vec.sort_by(|a, b| b.1.cmp(&a.1));

    eprintln!();
    eprintln!(
        "  Content:          {} ({} lines)",
        human_bytes(total_content_bytes),
        total_lines
    );
    eprintln!("  Languages:");
    for (lang, count) in &lang_vec {
        eprintln!("    {lang}: {count} files");
    }

    // Disk usage
    let segments_dir = index_dir.join("segments");
    let disk_bytes = dir_size(&segments_dir);
    eprintln!();
    eprintln!("  Disk usage:       {}", human_bytes(disk_bytes));
    if total_content_bytes > 0 {
        eprintln!(
            "  Index / raw:      {:.2}x",
            disk_bytes as f64 / total_content_bytes as f64
        );
    }

    // Quick search sanity check
    let result = search_segments(&snap, "fn ")?;
    eprintln!();
    eprintln!(
        "  Search sanity:    \"fn \" -> {} matches in {} files",
        result.total_match_count, result.total_file_count
    );

    Ok(())
}

fn dir_size(path: &PathBuf) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(meta) = p.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <directory>", args[0]);
        eprintln!();
        eprintln!("Builds or updates an indexrs index, then prints stats.");
        eprintln!("Index is stored in <directory>/.indexrs/");
        std::process::exit(1);
    }
    let dir = PathBuf::from(&args[1]);
    let index_dir = dir.join(".indexrs");

    let t0 = Instant::now();

    let existing = index_dir.join("segments").exists();
    let manager = SegmentManager::new(&index_dir)?;
    let had_segments = !manager.snapshot().is_empty();

    if existing && had_segments {
        eprintln!("=== Incremental Update ===");
        let change_count = incremental_update(&dir, &manager)?;
        if change_count == 0 {
            eprintln!("  No changes detected — index is up to date.");
        } else {
            eprintln!("  Applied {change_count} change(s) in {:.1?}", t0.elapsed());
        }
    } else {
        eprintln!("=== Full Index Build ===");
        full_build(&dir, &manager)?;
        eprintln!("  Built in {:.1?}", t0.elapsed());
    }

    print_stats(&manager, &index_dir)?;

    eprintln!();
    eprintln!("Total time: {:.1?}", t0.elapsed());

    Ok(())
}
