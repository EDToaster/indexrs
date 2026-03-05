//! Benchmark: estimate index disk space for a directory.
//!
//! Walks a directory, filters to indexable text files, then measures:
//! - Raw content size
//! - Zstd-compressed content size (content.zst)
//! - Trigram posting list size (delta-varint encoded)
//! - Metadata + path pool size (meta.bin + paths.bin)
//! - Tombstone overhead per segment
//!
//! Usage:
//!   cargo run -p ferret-indexer-core --example bench_space -- <directory> [segment-budget-mb]
//!   cargo run -p ferret-indexer-core --example bench_space --release -- <directory>
//!   cargo run -p ferret-indexer-core --example bench_space --release -- <directory> 256

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use ferret_indexer_core::{
    DEFAULT_MAX_FILE_SIZE, DirectoryWalkerBuilder, Language, Trigram, encode_delta_varint,
    extract_trigrams_folded, extract_unique_trigrams_folded, is_binary_content, is_binary_path,
};

/// Metadata entry size in meta.bin (fixed 58 bytes per file).
const META_ENTRY_SIZE: usize = 58;

/// Tombstone header size per segment.
const TOMBSTONE_HEADER_SIZE: usize = 14;

/// Trigram table entry size (19 bytes: 3B trigram + 4x u32 offsets/lengths).
const TRIGRAM_TABLE_ENTRY_SIZE: usize = 19;

/// Trigram index header size.
const TRIGRAM_HEADER_SIZE: usize = 10;

/// Metadata header size.
const META_HEADER_SIZE: usize = 10;

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

/// Default segment budget: 256 MB (matches DEFAULT_COMPACTION_BUDGET).
const DEFAULT_SEGMENT_BUDGET_MB: u64 = 256;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <directory> [segment-budget-mb]", args[0]);
        eprintln!();
        eprintln!("Estimates how much disk space an ferret index would use.");
        eprintln!(
            "Optional segment-budget-mb sets per-segment content cap (default: {DEFAULT_SEGMENT_BUDGET_MB})."
        );
        std::process::exit(1);
    }
    let dir = PathBuf::from(&args[1]);
    let segment_budget_mb: u64 = args
        .get(2)
        .map(|s| s.parse().expect("segment-budget-mb must be a number"))
        .unwrap_or(DEFAULT_SEGMENT_BUDGET_MB);
    let segment_budget_bytes = segment_budget_mb * 1024 * 1024;

    let t0 = Instant::now();

    // Phase 1: Walk and filter files
    eprint!("Walking directory...");
    let walked = DirectoryWalkerBuilder::new(&dir).build().run()?;
    eprintln!(" found {} entries", walked.len());

    let mut files: Vec<(PathBuf, Vec<u8>, Language)> = Vec::new();
    let mut skipped_binary = 0u64;
    let mut skipped_large = 0u64;
    let mut skipped_read_err = 0u64;
    let total = walked.len();

    for (idx, w) in walked.iter().enumerate() {
        if idx % 100 == 0 || idx + 1 == total {
            let pct = (idx + 1) * 100 / total;
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

    eprintln!();
    eprintln!("=== Input ===");
    eprintln!("  Indexable files:  {file_count}");
    eprintln!("  Raw content:      {}", human_bytes(raw_content_bytes));
    eprintln!("  Skipped binary:   {skipped_binary}");
    eprintln!("  Skipped too large:{skipped_large}");
    eprintln!("  Skipped read err: {skipped_read_err}");

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
        let compressed = zstd::encode_all(content.as_slice(), 3)?;
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
    // Empirical estimate: ~1.5 bytes per occurrence after delta-varint encoding
    // (offsets within a file have small deltas, file_id groups are cheap).
    let positional_posting_bytes_est = (total_trigram_occurrences as f64 * 1.5) as u64;

    // Phase 4: Compute component sizes
    let trigram_table_bytes =
        TRIGRAM_HEADER_SIZE as u64 + unique_trigram_count * TRIGRAM_TABLE_ENTRY_SIZE as u64;

    let trigrams_bin_file_only = trigram_table_bytes + file_posting_bytes;
    let trigrams_bin_with_positions = trigrams_bin_file_only + positional_posting_bytes_est;

    let path_pool_bytes: u64 = files
        .iter()
        .map(|(p, _, _)| p.strip_prefix(&dir).unwrap_or(p).to_string_lossy().len() as u64)
        .sum();

    let meta_bin = META_HEADER_SIZE as u64 + file_count * META_ENTRY_SIZE as u64;
    let paths_bin = path_pool_bytes;
    let content_zst = compressed_total;
    let tombstone_bin = TOMBSTONE_HEADER_SIZE as u64; // empty tombstone per segment

    let total_index = trigrams_bin_file_only + meta_bin + paths_bin + content_zst + tombstone_bin;
    let total_index_with_positions =
        trigrams_bin_with_positions + meta_bin + paths_bin + content_zst + tombstone_bin;

    // Phase 5: Estimate peak RAM during segment build
    // PostingListBuilder holds:
    //   file_postings: HashMap<Trigram, Vec<FileId>> — ~unique_trigrams_per_file * 4 bytes
    //   positional_postings (if enabled): HashMap<Trigram, Vec<(FileId, u32)>> — ~total_occurrences * 8 bytes
    let ram_file_postings = unique_trigrams_per_file * 4;
    let ram_positional_postings = total_trigram_occurrences * 8;
    // HashMap overhead: ~1.5x for buckets/pointers
    let ram_hashmap_file_only = ((unique_trigram_count * 48) as f64 * 1.5) as u64; // one hashmap, ~48 bytes per bucket
    let ram_hashmap_with_positions = ((unique_trigram_count * 2 * 48) as f64 * 1.5) as u64; // two hashmaps
    let ram_file_content = raw_content_bytes; // InputFile content held in memory
    let peak_ram = ram_file_postings + ram_hashmap_file_only + ram_file_content;
    let peak_ram_with_positions =
        ram_file_postings + ram_positional_postings + ram_hashmap_with_positions + ram_file_content;

    eprintln!();
    eprintln!("=== Index Size Breakdown ===");
    eprintln!("  trigrams.bin:");
    eprintln!(
        "    Trigram table:      {} ({} unique trigrams x {}B)",
        human_bytes(trigram_table_bytes),
        unique_trigram_count,
        TRIGRAM_TABLE_ENTRY_SIZE
    );
    eprintln!(
        "    File postings:      {} (delta-varint encoded)",
        human_bytes(file_posting_bytes)
    );
    eprintln!(
        "    Subtotal:            {}",
        human_bytes(trigrams_bin_file_only)
    );
    eprintln!(
        "    (with positions:    ~{}, {} occurrences)",
        human_bytes(trigrams_bin_with_positions),
        total_trigram_occurrences
    );
    eprintln!();
    eprintln!(
        "  meta.bin:             {} ({} files x {}B + header)",
        human_bytes(meta_bin),
        file_count,
        META_ENTRY_SIZE
    );
    eprintln!("  paths.bin:            {}", human_bytes(paths_bin));
    eprintln!(
        "  content.zst:          {} ({:.1}x compression)",
        human_bytes(content_zst),
        if compressed_total > 0 {
            raw_content_bytes as f64 / compressed_total as f64
        } else {
            0.0
        }
    );
    eprintln!(
        "  tombstones.bin:       {} (empty)",
        human_bytes(tombstone_bin)
    );

    eprintln!();
    eprintln!("=== Totals ===");
    eprintln!("  Raw content:          {}", human_bytes(raw_content_bytes));
    eprintln!("  Estimated index size: ~{}", human_bytes(total_index));
    eprintln!(
        "  Index / raw ratio:    {:.2}x",
        total_index as f64 / raw_content_bytes.max(1) as f64
    );
    eprintln!(
        "  (with positions:     ~{}, {:.2}x)",
        human_bytes(total_index_with_positions),
        total_index_with_positions as f64 / raw_content_bytes.max(1) as f64
    );
    eprintln!();
    eprintln!("=== Peak RAM (single-segment build) ===");
    eprintln!(
        "  File content in memory:   {}",
        human_bytes(ram_file_content)
    );
    eprintln!(
        "  File-level postings:      {}",
        human_bytes(ram_file_postings)
    );
    eprintln!(
        "  HashMap overhead:         ~{}",
        human_bytes(ram_hashmap_file_only)
    );
    eprintln!("  Estimated peak total:     ~{}", human_bytes(peak_ram));
    eprintln!(
        "  (with positions:         ~{})",
        human_bytes(peak_ram_with_positions)
    );

    // Per-segment estimates: scale proportionally by content budget
    eprintln!();
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

        eprintln!("=== Peak RAM (budgeted, {segment_budget_mb} MB/segment) ===");
        eprintln!("  Estimated segments:       {num_segments}");
        eprintln!(
            "  File content in memory:   {}",
            human_bytes(seg_ram_content)
        );
        eprintln!(
            "  File-level postings:      {}",
            human_bytes(seg_ram_file_postings)
        );
        eprintln!(
            "  HashMap overhead:         ~{}",
            human_bytes(seg_ram_hashmap)
        );
        eprintln!("  Estimated peak total:     ~{}", human_bytes(seg_peak_ram));
        eprintln!(
            "  (with positions:         ~{})",
            human_bytes(seg_peak_ram_with_pos)
        );
    } else {
        eprintln!("=== Peak RAM (budgeted) ===");
        eprintln!("  (no content or no budget to estimate)");
    }

    eprintln!();
    eprintln!("Done in {:.1?}", t0.elapsed());

    Ok(())
}
