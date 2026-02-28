//! Posting list intersection for multi-trigram queries.
//!
//! Provides sorted merge intersection of file ID lists and a high-level
//! [`find_candidates`] function that extracts trigrams from a query string,
//! looks up each in the index, and intersects the results to produce a set
//! of candidate file IDs.
//!
//! # Algorithm
//!
//! [`intersect_file_ids`] uses a sorted merge strategy:
//! 1. Sort input lists by length (smallest first) to minimize work.
//! 2. Use two-pointer merge for pairwise intersection.
//!
//! [`find_candidates`] ties it together:
//! 1. Extract unique trigrams from the query string.
//! 2. Look up each trigram's file posting list in the index.
//! 3. Intersect all lists to find files containing every trigram.
//!
//! Queries shorter than 3 characters cannot produce trigrams and return an
//! empty result.

use crate::error::IndexError;
use crate::index_reader::TrigramIndexReader;
use crate::trigram::extract_unique_trigrams_folded;
use crate::types::FileId;

/// Intersect multiple sorted file_id lists. Returns file_ids present in ALL lists.
///
/// Uses sorted merge starting with the smallest list for efficiency. Each
/// pairwise intersection uses a two-pointer merge that runs in O(n + m) time.
///
/// # Edge Cases
///
/// - Empty input (`lists` is empty): returns an empty vector.
/// - Single list: returns a clone of that list.
/// - Any list is empty: returns an empty vector (no file can be in all lists).
///
/// # Examples
///
/// ```
/// use indexrs_core::intersection::intersect_file_ids;
/// use indexrs_core::types::FileId;
///
/// let lists = vec![
///     vec![FileId(0), FileId(1), FileId(2)],
///     vec![FileId(1), FileId(2), FileId(3)],
///     vec![FileId(2), FileId(3), FileId(4)],
/// ];
/// let result = intersect_file_ids(&lists);
/// assert_eq!(result, vec![FileId(2)]);
/// ```
pub fn intersect_file_ids(lists: &[Vec<FileId>]) -> Vec<FileId> {
    if lists.is_empty() {
        return Vec::new();
    }

    if lists.len() == 1 {
        return lists[0].clone();
    }

    // Early exit if any list is empty
    if lists.iter().any(|l| l.is_empty()) {
        return Vec::new();
    }

    // Sort by length (smallest first) for efficiency
    let mut indices: Vec<usize> = (0..lists.len()).collect();
    indices.sort_by_key(|&i| lists[i].len());

    // Start with the smallest list and intersect pairwise
    let mut result = lists[indices[0]].clone();

    for &idx in &indices[1..] {
        result = intersect_two(&result, &lists[idx]);
        if result.is_empty() {
            break;
        }
    }

    result
}

/// Intersect two sorted FileId vectors using two-pointer merge.
fn intersect_two(a: &[FileId], b: &[FileId]) -> Vec<FileId> {
    let mut result = Vec::new();
    let mut i = 0;
    let mut j = 0;

    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }

    result
}

/// Given a search string, extract its trigrams, look up each in the index,
/// and intersect to find candidate files.
///
/// Returns the set of file IDs that contain every trigram extracted from the
/// query string. This is the candidate set that must then be verified against
/// the actual file content to confirm the match.
///
/// # Edge Cases
///
/// - Queries shorter than 3 characters return an empty vector, since no
///   trigrams can be extracted.
/// - If any trigram has no matches in the index, the result is empty.
///
/// # Errors
///
/// Returns [`IndexError`] if any trigram lookup fails (e.g., corrupted index).
pub fn find_candidates(
    reader: &TrigramIndexReader,
    query: &str,
) -> Result<Vec<FileId>, IndexError> {
    // Queries shorter than 3 chars cannot produce trigrams
    if query.len() < 3 {
        return Ok(Vec::new());
    }

    let trigrams = extract_unique_trigrams_folded(query.as_bytes());

    if trigrams.is_empty() {
        return Ok(Vec::new());
    }

    let mut lists = Vec::with_capacity(trigrams.len());

    for trigram in &trigrams {
        let file_ids = reader.lookup_file_ids(*trigram)?;
        lists.push(file_ids);
    }

    Ok(intersect_file_ids(&lists))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_writer::TrigramIndexWriter;
    use crate::posting::PostingListBuilder;

    /// Build the Appendix A posting list builder (2 files).
    fn build_appendix_a() -> PostingListBuilder {
        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(0), b"fn main() {}");
        builder.add_file(FileId(1), b"fn parse() {}");
        builder.finalize();
        builder
    }

    /// Write Appendix A index and open reader.
    fn write_and_open() -> (tempfile::TempDir, TrigramIndexReader) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        let builder = build_appendix_a();
        TrigramIndexWriter::write(&builder, &path).unwrap();
        let reader = TrigramIndexReader::open(&path).unwrap();
        (dir, reader)
    }

    // ---- intersect_file_ids tests ----

    #[test]
    fn test_intersect_overlapping() {
        let lists = vec![
            vec![FileId(0), FileId(1), FileId(2)],
            vec![FileId(1), FileId(2), FileId(3)],
            vec![FileId(2), FileId(3), FileId(4)],
        ];
        let result = intersect_file_ids(&lists);
        assert_eq!(result, vec![FileId(2)]);
    }

    #[test]
    fn test_intersect_no_overlap() {
        let lists = vec![vec![FileId(0), FileId(1)], vec![FileId(2), FileId(3)]];
        let result = intersect_file_ids(&lists);
        assert!(result.is_empty());
    }

    #[test]
    fn test_intersect_single_list() {
        let lists = vec![vec![FileId(5), FileId(10), FileId(15)]];
        let result = intersect_file_ids(&lists);
        assert_eq!(result, vec![FileId(5), FileId(10), FileId(15)]);
    }

    #[test]
    fn test_intersect_empty_input() {
        let lists: Vec<Vec<FileId>> = vec![];
        let result = intersect_file_ids(&lists);
        assert!(result.is_empty());
    }

    #[test]
    fn test_intersect_one_empty_list() {
        let lists = vec![vec![FileId(0), FileId(1)], vec![]];
        let result = intersect_file_ids(&lists);
        assert!(result.is_empty());
    }

    #[test]
    fn test_intersect_identical_lists() {
        let lists = vec![
            vec![FileId(1), FileId(2), FileId(3)],
            vec![FileId(1), FileId(2), FileId(3)],
        ];
        let result = intersect_file_ids(&lists);
        assert_eq!(result, vec![FileId(1), FileId(2), FileId(3)]);
    }

    #[test]
    fn test_intersect_starts_with_smallest() {
        // The smallest list has only FileId(2), so the result should be
        // at most {FileId(2)}, and since 2 is in all lists, it's [FileId(2)].
        let lists = vec![
            vec![FileId(0), FileId(1), FileId(2), FileId(3), FileId(4)],
            vec![FileId(2)],
            vec![FileId(1), FileId(2), FileId(3)],
        ];
        let result = intersect_file_ids(&lists);
        assert_eq!(result, vec![FileId(2)]);
    }

    // ---- find_candidates tests ----

    #[test]
    fn test_find_candidates_parse() {
        let (_dir, reader) = write_and_open();

        // "parse" has trigrams: "par", "ars", "rse" — all only in file 1
        let candidates = find_candidates(&reader, "parse").unwrap();
        assert_eq!(candidates, vec![FileId(1)]);
    }

    #[test]
    fn test_find_candidates_main() {
        let (_dir, reader) = write_and_open();

        // "main" has trigrams: "mai", "ain" — both only in file 0
        let candidates = find_candidates(&reader, "main").unwrap();
        assert_eq!(candidates, vec![FileId(0)]);
    }

    #[test]
    fn test_find_candidates_fn_too_short() {
        let (_dir, reader) = write_and_open();

        // "fn" is only 2 chars — cannot extract trigrams
        let candidates = find_candidates(&reader, "fn").unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_find_candidates_single_char() {
        let (_dir, reader) = write_and_open();

        let candidates = find_candidates(&reader, "a").unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_find_candidates_empty_query() {
        let (_dir, reader) = write_and_open();

        let candidates = find_candidates(&reader, "").unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_find_candidates_shared_trigram() {
        let (_dir, reader) = write_and_open();

        // "fn " has exactly one trigram "fn " which is in both files
        let candidates = find_candidates(&reader, "fn ").unwrap();
        assert_eq!(candidates, vec![FileId(0), FileId(1)]);
    }

    #[test]
    fn test_find_candidates_not_found() {
        let (_dir, reader) = write_and_open();

        // "xyz" has trigram "xyz" which is not in the index
        let candidates = find_candidates(&reader, "xyz").unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_find_candidates_shared_substring() {
        let (_dir, reader) = write_and_open();

        // "() {}" has trigrams: "() ", ") {", " {}" — all shared by both files
        let candidates = find_candidates(&reader, "() {}").unwrap();
        assert_eq!(candidates, vec![FileId(0), FileId(1)]);
    }

    #[test]
    fn test_find_candidates_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");

        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(0), b"fn main() {}");
        builder.add_file(FileId(1), b"fn parse() {}");
        builder.finalize();
        TrigramIndexWriter::write(&builder, &path).unwrap();
        let reader = TrigramIndexReader::open(&path).unwrap();

        let candidates = find_candidates(&reader, "MAIN").unwrap();
        assert_eq!(candidates, vec![FileId(0)]);
    }

    #[test]
    fn test_find_candidates_mixed_case_query() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");

        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(0), b"fn main() {}");
        builder.add_file(FileId(1), b"fn parse() {}");
        builder.finalize();
        TrigramIndexWriter::write(&builder, &path).unwrap();
        let reader = TrigramIndexReader::open(&path).unwrap();

        let candidates = find_candidates(&reader, "Parse").unwrap();
        assert_eq!(candidates, vec![FileId(1)]);
    }
}
