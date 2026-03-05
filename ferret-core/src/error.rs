//! Central error type for the ferret indexing system.
//!
//! All fallible operations in ferret-indexer-core return [`IndexError`] via the
//! convenience type alias [`Result<T>`].

use crate::types::SegmentId;

/// Central error type for all ferret operations.
///
/// Uses `thiserror` for ergonomic error definition with automatic `Display`
/// and `From` implementations.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// An I/O error occurred during file or index operations.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The index data is corrupted or internally inconsistent.
    #[error("index corruption: {0}")]
    IndexCorruption(String),

    /// The query string could not be parsed.
    #[error("query parse error: {0}")]
    QueryParse(String),

    /// The on-disk index format version is not supported by this binary.
    #[error("unsupported format version: {version}")]
    UnsupportedVersion {
        /// The format version number found on disk.
        version: u32,
    },

    /// The requested segment does not exist in the index.
    #[error("segment not found: {0}")]
    SegmentNotFound(SegmentId),

    /// An error occurred while walking the directory tree.
    #[error("walk error: {0}")]
    Walk(String),

    /// A git command failed or the directory is not a git repository.
    #[error("git error: {0}")]
    Git(String),

    /// An error occurred in the filesystem watcher.
    #[error("watcher error: {0}")]
    Watcher(String),

    /// An error occurred parsing or writing a configuration file.
    #[error("config error: {0}")]
    Config(String),
}

/// Convenience type alias for `std::result::Result<T, IndexError>`.
pub type Result<T> = std::result::Result<T, IndexError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_io_error_from() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let index_err: IndexError = io_err.into();
        match &index_err {
            IndexError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected Io variant, got: {other}"),
        }
        assert!(index_err.to_string().contains("file not found"));
    }

    #[test]
    fn test_io_error_from_function_signature() {
        // Verify that the From conversion works in a function returning Result
        fn read_something() -> Result<()> {
            let _bytes = std::fs::read("/nonexistent/path/that/should/not/exist")?;
            Ok(())
        }
        let err = read_something().unwrap_err();
        assert!(matches!(err, IndexError::Io(_)));
    }

    #[test]
    fn test_index_corruption_display() {
        let err = IndexError::IndexCorruption("invalid trigram table header".to_string());
        assert_eq!(
            err.to_string(),
            "index corruption: invalid trigram table header"
        );
    }

    #[test]
    fn test_query_parse_display() {
        let err = IndexError::QueryParse("unexpected token at position 5".to_string());
        assert_eq!(
            err.to_string(),
            "query parse error: unexpected token at position 5"
        );
    }

    #[test]
    fn test_unsupported_version_display() {
        let err = IndexError::UnsupportedVersion { version: 99 };
        assert_eq!(err.to_string(), "unsupported format version: 99");
    }

    #[test]
    fn test_segment_not_found_display() {
        let err = IndexError::SegmentNotFound(SegmentId(42));
        assert_eq!(err.to_string(), "segment not found: 42");
    }

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<IndexError>();
    }
}
