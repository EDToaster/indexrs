use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use ferret_indexer_core::{
    DEFAULT_MAX_FILE_SIZE, DirectoryWalkerBuilder, Language, Trigram, encode_delta_varint,
    extract_trigrams_folded, extract_unique_trigrams_folded, is_binary_content, is_binary_path,
};

use crate::output::human_bytes;

/// Metadata entry size in meta.bin (fixed 58 bytes per file).
const META_ENTRY_SIZE: u64 = 58;

/// Tombstone header size per segment.
const TOMBSTONE_HEADER_SIZE: u64 = 14;

/// Trigram table entry size (19 bytes: 3B trigram + 4x u32 offsets/lengths).
const TRIGRAM_TABLE_ENTRY_SIZE: u64 = 19;

/// Trigram index header size.
const TRIGRAM_HEADER_SIZE: u64 = 10;

/// Metadata header size.
const META_HEADER_SIZE: u64 = 10;

/// Estimate index disk space and peak RAM for a directory without building.
///
/// Walks the directory, measures content compression and trigram posting sizes,
/// then reports estimated index size and peak RAM to stdout (progress to stderr).
pub fn run_estimate(
    directory: &Path,
    segment_budget_mb: u64,
) -> Result<(), ferret_indexer_core::IndexError> {
    let segment_budget_bytes = segment_budget_mb * 1024 * 1024;

    let t0 = Instant::now();

    // Phase 1: Walk and filter files
    eprint!("Walking directory...");
    let walked = DirectoryWalkerBuilder::new(directory).build().run()?;
    eprintln!(" found {} entries", walked.len());

    let mut files: Vec<(std::path::PathBuf, Vec<u8>, Language)> = Vec::new();
    let mut skipped_binary = 0u64;
    let mut skipped_large = 0u64;
    let mut skipped_read_err = 0u64;
    let total = walked.len();

    for (idx, w) in walked.iter().enumerate() {
        if idx % 100 == 0 || idx + 1 == total {
            let pct = (idx + 1) * 100 / total.max(1);
            eprint!("\x1b[2K\rFiltering files... {pct}% ({}/{})", idx + 1, total);
            let _ = std::io::stderr().flush();
        }
        if is_binary_path(&w.path) {
            skipped_binary += 1;
            continue;
        }
        if w.metadata.len() > DEFAULT_MAX_FILE_SIZE {
            skipped_large += 1;
            continue;
        }
        let content = match std::fs::read(&w.path) {
            Ok(c) => c,
            Err(_) => {
                skipped_read_err += 1;
                continue;
            }
        };
        if is_binary_content(&content) {
            skipped_binary += 1;
            continue;
        }
        let lang = Language::from_path(&w.path);
        files.push((w.path.clone(), content, lang));
    }
    eprintln!("\x1b[2K\rFiltered {total} entries");

    let file_count = files.len() as u64;
    let raw_content_bytes: u64 = files.iter().map(|(_, c, _)| c.len() as u64).sum();

    // Phase 2: Measure compressed content size
    let mut compressed_total: u64 = 0;
    let fc = files.len();
    for (idx, (_, content, _)) in files.iter().enumerate() {
        if idx % 100 == 0 || idx + 1 == fc {
            let pct = (idx + 1) * 100 / fc.max(1);
            eprint!(
                "\x1b[2K\rCompressing content... {pct}% ({}/{})",
                idx + 1,
                fc
            );
            let _ = std::io::stderr().flush();
        }
        let compressed =
            zstd::encode_all(content.as_slice(), 3).map_err(ferret_indexer_core::IndexError::Io)?;
        compressed_total += compressed.len() as u64;
    }
    eprintln!("\x1b[2K\rCompressed {fc} files");

    // Phase 3: Build posting lists and measure encoded size
    let mut file_postings: HashMap<Trigram, Vec<u32>> = HashMap::new();
    let mut total_trigram_occurrences: u64 = 0;
    let mut unique_trigrams_per_file: u64 = 0;

    for (i, (_, content, _)) in files.iter().enumerate() {
        if i % 100 == 0 || i + 1 == fc {
            let pct = (i + 1) * 100 / fc.max(1);
            eprint!(
                "\x1b[2K\rBuilding trigram posting lists... {pct}% ({}/{})",
                i + 1,
                fc
            );
            let _ = std::io::stderr().flush();
        }
        let file_id = i as u32;

        // File-level postings (unique trigrams per file)
        let unique: HashSet<Trigram> = extract_unique_trigrams_folded(content);
        unique_trigrams_per_file += unique.len() as u64;
        for tri in unique {
            file_postings.entry(tri).or_default().push(file_id);
        }

        // Count total occurrences for positional posting estimate
        total_trigram_occurrences += extract_trigrams_folded(content).count() as u64;
    }
    eprintln!("\x1b[2K\rBuilt trigram posting lists for {fc} files");

    let unique_trigram_count = file_postings.len() as u64;

    // Encode file-level posting lists to measure actual size
    let mut file_posting_bytes: u64 = 0;
    let tc = file_postings.len();
    for (idx, (_, ids)) in file_postings.iter().enumerate() {
        if idx % 1000 == 0 || idx + 1 == tc {
            let pct = (idx + 1) * 100 / tc.max(1);
            eprint!(
                "\x1b[2K\rEncoding posting lists... {pct}% ({}/{})",
                idx + 1,
                tc
            );
            let _ = std::io::stderr().flush();
        }
        let encoded = encode_delta_varint(ids);
        file_posting_bytes += encoded.len() as u64;
    }
    eprintln!("\x1b[2K\rEncoded {tc} posting lists");

    // Estimate positional posting size:
    // Each occurrence is (file_id, offset). Grouped by file, delta-encoded.
    // Empirical estimate: ~1.5 bytes per occurrence after delta-varint encoding.
    let positional_posting_bytes_est = (total_trigram_occurrences as f64 * 1.5) as u64;

    // Phase 4: Compute component sizes
    let trigram_table_bytes = TRIGRAM_HEADER_SIZE + unique_trigram_count * TRIGRAM_TABLE_ENTRY_SIZE;

    let trigrams_bin_file_only = trigram_table_bytes + file_posting_bytes;
    let trigrams_bin_with_positions = trigrams_bin_file_only + positional_posting_bytes_est;

    let path_pool_bytes: u64 = files
        .iter()
        .map(|(p, _, _)| {
            p.strip_prefix(directory)
                .unwrap_or(p)
                .to_string_lossy()
                .len() as u64
        })
        .sum();

    let meta_bin = META_HEADER_SIZE + file_count * META_ENTRY_SIZE;
    let paths_bin = path_pool_bytes;
    let content_zst = compressed_total;
    let tombstone_bin = TOMBSTONE_HEADER_SIZE; // empty tombstone per segment

    let total_index = trigrams_bin_file_only + meta_bin + paths_bin + content_zst + tombstone_bin;
    let total_index_with_positions =
        trigrams_bin_with_positions + meta_bin + paths_bin + content_zst + tombstone_bin;

    // Phase 5: Estimate peak RAM during segment build
    let ram_file_postings = unique_trigrams_per_file * 4;
    let ram_positional_postings = total_trigram_occurrences * 8;
    let ram_hashmap_file_only = ((unique_trigram_count * 48) as f64 * 1.5) as u64;
    let ram_hashmap_with_positions = ((unique_trigram_count * 2 * 48) as f64 * 1.5) as u64;
    let ram_file_content = raw_content_bytes;
    let peak_ram = ram_file_postings + ram_hashmap_file_only + ram_file_content;
    let peak_ram_with_positions =
        ram_file_postings + ram_positional_postings + ram_hashmap_with_positions + ram_file_content;

    // Print results to stdout
    println!();
    println!("=== Input ===");
    println!("  Indexable files:  {file_count}");
    println!("  Raw content:      {}", human_bytes(raw_content_bytes));
    println!("  Skipped binary:   {skipped_binary}");
    println!("  Skipped too large:{skipped_large}");
    println!("  Skipped read err: {skipped_read_err}");

    println!();
    println!("=== Index Size Breakdown ===");
    println!("  trigrams.bin:");
    println!(
        "    Trigram table:      {} ({} unique trigrams x {}B)",
        human_bytes(trigram_table_bytes),
        unique_trigram_count,
        TRIGRAM_TABLE_ENTRY_SIZE
    );
    println!(
        "    File postings:      {} (delta-varint encoded)",
        human_bytes(file_posting_bytes)
    );
    println!(
        "    Subtotal:            {}",
        human_bytes(trigrams_bin_file_only)
    );
    println!(
        "    (with positions:    ~{}, {} occurrences)",
        human_bytes(trigrams_bin_with_positions),
        total_trigram_occurrences
    );
    println!();
    println!(
        "  meta.bin:             {} ({} files x {}B + header)",
        human_bytes(meta_bin),
        file_count,
        META_ENTRY_SIZE
    );
    println!("  paths.bin:            {}", human_bytes(paths_bin));
    println!(
        "  content.zst:          {} ({:.1}x compression)",
        human_bytes(content_zst),
        if compressed_total > 0 {
            raw_content_bytes as f64 / compressed_total as f64
        } else {
            0.0
        }
    );
    println!(
        "  tombstones.bin:       {} (empty)",
        human_bytes(tombstone_bin)
    );

    println!();
    println!("=== Totals ===");
    println!("  Raw content:          {}", human_bytes(raw_content_bytes));
    println!("  Estimated index size: ~{}", human_bytes(total_index));
    println!(
        "  Index / raw ratio:    {:.2}x",
        total_index as f64 / raw_content_bytes.max(1) as f64
    );
    println!(
        "  (with positions:     ~{}, {:.2}x)",
        human_bytes(total_index_with_positions),
        total_index_with_positions as f64 / raw_content_bytes.max(1) as f64
    );

    println!();
    println!("=== Peak RAM (single-segment build) ===");
    println!(
        "  File content in memory:   {}",
        human_bytes(ram_file_content)
    );
    println!(
        "  File-level postings:      {}",
        human_bytes(ram_file_postings)
    );
    println!(
        "  HashMap overhead:         ~{}",
        human_bytes(ram_hashmap_file_only)
    );
    println!("  Estimated peak total:     ~{}", human_bytes(peak_ram));
    println!(
        "  (with positions:         ~{})",
        human_bytes(peak_ram_with_positions)
    );

    // Per-segment estimates: scale proportionally by content budget
    println!();
    if raw_content_bytes > 0 && segment_budget_bytes > 0 {
        let num_segments = raw_content_bytes.div_ceil(segment_budget_bytes);
        let fraction = if raw_content_bytes > segment_budget_bytes {
            segment_budget_bytes as f64 / raw_content_bytes as f64
        } else {
            1.0
        };
        let seg_ram_content = (ram_file_content as f64 * fraction) as u64;
        let seg_ram_file_postings = (ram_file_postings as f64 * fraction) as u64;
        let seg_ram_positional = (ram_positional_postings as f64 * fraction) as u64;
        let seg_ram_hashmap = (ram_hashmap_file_only as f64 * fraction) as u64;
        let seg_ram_hashmap_with_pos = (ram_hashmap_with_positions as f64 * fraction) as u64;
        let seg_peak_ram = seg_ram_content + seg_ram_file_postings + seg_ram_hashmap;
        let seg_peak_ram_with_pos =
            seg_ram_content + seg_ram_file_postings + seg_ram_positional + seg_ram_hashmap_with_pos;

        println!("=== Peak RAM (budgeted, {segment_budget_mb} MB/segment) ===");
        println!("  Estimated segments:       {num_segments}");
        println!(
            "  File content in memory:   {}",
            human_bytes(seg_ram_content)
        );
        println!(
            "  File-level postings:      {}",
            human_bytes(seg_ram_file_postings)
        );
        println!(
            "  HashMap overhead:         ~{}",
            human_bytes(seg_ram_hashmap)
        );
        println!("  Estimated peak total:     ~{}", human_bytes(seg_peak_ram));
        println!(
            "  (with positions:         ~{})",
            human_bytes(seg_peak_ram_with_pos)
        );
    } else {
        println!("=== Peak RAM (budgeted) ===");
        println!("  (no content or no budget to estimate)");
    }

    println!();
    eprintln!("Done in {:.1?}", t0.elapsed());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_estimate_empty_directory() {
        let dir = tempdir().unwrap();
        // Need a .git dir so walker doesn't walk up
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let result = run_estimate(dir.path(), 256);
        assert!(result.is_ok());
    }

    #[test]
    fn test_estimate_with_files() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let result = run_estimate(dir.path(), 256);
        assert!(result.is_ok());
    }
}
