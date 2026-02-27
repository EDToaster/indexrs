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
    // M0: types
    FileId, FileMetadata, Language,
    // M1: indexing pipeline
    ContentStoreReader, ContentStoreWriter, MetadataBuilder, PostingListBuilder,
    TrigramIndexReader, TrigramIndexWriter,
    // M1: search
    find_candidates,
    // M2: file discovery
    DirectoryWalkerBuilder,
    // M2: binary detection
    is_binary_content, is_binary_path, DEFAULT_MAX_FILE_SIZE,
    // M2: change detection
    GitChangeDetector,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <directory> <query>", args[0]);
        eprintln!();
        eprintln!("Demos the full indexrs pipeline:");
        eprintln!("  1. Directory walking with .gitignore support (M2)");
        eprintln!("  2. Binary file detection and language classification (M2)");
        eprintln!("  3. Trigram index build: posting lists + content store (M1)");
        eprintln!("  4. Trigram search with candidate verification (M1)");
        eprintln!("  5. Git-based change detection (M2)");
        std::process::exit(1);
    }
    let dir = PathBuf::from(&args[1]);
    let query = &args[2];

    // Temp directory for index files (auto-cleaned on drop)
    let index_tmp = tempfile::tempdir()?;
    let index_dir = index_tmp.path();

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

    let mut text_files: Vec<(PathBuf, Vec<u8>, Language)> = Vec::new();
    let mut skipped_binary = 0u32;
    let mut skipped_large = 0u32;

    for walked in &walked_files {
        // Skip binary extensions without reading
        if is_binary_path(&walked.path) {
            skipped_binary += 1;
            continue;
        }

        // Skip files over size limit
        if walked.metadata.len() > DEFAULT_MAX_FILE_SIZE {
            skipped_large += 1;
            continue;
        }

        // Read and check content for binary data
        let content = match std::fs::read(&walked.path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if is_binary_content(&content) {
            skipped_binary += 1;
            continue;
        }

        // Detect language (M2: Language::from_path)
        let lang = Language::from_path(&walked.path);
        text_files.push((walked.path.clone(), content, lang));
    }

    // Show language breakdown
    let mut lang_counts: std::collections::HashMap<Language, u32> = std::collections::HashMap::new();
    for (_, _, lang) in &text_files {
        *lang_counts.entry(*lang).or_default() += 1;
    }
    let mut lang_vec: Vec<_> = lang_counts.into_iter().collect();
    lang_vec.sort_by(|a, b| b.1.cmp(&a.1));

    eprintln!(
        "Indexable: {} text files, skipped: {} binary, {} too large",
        text_files.len(),
        skipped_binary,
        skipped_large
    );
    eprintln!("Languages:");
    for (lang, count) in &lang_vec {
        eprintln!("  {lang}: {count}");
    }
    eprintln!("Classified in {:.1?}", t1.elapsed());

    // ── Phase 3: Build index (M1: posting lists + content store + metadata) ─
    let t2 = Instant::now();
    eprintln!("\n=== Phase 3: Index Build ===");

    let mut posting_builder = PostingListBuilder::new();
    let mut metadata_builder = MetadataBuilder::new();
    let mut content_writer = ContentStoreWriter::new(&index_dir.join("content.zst"))?;

    for (i, (path, content, lang)) in text_files.iter().enumerate() {
        let file_id = FileId(i as u32);
        let rel_path = path.strip_prefix(&dir).unwrap_or(path);
        posting_builder.add_file(file_id, content);
        let (offset, compressed_len) = content_writer.add_content(content)?;
        let hash = blake3::hash(content);
        let mut content_hash = [0u8; 16];
        content_hash.copy_from_slice(&hash.as_bytes()[..16]);

        metadata_builder.add_file(FileMetadata {
            file_id,
            path: rel_path.to_string_lossy().to_string(),
            content_hash,
            language: *lang,
            size_bytes: content.len() as u32,
            mtime_epoch_secs: 0,
            line_count: content.iter().filter(|&&b| b == b'\n').count() as u32,
            content_offset: offset,
            content_len: compressed_len,
        });
    }

    posting_builder.finalize();
    content_writer.finish()?;
    TrigramIndexWriter::write(&posting_builder, &index_dir.join("trigrams.bin"))?;

    eprintln!(
        "Built index: {} trigrams across {} files in {:.1?}",
        posting_builder.trigram_count(),
        metadata_builder.file_count(),
        t2.elapsed()
    );

    // ── Phase 4: Search (M1: trigram lookup + intersection + verification) ───
    let t3 = Instant::now();
    eprintln!("\n=== Phase 4: Search ===");
    eprintln!("Query: {:?}", query);

    let reader = TrigramIndexReader::open(&index_dir.join("trigrams.bin"))?;
    let content_reader = ContentStoreReader::open(&index_dir.join("content.zst"))?;
    let candidates = find_candidates(&reader, query)?;

    if candidates.is_empty() {
        println!("No results found.");
    } else {
        let re = regex::Regex::new(&regex::escape(query))?;
        let mut match_count = 0;

        for file_id in &candidates {
            let meta = match metadata_builder.get(*file_id) {
                Some(m) => m,
                None => continue,
            };
            let content = content_reader.read_content(meta.content_offset, meta.content_len)?;
            let text = String::from_utf8_lossy(&content);

            for (line_num, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    println!("{}:{}:{}", meta.path, line_num + 1, line);
                    match_count += 1;
                }
            }
        }

        eprintln!(
            "\n{} matches in {} candidate files (searched in {:.1?})",
            match_count,
            candidates.len(),
            t3.elapsed()
        );
    }

    // ── Phase 5: Git change detection (M2) ──────────────────────────────────
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
