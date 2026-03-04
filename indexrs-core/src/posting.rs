//! In-memory posting lists builder for trigram indexing.
//!
//! This module provides [`PostingListBuilder`], which accumulates trigram posting
//! lists during the index-build phase. It maintains two kinds of posting lists:
//!
//! - **File-level postings**: For each trigram, which file IDs contain it. Used for
//!   fast candidate filtering during search.
//! - **Positional postings**: For each trigram, the (file_id, byte_offset) pairs
//!   where it appears. Used for multi-trigram proximity verification.
//!
//! After all files have been added, call [`PostingListBuilder::finalize`] to sort
//! and deduplicate the posting lists in preparation for serialization or querying.

use std::collections::HashMap;

use crate::trigram::extract_trigrams_folded;
use crate::types::{FileId, Trigram};

/// Accumulates trigram posting lists during index building.
///
/// # Usage
///
/// ```
/// use indexrs_core::posting::PostingListBuilder;
/// use indexrs_core::types::FileId;
///
/// let mut builder = PostingListBuilder::new();
/// builder.add_file(FileId(0), b"fn main() {}");
/// builder.add_file(FileId(1), b"fn parse() {}");
/// builder.finalize();
///
/// assert!(builder.trigram_count() > 0);
/// ```
pub struct PostingListBuilder {
    /// Trigram -> list of file IDs that contain this trigram.
    file_postings: HashMap<Trigram, Vec<FileId>>,
    /// Trigram -> list of (file_id, byte_offset) where this trigram appears.
    positional_postings: HashMap<Trigram, Vec<(FileId, u32)>>,
    /// Whether to accumulate positional postings.
    store_positions: bool,
}

impl PostingListBuilder {
    /// Create a new empty posting list builder with positional postings enabled.
    pub fn new() -> Self {
        PostingListBuilder {
            file_postings: HashMap::new(),
            positional_postings: HashMap::new(),
            store_positions: true,
        }
    }

    /// Create a posting list builder that skips positional postings.
    ///
    /// Only file-level posting lists are accumulated, dramatically reducing
    /// memory usage and index size. Use this when byte-offset positions are
    /// not needed (the common case for search).
    pub fn file_only() -> Self {
        PostingListBuilder {
            file_postings: HashMap::new(),
            positional_postings: HashMap::new(),
            store_positions: false,
        }
    }

    /// Create a file-only posting list builder with pre-allocated capacity.
    ///
    /// `estimated_trigrams` sets the initial capacity of the HashMap to avoid
    /// rehashing. A typical 256 MB segment has 30K–60K distinct trigrams;
    /// 65536 is a reasonable default.
    pub fn file_only_with_capacity(estimated_trigrams: usize) -> Self {
        PostingListBuilder {
            file_postings: HashMap::with_capacity(estimated_trigrams),
            positional_postings: HashMap::new(),
            store_positions: false,
        }
    }

    /// Whether this builder accumulates positional postings.
    pub fn stores_positions(&self) -> bool {
        self.store_positions
    }

    /// Add a file's content to the index.
    ///
    /// Extracts all trigrams from `content` and records:
    /// - The `file_id` in the file-level posting list for each trigram
    /// - The `(file_id, byte_offset)` pair in the positional posting list
    ///
    /// File-level postings may contain duplicate file IDs after multiple calls;
    /// call [`finalize`](Self::finalize) to deduplicate and sort.
    pub fn add_file(&mut self, file_id: FileId, content: &[u8]) {
        if self.store_positions {
            for (offset, trigram) in extract_trigrams_folded(content).enumerate() {
                debug_assert!(
                    offset <= u32::MAX as usize,
                    "file content too large for u32 offset: {offset}"
                );
                self.file_postings.entry(trigram).or_default().push(file_id);
                self.positional_postings
                    .entry(trigram)
                    .or_default()
                    .push((file_id, offset as u32));
            }
        } else {
            for trigram in extract_trigrams_folded(content) {
                self.file_postings.entry(trigram).or_default().push(file_id);
            }
        }
    }

    /// Sort and deduplicate all posting lists.
    ///
    /// After finalization:
    /// - File posting lists are sorted by file ID (ascending) with duplicates removed.
    /// - Positional posting lists are sorted by (file_id, offset) ascending.
    ///
    /// This must be called before the posting lists are serialized or queried.
    pub fn finalize(&mut self) {
        if self.store_positions {
            for file_ids in self.file_postings.values_mut() {
                file_ids.sort();
                file_ids.dedup();
            }
            for positions in self.positional_postings.values_mut() {
                positions.sort();
            }
        } else {
            // In file-only mode, file IDs are pushed in ascending order
            // (FileId(0), FileId(1), ...), so sort is unnecessary — just dedup.
            for file_ids in self.file_postings.values_mut() {
                file_ids.dedup();
            }
        }
    }

    /// Returns a reference to the file-level posting lists.
    ///
    /// Each entry maps a trigram to the sorted, deduplicated list of file IDs
    /// containing that trigram (after [`finalize`](Self::finalize) has been called).
    pub fn file_postings(&self) -> &HashMap<Trigram, Vec<FileId>> {
        &self.file_postings
    }

    /// Returns a reference to the positional posting lists.
    ///
    /// Each entry maps a trigram to the sorted list of (file_id, byte_offset)
    /// pairs where that trigram appears (after [`finalize`](Self::finalize) has been called).
    pub fn positional_postings(&self) -> &HashMap<Trigram, Vec<(FileId, u32)>> {
        &self.positional_postings
    }

    /// Returns the number of distinct trigrams in the index.
    pub fn trigram_count(&self) -> usize {
        self.file_postings.len()
    }
}

impl Default for PostingListBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_posting_builder_empty() {
        let builder = PostingListBuilder::new();
        assert_eq!(builder.trigram_count(), 0);
        assert!(builder.file_postings().is_empty());
        assert!(builder.positional_postings().is_empty());
    }

    #[test]
    fn test_posting_builder_single_file() {
        // "fn main() {}" has 10 unique trigrams (all distinct, from Appendix A)
        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(0), b"fn main() {}");
        assert_eq!(builder.trigram_count(), 10);
    }

    #[test]
    fn test_posting_builder_appendix_a_file_postings() {
        // Appendix A: File 0 = "fn main() {}", File 1 = "fn parse() {}"
        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(0), b"fn main() {}");
        builder.add_file(FileId(1), b"fn parse() {}");
        builder.finalize();

        let fp = builder.file_postings();

        // Shared trigrams: "fn ", "() ", ") {", " {}" -> [0, 1]
        assert_eq!(
            fp[&Trigram::from_bytes(b'f', b'n', b' ')],
            vec![FileId(0), FileId(1)]
        );
        assert_eq!(
            fp[&Trigram::from_bytes(b'(', b')', b' ')],
            vec![FileId(0), FileId(1)]
        );
        assert_eq!(
            fp[&Trigram::from_bytes(b')', b' ', b'{')],
            vec![FileId(0), FileId(1)]
        );
        assert_eq!(
            fp[&Trigram::from_bytes(b' ', b'{', b'}')],
            vec![FileId(0), FileId(1)]
        );

        // File 0 only trigrams: "n m", " ma", "mai", "ain", "in(", "n()"
        assert_eq!(fp[&Trigram::from_bytes(b'n', b' ', b'm')], vec![FileId(0)]);
        assert_eq!(fp[&Trigram::from_bytes(b' ', b'm', b'a')], vec![FileId(0)]);
        assert_eq!(fp[&Trigram::from_bytes(b'm', b'a', b'i')], vec![FileId(0)]);
        assert_eq!(fp[&Trigram::from_bytes(b'a', b'i', b'n')], vec![FileId(0)]);
        assert_eq!(fp[&Trigram::from_bytes(b'i', b'n', b'(')], vec![FileId(0)]);
        assert_eq!(fp[&Trigram::from_bytes(b'n', b'(', b')')], vec![FileId(0)]);

        // File 1 only trigrams: "n p", " pa", "par", "ars", "rse", "se(", "e()"
        assert_eq!(fp[&Trigram::from_bytes(b'n', b' ', b'p')], vec![FileId(1)]);
        assert_eq!(fp[&Trigram::from_bytes(b' ', b'p', b'a')], vec![FileId(1)]);
        assert_eq!(fp[&Trigram::from_bytes(b'p', b'a', b'r')], vec![FileId(1)]);
        assert_eq!(fp[&Trigram::from_bytes(b'a', b'r', b's')], vec![FileId(1)]);
        assert_eq!(fp[&Trigram::from_bytes(b'r', b's', b'e')], vec![FileId(1)]);
        assert_eq!(fp[&Trigram::from_bytes(b's', b'e', b'(')], vec![FileId(1)]);
        assert_eq!(fp[&Trigram::from_bytes(b'e', b'(', b')')], vec![FileId(1)]);
    }

    #[test]
    fn test_posting_builder_appendix_a_positional_postings() {
        // Verify positional posting lists match Appendix A exactly
        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(0), b"fn main() {}");
        builder.add_file(FileId(1), b"fn parse() {}");
        builder.finalize();

        let pp = builder.positional_postings();

        // "fn " -> [(0, 0), (1, 0)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'f', b'n', b' ')],
            vec![(FileId(0), 0), (FileId(1), 0)]
        );

        // "n m" -> [(0, 1)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'n', b' ', b'm')],
            vec![(FileId(0), 1)]
        );

        // " ma" -> [(0, 2)]
        assert_eq!(
            pp[&Trigram::from_bytes(b' ', b'm', b'a')],
            vec![(FileId(0), 2)]
        );

        // "mai" -> [(0, 3)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'm', b'a', b'i')],
            vec![(FileId(0), 3)]
        );

        // "ain" -> [(0, 4)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'a', b'i', b'n')],
            vec![(FileId(0), 4)]
        );

        // "in(" -> [(0, 5)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'i', b'n', b'(')],
            vec![(FileId(0), 5)]
        );

        // "n()" -> [(0, 6)] — only in file 0 ("fn main() {}")
        // Note: "fn parse() {}" does NOT contain "n()" — the 'n' at pos 1 and
        // '(' at pos 8 are not adjacent.
        assert_eq!(
            pp[&Trigram::from_bytes(b'n', b'(', b')')],
            vec![(FileId(0), 6)]
        );

        // "() " -> [(0, 7), (1, 8)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'(', b')', b' ')],
            vec![(FileId(0), 7), (FileId(1), 8)]
        );

        // ") {" -> [(0, 8), (1, 9)]
        assert_eq!(
            pp[&Trigram::from_bytes(b')', b' ', b'{')],
            vec![(FileId(0), 8), (FileId(1), 9)]
        );

        // " {}" -> [(0, 9), (1, 10)]
        assert_eq!(
            pp[&Trigram::from_bytes(b' ', b'{', b'}')],
            vec![(FileId(0), 9), (FileId(1), 10)]
        );

        // "n p" -> [(1, 1)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'n', b' ', b'p')],
            vec![(FileId(1), 1)]
        );

        // " pa" -> [(1, 2)]
        assert_eq!(
            pp[&Trigram::from_bytes(b' ', b'p', b'a')],
            vec![(FileId(1), 2)]
        );

        // "par" -> [(1, 3)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'p', b'a', b'r')],
            vec![(FileId(1), 3)]
        );

        // "ars" -> [(1, 4)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'a', b'r', b's')],
            vec![(FileId(1), 4)]
        );

        // "rse" -> [(1, 5)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'r', b's', b'e')],
            vec![(FileId(1), 5)]
        );

        // "se(" -> [(1, 6)]
        assert_eq!(
            pp[&Trigram::from_bytes(b's', b'e', b'(')],
            vec![(FileId(1), 6)]
        );

        // "e()" -> [(1, 7)]
        assert_eq!(
            pp[&Trigram::from_bytes(b'e', b'(', b')')],
            vec![(FileId(1), 7)]
        );
    }

    #[test]
    fn test_posting_builder_appendix_a_trigram_count() {
        // Appendix A shows 17 distinct trigrams across both files
        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(0), b"fn main() {}");
        builder.add_file(FileId(1), b"fn parse() {}");

        assert_eq!(builder.trigram_count(), 17);
    }

    #[test]
    fn test_posting_builder_finalize_sorts() {
        // Add files in reverse ID order to verify finalize sorts correctly
        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(5), b"fn parse() {}");
        builder.add_file(FileId(2), b"fn main() {}");
        builder.finalize();

        let fp = builder.file_postings();

        // "fn " should have [2, 5] (sorted), not [5, 2]
        assert_eq!(
            fp[&Trigram::from_bytes(b'f', b'n', b' ')],
            vec![FileId(2), FileId(5)]
        );

        // Positional postings should also be sorted by file_id then offset
        let pp = builder.positional_postings();
        let fn_space = &pp[&Trigram::from_bytes(b'f', b'n', b' ')];
        assert_eq!(fn_space, &vec![(FileId(2), 0), (FileId(5), 0)]);
    }

    #[test]
    fn test_posting_builder_file_dedup() {
        // Adding the same file_id content twice should result in deduplicated
        // file postings after finalize (but positional postings are kept as-is
        // since they record distinct occurrences)
        let mut builder = PostingListBuilder::new();
        builder.add_file(FileId(0), b"abc");
        builder.add_file(FileId(0), b"abc");
        builder.finalize();

        let fp = builder.file_postings();
        // File-level posting should be deduplicated
        assert_eq!(fp[&Trigram::from_bytes(b'a', b'b', b'c')], vec![FileId(0)]);

        let pp = builder.positional_postings();
        // Positional postings record both occurrences (both at offset 0)
        assert_eq!(
            pp[&Trigram::from_bytes(b'a', b'b', b'c')],
            vec![(FileId(0), 0), (FileId(0), 0)]
        );
    }

    #[test]
    fn test_posting_builder_default() {
        // Default trait should work the same as new()
        let builder = PostingListBuilder::default();
        assert_eq!(builder.trigram_count(), 0);
    }

    #[test]
    fn test_file_only_builder_skips_positions() {
        let mut builder = PostingListBuilder::file_only();
        builder.add_file(FileId(0), b"fn main() {}");
        builder.add_file(FileId(1), b"fn parse() {}");
        builder.finalize();

        // File postings populated normally
        assert_eq!(builder.trigram_count(), 17);
        let fp = builder.file_postings();
        assert_eq!(
            fp[&Trigram::from_bytes(b'f', b'n', b' ')],
            vec![FileId(0), FileId(1)]
        );

        // Positional postings empty
        assert!(builder.positional_postings().is_empty());
        assert!(!builder.stores_positions());
    }

    #[test]
    fn test_file_only_stores_positions_flag() {
        assert!(!PostingListBuilder::file_only().stores_positions());
        assert!(PostingListBuilder::new().stores_positions());
    }

    #[test]
    fn test_posting_builder_case_fold_uppercase_content() {
        let mut builder_upper = PostingListBuilder::file_only();
        builder_upper.add_file(FileId(0), b"FN MAIN() {}");
        builder_upper.finalize();

        let mut builder_lower = PostingListBuilder::file_only();
        builder_lower.add_file(FileId(0), b"fn main() {}");
        builder_lower.finalize();

        assert_eq!(builder_upper.trigram_count(), builder_lower.trigram_count());

        let fp_upper = builder_upper.file_postings();
        let fp_lower = builder_lower.file_postings();
        assert!(fp_upper.contains_key(&Trigram::from_bytes(b'f', b'n', b' ')));
        assert!(fp_lower.contains_key(&Trigram::from_bytes(b'f', b'n', b' ')));

        assert!(!fp_upper.contains_key(&Trigram::from_bytes(b'F', b'N', b' ')));
    }

    #[test]
    fn test_file_only_finalize_dedup_without_sort() {
        let mut builder = PostingListBuilder::file_only();
        // Add three files — trigrams from file 0, 1, 2 in order
        builder.add_file(FileId(0), b"fn main() {}");
        builder.add_file(FileId(1), b"fn main() { let x = 1; }");
        builder.add_file(FileId(2), b"fn parse() {}");
        builder.finalize();

        // "fn " trigram should appear in all three files, sorted and deduped
        let trigram_fn = crate::trigram::extract_unique_trigrams_folded(b"fn ");
        for tri in &trigram_fn {
            if let Some(ids) = builder.file_postings().get(tri) {
                // Must be sorted and deduped
                for window in ids.windows(2) {
                    assert!(window[0] <= window[1], "posting list not sorted: {:?}", ids);
                }
                // No duplicates
                let mut deduped = ids.clone();
                deduped.dedup();
                assert_eq!(ids, &deduped, "posting list has duplicates");
            }
        }
    }

    #[test]
    fn test_file_only_with_capacity() {
        let mut builder = PostingListBuilder::file_only_with_capacity(1024);
        assert!(!builder.stores_positions());
        builder.add_file(FileId(0), b"fn main() {}");
        builder.finalize();
        assert!(builder.trigram_count() > 0);
    }

    #[test]
    fn test_posting_builder_case_fold_mixed_case() {
        let mut builder = PostingListBuilder::file_only();
        builder.add_file(FileId(0), b"HttpRequest");
        builder.add_file(FileId(1), b"httprequest");
        builder.finalize();

        let fp = builder.file_postings();

        let htt = &fp[&Trigram::from_bytes(b'h', b't', b't')];
        assert_eq!(htt, &vec![FileId(0), FileId(1)]);

        assert!(!fp.contains_key(&Trigram::from_bytes(b'H', b't', b't')));
    }
}
