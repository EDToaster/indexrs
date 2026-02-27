//! Quick demo of the indexrs indexing + search pipeline.
//!
//! Usage: cargo run -p indexrs-core --example demo -- <directory> <query>
//!
//! Example: cargo run -p indexrs-core --example demo -- ./indexrs-core/src "Trigram"

use indexrs_core::{
    ContentStoreReader, ContentStoreWriter, FileId, FileMetadata, Language, MetadataBuilder,
    PostingListBuilder, TrigramIndexReader, TrigramIndexWriter,
};
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <directory> <query>", args[0]);
        std::process::exit(1);
    }
    let dir = PathBuf::from(&args[1]);
    let query = &args[2];

    // Create a temp directory for the index
    let index_dir = std::env::temp_dir().join("indexrs-demo");
    std::fs::create_dir_all(&index_dir)?;

    // Phase 1: Walk directory and collect files
    eprintln!("Indexing {}...", dir.display());
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    walk_dir(&dir, &dir, &mut files)?;
    eprintln!("Found {} files", files.len());

    // Phase 2: Build the index
    let mut posting_builder = PostingListBuilder::new();
    let mut metadata_builder = MetadataBuilder::new();
    let mut content_writer = ContentStoreWriter::new(&index_dir.join("content.zst"))?;

    for (i, (path, content)) in files.iter().enumerate() {
        let file_id = FileId(i as u32);
        let rel_path = path.strip_prefix(&dir).unwrap_or(path);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

        // Add to posting lists
        posting_builder.add_file(file_id, content);

        // Add to content store
        let (offset, compressed_len) = content_writer.add_content(content)?;

        // Add to metadata
        let hash = blake3::hash(content);
        let mut content_hash = [0u8; 16];
        content_hash.copy_from_slice(&hash.as_bytes()[..16]);

        metadata_builder.add_file(FileMetadata {
            file_id,
            path: rel_path.to_string_lossy().to_string(),
            content_hash,
            language: Language::from_extension(ext),
            size_bytes: content.len() as u32,
            mtime_epoch_secs: 0,
            line_count: content.iter().filter(|&&b| b == b'\n').count() as u32,
            content_offset: offset,
            content_len: compressed_len,
        });
    }

    posting_builder.finalize();
    content_writer.finish()?;

    // Write trigram index to disk
    TrigramIndexWriter::write(&posting_builder, &index_dir.join("trigrams.bin"))?;

    eprintln!(
        "Index built: {} trigrams, {} files",
        posting_builder.trigram_count(),
        metadata_builder.file_count()
    );

    // Phase 3: Search
    eprintln!("Searching for {:?}...\n", query);
    let reader = TrigramIndexReader::open(&index_dir.join("trigrams.bin"))?;
    let content_reader = ContentStoreReader::open(&index_dir.join("content.zst"))?;

    let candidates = indexrs_core::find_candidates(&reader, query)?;

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

            // Decompress and verify against actual content
            let content = content_reader.read_content(meta.content_offset, meta.content_len)?;
            let text = String::from_utf8_lossy(&content);

            for (line_num, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    println!("{}:{}:{}", meta.path, line_num + 1, line);
                    match_count += 1;
                }
            }
        }

        eprintln!("\n{} matches in {} candidate files", match_count, candidates.len());
    }

    // Cleanup
    std::fs::remove_dir_all(&index_dir)?;
    Ok(())
}

fn walk_dir(dir: &Path, root: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) -> std::io::Result<()> {
    for entry in ignore::Walk::new(dir) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Skip binary files (simple heuristic: check first 512 bytes)
        if let Ok(content) = std::fs::read(path) {
            let sample = &content[..content.len().min(512)];
            if sample.contains(&0) {
                continue; // likely binary
            }
            files.push((path.to_path_buf(), content));
        }
    }
    Ok(())
}
