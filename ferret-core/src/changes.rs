//! Shared change-event types for file-system and git-based change detection.
//!
//! Both the file-system watcher and the git-diff detector produce
//! [`ChangeEvent`] values, allowing downstream code to handle changes
//! uniformly regardless of the detection source.

use std::path::PathBuf;

/// The kind of change detected for a file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// A new file was created (or is untracked).
    Created,
    /// An existing file was modified.
    Modified,
    /// A file was deleted.
    Deleted,
    /// A file was renamed.
    Renamed,
}

/// A single file-change event.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeEvent {
    /// Path to the changed file, relative to the repository root.
    pub path: PathBuf,
    /// What kind of change occurred.
    pub kind: ChangeKind,
}
