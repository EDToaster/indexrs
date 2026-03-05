//! Checkpoint persistence for daemon indexing state.
//!
//! The checkpoint file (`checkpoint.json`) records the last indexed state so the
//! daemon can detect what has changed since the previous run and catch up
//! incrementally. It is stored in the `.ferret_index/` directory alongside segments.
//!
//! Writes use the atomic temp-file-then-rename pattern for crash safety.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{IndexError, Result};

/// File name for the checkpoint inside the `.ferret_index/` directory.
const CHECKPOINT_FILENAME: &str = "checkpoint.json";

/// Persistent record of the last indexed state.
///
/// Written after each successful indexing pass so the daemon can resume from
/// where it left off. Serialized as JSON for human readability and easy
/// debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Schema version for forward compatibility (currently 1).
    pub version: u32,
    /// The git commit SHA that was indexed, if the repository is a git repo.
    pub git_commit: Option<String>,
    /// Unix epoch seconds when the indexing pass completed.
    pub indexed_at_epoch: u64,
    /// Total number of files in the index at the time of the checkpoint.
    pub file_count: u64,
}

impl Checkpoint {
    /// Create a new checkpoint with version 1 and the current timestamp.
    ///
    /// # Arguments
    ///
    /// * `git_commit` — The HEAD commit SHA at indexing time, or `None` if
    ///   the directory is not a git repository.
    /// * `file_count` — Total number of files in the index.
    pub fn new(git_commit: Option<String>, file_count: u64) -> Self {
        let indexed_at_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_secs();
        Self {
            version: 1,
            git_commit,
            indexed_at_epoch,
            file_count,
        }
    }
}

/// Read a checkpoint from `<ferret_dir>/checkpoint.json`.
///
/// Returns `Ok(None)` if the file does not exist. Returns `Err` if the file
/// exists but cannot be read or contains invalid JSON.
///
/// # Errors
///
/// - [`IndexError::Io`] if the file exists but cannot be read.
/// - [`IndexError::IndexCorruption`] if the file contains invalid JSON.
pub fn read_checkpoint(ferret_dir: &Path) -> Result<Option<Checkpoint>> {
    let path = ferret_dir.join(CHECKPOINT_FILENAME);
    match fs::read_to_string(&path) {
        Ok(contents) => {
            let checkpoint: Checkpoint = serde_json::from_str(&contents).map_err(|e| {
                IndexError::IndexCorruption(format!("corrupt checkpoint.json: {e}"))
            })?;
            Ok(Some(checkpoint))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(IndexError::Io(e)),
    }
}

/// Write a checkpoint to `<ferret_dir>/checkpoint.json`.
///
/// Uses atomic temp-file-then-rename for crash safety. The parent directory
/// is created if it does not already exist.
///
/// # Errors
///
/// Returns [`IndexError::Io`] if the directory cannot be created or the file
/// cannot be written.
pub fn write_checkpoint(ferret_dir: &Path, checkpoint: &Checkpoint) -> Result<()> {
    fs::create_dir_all(ferret_dir)?;

    let path = ferret_dir.join(CHECKPOINT_FILENAME);
    let temp_path = ferret_dir.join(format!(".checkpoint.json.tmp.{}", std::process::id()));

    let json = serde_json::to_string_pretty(checkpoint)
        .map_err(|e| IndexError::IndexCorruption(format!("failed to serialize checkpoint: {e}")))?;

    {
        let mut f = fs::File::create(&temp_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&temp_path, path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_new_sets_current_timestamp() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cp = Checkpoint::new(Some("abc123".to_string()), 42);
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert_eq!(cp.version, 1);
        assert_eq!(cp.git_commit, Some("abc123".to_string()));
        assert_eq!(cp.file_count, 42);
        assert!(
            cp.indexed_at_epoch >= before && cp.indexed_at_epoch <= after,
            "timestamp {} not in [{}, {}]",
            cp.indexed_at_epoch,
            before,
            after
        );
    }

    #[test]
    fn test_checkpoint_new_without_git() {
        let cp = Checkpoint::new(None, 0);
        assert_eq!(cp.version, 1);
        assert!(cp.git_commit.is_none());
        assert_eq!(cp.file_count, 0);
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoint::new(Some("deadbeef".to_string()), 100);

        write_checkpoint(dir.path(), &cp).unwrap();
        let loaded = read_checkpoint(dir.path()).unwrap().expect("should exist");

        assert_eq!(loaded.version, cp.version);
        assert_eq!(loaded.git_commit, cp.git_commit);
        assert_eq!(loaded.indexed_at_epoch, cp.indexed_at_epoch);
        assert_eq!(loaded.file_count, cp.file_count);
    }

    #[test]
    fn test_read_checkpoint_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_checkpoint(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_checkpoint_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.json");
        fs::write(&path, "not valid json {{{").unwrap();

        let err = read_checkpoint(dir.path()).unwrap_err();
        match &err {
            IndexError::IndexCorruption(msg) => {
                assert!(
                    msg.contains("corrupt checkpoint.json"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected IndexCorruption, got: {other}"),
        }
    }

    #[test]
    fn test_write_checkpoint_atomic_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoint::new(None, 5);

        // File should not exist yet.
        assert!(!dir.path().join("checkpoint.json").exists());

        write_checkpoint(dir.path(), &cp).unwrap();

        // File should now exist and no temp file should remain.
        assert!(dir.path().join("checkpoint.json").exists());
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "only checkpoint.json should exist");
    }

    #[test]
    fn test_write_checkpoint_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();

        let cp1 = Checkpoint::new(Some("aaa".to_string()), 10);
        write_checkpoint(dir.path(), &cp1).unwrap();

        let cp2 = Checkpoint::new(Some("bbb".to_string()), 20);
        write_checkpoint(dir.path(), &cp2).unwrap();

        let loaded = read_checkpoint(dir.path()).unwrap().expect("should exist");
        assert_eq!(loaded.git_commit, Some("bbb".to_string()));
        assert_eq!(loaded.file_count, 20);
    }
}
