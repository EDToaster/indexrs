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
//!   cargo run -p indexrs-core --example bench_space -- <directory>
//!   cargo run -p indexrs-core --example bench_space --release -- <directory>

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use indexrs_core::{
    DEFAULT_MAX_FILE_SIZE, DirectoryWalkerBuilder, Language, Trigram,
    encode_delta_varint, extract_trigrams, extract_unique_trigrams,
    is_binary_content, is_binary_path,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <directory>", args[0]);
        eprintln!();
        eprintln!("Estimates how much disk space an indexrs index would use.");
        std::process::exit(1);
    }
    let dir = PathBuf::from(&args[1]);

    let t0 = Instant::now();

    // Phase 1: Walk and filter files
    eprint!("Walking directory...");
    let walked = DirectoryWalkerBuilder::new(&dir).build().run()?;
    eprintln!(" found {} entries", walked.len());

    let mut files: Vec<(PathBuf, Vec<u8>, Language)> = Vec::new();
    let mut skipped_binary = 0u64;
    let mut skipped_large = 0u64;
    let mut skipped_read_err = 0u64;

    for w in &walked {
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
    eprint!("Compressing content...");
    let mut compressed_total: u64 = 0;
    for (_, content, _) in &files {
        let compressed = zstd::encode_all(content.as_slice(), 3)?;
        compressed_total += compressed.len() as u64;
    }
    eprintln!(" done");

    // Phase 3: Build posting lists and measure encoded size
    eprint!("Building trigram posting lists...");
    let mut file_postings: HashMap<Trigram, Vec<u32>> = HashMap::new();
    let mut total_trigram_occurrences: u64 = 0;
    let mut unique_trigrams_per_file: u64 = 0;

    for (i, (_, content, _)) in files.iter().enumerate() {
        let file_id = i as u32;

        // File-level postings (unique trigrams per file)
        let unique: HashSet<Trigram> = extract_unique_trigrams(content);
        unique_trigrams_per_file += unique.len() as u64;
        for tri in unique {
            file_postings.entry(tri).or_default().push(file_id);
        }

        // Count total occurrences for positional posting estimate
        total_trigram_occurrences += extract_trigrams(content).count() as u64;
    }
    eprintln!(" done");

    let unique_trigram_count = file_postings.len() as u64;

    // Encode file-level posting lists to measure actual size
    eprint!("Encoding posting lists...");
    let mut file_posting_bytes: u64 = 0;
    for (_, ids) in &file_postings {
        let encoded = encode_delta_varint(ids);
        file_posting_bytes += encoded.len() as u64;
    }
    eprintln!(" done");

    // Estimate positional posting size:
    // Each occurrence is (file_id, offset). Grouped by file, delta-encoded.
    // Empirical estimate: ~1.5 bytes per occurrence after delta-varint encoding
    // (offsets within a file have small deltas, file_id groups are cheap).
    let positional_posting_bytes_est = (total_trigram_occurrences as f64 * 1.5) as u64;

    // Phase 4: Compute component sizes
    let trigram_table_bytes = TRIGRAM_HEADER_SIZE as u64
        + unique_trigram_count * TRIGRAM_TABLE_ENTRY_SIZE as u64;

    let trigrams_bin = trigram_table_bytes + file_posting_bytes + positional_posting_bytes_est;

    let path_pool_bytes: u64 = files
        .iter()
        .map(|(p, _, _)| {
            p.strip_prefix(&dir)
                .unwrap_or(p)
                .to_string_lossy()
                .len() as u64
        })
        .sum();

    let meta_bin = META_HEADER_SIZE as u64 + file_count * META_ENTRY_SIZE as u64;
    let paths_bin = path_pool_bytes;
    let content_zst = compressed_total;
    let tombstone_bin = TOMBSTONE_HEADER_SIZE as u64; // empty tombstone per segment

    let total_index = trigrams_bin + meta_bin + paths_bin + content_zst + tombstone_bin;

    // Phase 5: Estimate peak RAM during segment build
    // PostingListBuilder holds:
    //   file_postings: HashMap<Trigram, Vec<FileId>> — ~unique_trigrams_per_file * 4 bytes
    //   positional_postings: HashMap<Trigram, Vec<(FileId, u32)>> — ~total_occurrences * 8 bytes
    let ram_file_postings = unique_trigrams_per_file * 4;
    let ram_positional_postings = total_trigram_occurrences * 8;
    // HashMap overhead: ~1.5x for buckets/pointers
    let ram_hashmap_overhead =
        ((unique_trigram_count * 2 * 48) as f64 * 1.5) as u64; // two hashmaps, ~48 bytes per bucket
    let ram_file_content = raw_content_bytes; // InputFile content held in memory
    let peak_ram = ram_file_postings + ram_positional_postings + ram_hashmap_overhead + ram_file_content;

    eprintln!();
    eprintln!("=== Index Size Breakdown ===");
    eprintln!("  trigrams.bin:");
    eprintln!("    Trigram table:      {} ({} unique trigrams x {}B)",
        human_bytes(trigram_table_bytes), unique_trigram_count, TRIGRAM_TABLE_ENTRY_SIZE);
    eprintln!("    File postings:      {} (delta-varint encoded)",
        human_bytes(file_posting_bytes));
    eprintln!("    Positional postings:~{} (estimated, {} occurrences)",
        human_bytes(positional_posting_bytes_est), total_trigram_occurrences);
    eprintln!("    Subtotal:           ~{}", human_bytes(trigrams_bin));
    eprintln!();
    eprintln!("  meta.bin:             {} ({} files x {}B + header)",
        human_bytes(meta_bin), file_count, META_ENTRY_SIZE);
    eprintln!("  paths.bin:            {}", human_bytes(paths_bin));
    eprintln!("  content.zst:          {} ({:.1}x compression)",
        human_bytes(content_zst),
        if compressed_total > 0 { raw_content_bytes as f64 / compressed_total as f64 } else { 0.0 });
    eprintln!("  tombstones.bin:       {} (empty)", human_bytes(tombstone_bin));

    eprintln!();
    eprintln!("=== Totals ===");
    eprintln!("  Raw content:          {}", human_bytes(raw_content_bytes));
    eprintln!("  Estimated index size: ~{}", human_bytes(total_index));
    eprintln!("  Index / raw ratio:    {:.2}x", total_index as f64 / raw_content_bytes.max(1) as f64);
    eprintln!();
    eprintln!("=== Peak RAM (single-segment build) ===");
    eprintln!("  File content in memory:   {}", human_bytes(ram_file_content));
    eprintln!("  File-level postings:      {}", human_bytes(ram_file_postings));
    eprintln!("  Positional postings:      {}", human_bytes(ram_positional_postings));
    eprintln!("  HashMap overhead:         ~{}", human_bytes(ram_hashmap_overhead));
    eprintln!("  Estimated peak total:     ~{}", human_bytes(peak_ram));

    eprintln!();
    eprintln!("Done in {:.1?}", t0.elapsed());

    Ok(())
}
