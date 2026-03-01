pub mod binary;
pub mod changes;
pub mod codec;
pub mod content;
pub mod error;
pub mod git_diff;
pub mod hybrid_detector;
pub mod index_reader;
pub mod index_state;
pub mod index_writer;
pub mod intersection;
pub mod metadata;
pub mod multi_search;
pub mod posting;
pub mod query;
pub mod query_plan;
pub mod query_trigrams;
pub mod ranking;
pub mod recovery;
pub mod search;
pub mod segment;
pub mod segment_manager;
pub mod tombstone;
pub mod trigram;
pub mod types;
pub mod verify;
pub mod walker;
pub mod watcher;

pub use binary::{
    DEFAULT_MAX_FILE_SIZE, is_binary_content, is_binary_extension, is_binary_path,
    should_index_file,
};
pub use changes::{ChangeEvent, ChangeKind};
pub use codec::{
    decode_delta_varint, decode_positional_postings, encode_delta_varint,
    encode_positional_postings,
};
pub use content::{ContentStoreReader, ContentStoreWriter};
pub use error::{IndexError, Result};
pub use git_diff::GitChangeDetector;
pub use hybrid_detector::HybridDetector;
pub use index_reader::TrigramIndexReader;
pub use index_state::{IndexState, SegmentList};
pub use index_writer::TrigramIndexWriter;
pub use intersection::{find_candidates, intersect_file_ids};
pub use metadata::{FileMetadata, MetadataBuilder, MetadataReader};
pub use multi_search::{
    search_segments, search_segments_streaming, search_segments_with_options,
    search_segments_with_pattern, search_segments_with_pattern_and_options,
};
pub use posting::PostingListBuilder;
pub use query::{LiteralQuery, PhraseQuery, Query, RegexQuery, parse_query};
pub use query_plan::{
    PreFilter, QueryInput, QueryPlan, ScoredTrigram, VerifyStep, plan_literal_query, plan_query,
    plan_query_multi, plan_regex_query,
};
pub use query_trigrams::{
    TrigramQuery, extract_literal_trigrams, extract_query_trigrams, extract_regex_trigrams,
};
pub use ranking::{MatchType, RankingConfig, ScoringInput, score_file_match};
pub use recovery::{cleanup_lock_file, recover_segments};
pub use search::{
    ContextBlock, ContextLine, FileMatch, LineMatch, MatchPattern, SearchOptions, SearchResult,
};
pub use segment::{InputFile, Segment, SegmentWriter};
pub use segment_manager::SegmentManager;
pub use tombstone::{TombstoneSet, needs_new_entry, needs_tombstone};
pub use trigram::{
    ascii_fold_byte, extract_trigrams, extract_trigrams_folded, extract_unique_trigrams,
    extract_unique_trigrams_folded,
};
pub use types::{FileId, Language, SegmentId, SymbolKind, Trigram};
pub use verify::ContentVerifier;
pub use walker::{DirectoryWalkerBuilder, WalkedFile, Walker};
pub use watcher::FileWatcher;
