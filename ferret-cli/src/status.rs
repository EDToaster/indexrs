use std::collections::HashMap;
use std::path::Path;

use ferret_indexer_core::{Language, SegmentManager, dir_size, search_segments};

use crate::output::human_bytes;

/// Run the status command, printing rich diagnostics to stdout.
pub fn run_status(repo_root: &Path) -> Result<(), ferret_indexer_core::IndexError> {
    let manager = crate::repo::load_index(repo_root)?;
    let snapshot = manager.snapshot();

    if snapshot.is_empty() {
        println!("No index found. Run 'ferret init' first.");
        return Ok(());
    }

    print_segment_summary(&manager, &snapshot)?;
    print_language_breakdown(&snapshot)?;
    print_disk_usage(repo_root, &snapshot)?;
    print_search_sanity(&snapshot)?;

    Ok(())
}

/// Print per-segment entry/tombstone counts and compaction recommendation.
fn print_segment_summary(
    manager: &SegmentManager,
    snapshot: &ferret_indexer_core::SegmentList,
) -> Result<(), ferret_indexer_core::IndexError> {
    println!("=== Segment Summary ===");

    let mut total_entries: u64 = 0;
    let mut total_tombstoned: u64 = 0;

    for seg in snapshot.iter() {
        let entries = seg.entry_count() as u64;
        let tombstones = seg.load_tombstones()?;
        let tombstoned = tombstones.len() as u64;
        let live = entries.saturating_sub(tombstoned);

        println!(
            "  seg_{:04}: {} entries, {} tombstoned, {} live",
            seg.segment_id(),
            entries,
            tombstoned,
            live
        );

        total_entries += entries;
        total_tombstoned += tombstoned;
    }

    let total_live = total_entries.saturating_sub(total_tombstoned);
    println!();
    println!(
        "Total: {} segments, {} entries ({} live, {} tombstoned)",
        snapshot.len(),
        total_entries,
        total_live,
        total_tombstoned
    );

    if manager.should_compact() {
        println!("Recommendation: run 'ferret reindex' to compact segments.");
    }

    println!();
    Ok(())
}

/// Print a breakdown of live files by language, with size and line totals.
fn print_language_breakdown(
    snapshot: &ferret_indexer_core::SegmentList,
) -> Result<(), ferret_indexer_core::IndexError> {
    println!("=== Language Breakdown ===");

    let mut lang_files: HashMap<Language, u64> = HashMap::new();
    let mut lang_bytes: HashMap<Language, u64> = HashMap::new();
    let mut lang_lines: HashMap<Language, u64> = HashMap::new();
    let mut total_bytes: u64 = 0;
    let mut total_lines: u64 = 0;

    for seg in snapshot.iter() {
        let tombstones = seg.load_tombstones()?;
        let reader = seg.metadata_reader();

        for entry_result in reader.iter_all() {
            let entry = entry_result?;
            if tombstones.contains(entry.file_id) {
                continue;
            }

            *lang_files.entry(entry.language).or_insert(0) += 1;
            *lang_bytes.entry(entry.language).or_insert(0) += entry.size_bytes as u64;
            *lang_lines.entry(entry.language).or_insert(0) += entry.line_count as u64;
            total_bytes += entry.size_bytes as u64;
            total_lines += entry.line_count as u64;
        }
    }

    println!(
        "Content total: {}, {} lines",
        human_bytes(total_bytes),
        total_lines
    );
    println!();

    // Sort by file count descending.
    let mut langs: Vec<_> = lang_files.into_iter().collect();
    langs.sort_by(|a, b| b.1.cmp(&a.1));

    for (lang, count) in &langs {
        let bytes = lang_bytes.get(lang).copied().unwrap_or(0);
        let lines = lang_lines.get(lang).copied().unwrap_or(0);
        println!(
            "  {:<16} {:>5} files  {:>10}  {:>8} lines",
            lang.to_string(),
            count,
            human_bytes(bytes),
            lines
        );
    }

    println!();
    Ok(())
}

/// Print on-disk size of the index directory and raw/index ratio.
fn print_disk_usage(
    repo_root: &Path,
    snapshot: &ferret_indexer_core::SegmentList,
) -> Result<(), ferret_indexer_core::IndexError> {
    println!("=== Disk Usage ===");

    let segments_dir = repo_root.join(".ferret_index").join("segments");
    let disk_bytes = dir_size(&segments_dir);

    // Compute total raw content size from metadata.
    let mut raw_bytes: u64 = 0;
    for seg in snapshot.iter() {
        let tombstones = seg.load_tombstones()?;
        let reader = seg.metadata_reader();
        for entry_result in reader.iter_all() {
            let entry = entry_result?;
            if !tombstones.contains(entry.file_id) {
                raw_bytes += entry.size_bytes as u64;
            }
        }
    }

    println!("  Index on disk: {}", human_bytes(disk_bytes));
    println!("  Raw content:   {}", human_bytes(raw_bytes));

    if raw_bytes > 0 {
        let ratio = disk_bytes as f64 / raw_bytes as f64;
        println!("  Index/raw ratio: {ratio:.2}x");
    }

    println!();
    Ok(())
}

/// Run a quick sanity search and print match/file counts.
fn print_search_sanity(
    snapshot: &ferret_indexer_core::SegmentList,
) -> Result<(), ferret_indexer_core::IndexError> {
    println!("=== Search Sanity ===");

    let result = search_segments(snapshot, "fn ")?;
    println!(
        "  Query 'fn ': {} matches in {} files",
        result.total_match_count, result.total_file_count
    );

    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_indexer_core::{InputFile, SegmentManager};
    use tempfile::tempdir;

    #[test]
    fn test_run_status_empty_index() {
        let dir = tempdir().unwrap();
        let index_dir = dir.path().join(".ferret_index");
        std::fs::create_dir_all(index_dir.join("segments")).unwrap();
        let result = run_status(dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_status_with_data() {
        let dir = tempdir().unwrap();
        let index_dir = dir.path().join(".ferret_index");
        let manager = SegmentManager::new(&index_dir).unwrap();

        let files = vec![
            InputFile {
                path: "src/main.rs".to_string(),
                content: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
                mtime: 1000,
            },
            InputFile {
                path: "lib.py".to_string(),
                content: b"def hello():\n    print('hello')\n".to_vec(),
                mtime: 2000,
            },
        ];
        manager.index_files(files).unwrap();

        let result = run_status(dir.path());
        assert!(result.is_ok());
    }
}
