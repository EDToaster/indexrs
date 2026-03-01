use std::path::{Path, PathBuf};

use indexrs_core::IndexError;
use indexrs_core::SegmentManager;

/// Find the repository root directory.
///
/// If `repo_arg` is provided, expands `~` and canonicalizes it.
/// Otherwise, walks up from the current directory looking for `.indexrs/` or `.git/`.
pub fn find_repo_root(repo_arg: Option<&Path>) -> Result<PathBuf, IndexError> {
    if let Some(repo) = repo_arg {
        let expanded = expand_tilde(repo);
        return std::fs::canonicalize(&expanded).map_err(IndexError::Io);
    }
    let cwd = std::env::current_dir().map_err(IndexError::Io)?;
    find_repo_root_from(&cwd)
}

/// Expand a leading `~` or `~/` to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Walk up from `start` looking for `.indexrs/` or `.git/`.
fn find_repo_root_from(start: &Path) -> Result<PathBuf, IndexError> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".indexrs").is_dir() || dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(IndexError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not inside a git repository or indexrs project",
            )));
        }
    }
}

/// Load a SegmentManager from the `.indexrs/` directory inside repo_root.
///
/// Creates `.indexrs/segments/` if it doesn't exist.
pub fn load_index(repo_root: &Path) -> Result<SegmentManager, IndexError> {
    let indexrs_dir = repo_root.join(".indexrs");
    SegmentManager::new(&indexrs_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_find_repo_root_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_repo_root(Some(dir.path()));
        assert!(result.is_ok());
        // canonicalize both sides — on macOS /var is a symlink to /private/var
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn test_find_repo_root_with_indexrs_dir() {
        let dir = tempfile::tempdir().unwrap();
        let indexrs_dir = dir.path().join(".indexrs");
        fs::create_dir(&indexrs_dir).unwrap();

        let subdir = dir.path().join("src").join("deep");
        fs::create_dir_all(&subdir).unwrap();

        let result = find_repo_root_from(&subdir);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path());
    }

    #[test]
    fn test_find_repo_root_with_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir(&git_dir).unwrap();

        let subdir = dir.path().join("src");
        fs::create_dir_all(&subdir).unwrap();

        let result = find_repo_root_from(&subdir);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path());
    }

    #[test]
    fn test_find_repo_root_prefers_indexrs_over_git() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::create_dir(dir.path().join(".indexrs")).unwrap();

        let result = find_repo_root_from(dir.path());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path());
    }

    #[test]
    fn test_find_repo_root_not_found() {
        let dir = tempfile::tempdir().unwrap();
        // No .git or .indexrs anywhere
        let result = find_repo_root_from(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_expand_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde(Path::new("~")), PathBuf::from(&home));
        assert_eq!(
            expand_tilde(Path::new("~/foo/bar")),
            PathBuf::from(&home).join("foo/bar")
        );
        // Non-tilde paths are returned unchanged
        assert_eq!(
            expand_tilde(Path::new("/absolute/path")),
            PathBuf::from("/absolute/path")
        );
        assert_eq!(
            expand_tilde(Path::new("relative/path")),
            PathBuf::from("relative/path")
        );
    }
}
