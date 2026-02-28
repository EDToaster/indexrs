//! Full-pipeline demo of the indexrs indexing + search + change detection system.
//!
//! Usage:
//!   cargo run -p indexrs-core --example demo -- <directory> <query>
//!
//! Examples:
//!   cargo run -p indexrs-core --example demo -- ./indexrs-core/src "Trigram"
//!   cargo run -p indexrs-core --example demo -- . "IndexError"
//!   cargo run -p indexrs-core --example demo -- . "fn main"

use std::path::PathBuf;
use std::time::Instant;

use indexrs_core::{
    DEFAULT_MAX_FILE_SIZE,
    // M2: file discovery & classification
    DirectoryWalkerBuilder,
    // M2: change detection
    GitChangeDetector,
    // M3: segment-based indexing & search
    InputFile,
    Language,
    SegmentManager,
    is_binary_content,
    is_binary_path,
    search_segments,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <directory> <query>", args[0]);
        eprintln!();
        eprintln!("Demos the full indexrs pipeline:");
        eprintln!("  1. Directory walking with .gitignore support (M2)");
        eprintln!("  2. Binary file detection and language classification (M2)");
        eprintln!("  3. Segment-based index build via SegmentManager (M3)");
        eprintln!("  4. Multi-segment search with ranked results (M4)");
        eprintln!("  5. Git-based change detection (M2)");
        std::process::exit(1);
    }
    let dir = PathBuf::from(&args[1]);
    let query = &args[2];

    // Use a temp directory for the index (auto-cleaned on drop)
    let index_tmp = tempfile::tempdir()?;
    let index_dir = index_tmp.path().join(".indexrs");

    // ── Phase 1: Walk directory (M2: DirectoryWalkerBuilder) ─────────────
    let t0 = Instant::now();
    eprintln!("=== Phase 1: Directory Walking ===");
    eprintln!("Root: {}", dir.display());

    let walked_files = DirectoryWalkerBuilder::new(&dir).build().run()?;
    eprintln!(
        "Walker found {} entries (respecting .gitignore) in {:.1?}",
        walked_files.len(),
        t0.elapsed()
    );

    // ── Phase 2: Filter & classify files (M2: binary detection + language) ──
    let t1 = Instant::now();
    eprintln!("\n=== Phase 2: Binary Detection & Language Classification ===");

    let mut input_files: Vec<InputFile> = Vec::new();
    let mut lang_counts: std::collections::HashMap<Language, u32> =
        std::collections::HashMap::new();
    let mut skipped_binary = 0u32;
    let mut skipped_large = 0u32;

    for walked in &walked_files {
        if is_binary_path(&walked.path) {
            skipped_binary += 1;
            continue;
        }

        if walked.metadata.len() > DEFAULT_MAX_FILE_SIZE {
            skipped_large += 1;
            continue;
        }

        let content = match std::fs::read(&walked.path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if is_binary_content(&content) {
            skipped_binary += 1;
            continue;
        }

        let lang = Language::from_path(&walked.path);
        *lang_counts.entry(lang).or_default() += 1;

        let rel_path = walked.path.strip_prefix(&dir).unwrap_or(&walked.path);
        let mtime = walked
            .metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        input_files.push(InputFile {
            path: rel_path.to_string_lossy().to_string(),
            content,
            mtime,
        });
    }

    let mut lang_vec: Vec<_> = lang_counts.into_iter().collect();
    lang_vec.sort_by(|a, b| b.1.cmp(&a.1));

    eprintln!(
        "Indexable: {} text files, skipped: {} binary, {} too large",
        input_files.len(),
        skipped_binary,
        skipped_large
    );
    eprintln!("Languages:");
    for (lang, count) in &lang_vec {
        eprintln!("  {lang}: {count}");
    }
    eprintln!("Classified in {:.1?}", t1.elapsed());

    // ── Phase 3: Build index (M3: SegmentManager) ───────────────────────
    let t2 = Instant::now();
    eprintln!("\n=== Phase 3: Segment Build ===");

    let file_count = input_files.len();
    let manager = SegmentManager::new(&index_dir)?;
    manager.index_files(input_files)?;

    let snap = manager.snapshot();
    eprintln!(
        "Built {} segment(s) covering {} files in {:.1?}",
        snap.len(),
        file_count,
        t2.elapsed()
    );

    // ── Phase 4: Search (M3+M4: multi-segment search with ranking) ──────
    let t3 = Instant::now();
    eprintln!("\n=== Phase 4: Search ===");
    eprintln!("Query: {query:?}");

    let result = search_segments(&snap, query)?;

    if result.total_match_count == 0 {
        println!("No results found.");
    } else {
        // SearchResult implements Display with ranked output
        print!("{result}");
        eprintln!(
            "\n{} matches in {} files (searched in {:.1?})",
            result.total_match_count,
            result.total_file_count,
            t3.elapsed()
        );
    }

    // ── Phase 5: Git change detection (M2) ──────────────────────────────
    eprintln!("\n=== Phase 5: Git Change Detection ===");

    let abs_dir = std::fs::canonicalize(&dir)?;
    let git_detector = GitChangeDetector::new(abs_dir);
    match git_detector.get_head_sha() {
        Ok(sha) => {
            eprintln!("HEAD: {}", &sha[..12]);
            match git_detector.detect_changes() {
                Ok(changes) => {
                    if changes.is_empty() {
                        eprintln!("No uncommitted changes detected.");
                    } else {
                        eprintln!("{} changed file(s):", changes.len());
                        for event in &changes {
                            eprintln!("  {:?}: {}", event.kind, event.path.display());
                        }
                    }
                }
                Err(e) => eprintln!("Change detection failed: {e}"),
            }
        }
        Err(e) => eprintln!("Not a git repo or git unavailable: {e}"),
    }

    eprintln!("\n=== Done (total: {:.1?}) ===", t0.elapsed());
    Ok(())
}
