use std::path::{Path, PathBuf};

use ferret_indexer_core::IndexError;
use ferret_indexer_core::SegmentManager;
use ferret_indexer_core::registry::{config_file_path, load_config_from};

/// Resolve a `--repo` value to a repo root path.
///
/// Resolution order:
/// 1. If a value is given, look it up as a repo name in `~/.config/ferret/repos.toml`.
/// 2. If not found as a name, treat it as a filesystem path (existing behavior).
/// 3. If no `--repo` passed, infer from CWD (existing behavior).
pub fn find_repo_root(repo_arg: Option<&str>) -> Result<PathBuf, IndexError> {
    resolve_repo_with_config(repo_arg, &config_file_path())
}

/// Testable version that accepts a config path.
pub fn resolve_repo_with_config(
    repo_arg: Option<&str>,
    config_path: &Path,
) -> Result<PathBuf, IndexError> {
    match repo_arg {
        Some(value) => {
            // Try name lookup first.
            if let Ok(config) = load_config_from(config_path)
                && let Some(entry) = config.find_by_name(value)
            {
                return Ok(entry.path.clone());
            }
            // Fall back to path.
            let path = expand_tilde(Path::new(value));
            let canonical = std::fs::canonicalize(&path).map_err(IndexError::Io)?;
            if canonical.join(".ferret_index").is_dir() || canonical.join(".git").exists() {
                Ok(canonical)
            } else {
                Err(IndexError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "\"{}\" is not a registered repo name or a valid repo path",
                        value
                    ),
                )))
            }
        }
        None => {
            let cwd = std::env::current_dir().map_err(IndexError::Io)?;
            find_repo_root_from(&cwd)
        }
    }
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

/// Walk up from `start` looking for `.ferret_index/` or `.git/`.
fn find_repo_root_from(start: &Path) -> Result<PathBuf, IndexError> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".ferret_index").is_dir() || dir.join(".git").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(IndexError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not inside a git repository or ferret project",
            )));
        }
    }
}

/// Load a SegmentManager from the `.ferret_index/` directory inside repo_root.
///
/// Creates `.ferret_index/segments/` if it doesn't exist.
pub fn load_index(repo_root: &Path) -> Result<SegmentManager, IndexError> {
    let ferret_dir = repo_root.join(".ferret_index");
    SegmentManager::new(&ferret_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_find_repo_root_explicit() {
        let dir = tempfile::tempdir().unwrap();
        // The path must look like a valid repo (has .git or .ferret_index).
        fs::create_dir(dir.path().join(".git")).unwrap();
        let path_str = dir.path().to_string_lossy().to_string();
        let result = find_repo_root(Some(&path_str));
        assert!(result.is_ok());
        // canonicalize both sides — on macOS /var is a symlink to /private/var
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn test_find_repo_root_with_ferret_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ferret_dir = dir.path().join(".ferret_index");
        fs::create_dir(&ferret_dir).unwrap();

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
    fn test_find_repo_root_prefers_ferret_index_over_git() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::create_dir(dir.path().join(".ferret_index")).unwrap();

        let result = find_repo_root_from(dir.path());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path());
    }

    #[test]
    fn test_find_repo_root_not_found() {
        let dir = tempfile::tempdir().unwrap();
        // No .git or .ferret_index anywhere
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

    #[test]
    fn test_resolve_repo_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("myrepo");
        std::fs::create_dir_all(repo_path.join(".ferret_index")).unwrap();

        let config_path = dir.path().join("repos.toml");
        let config = ferret_indexer_core::registry::RepoConfig {
            repo: vec![ferret_indexer_core::registry::RepoEntry {
                name: Some("myrepo".to_string()),
                path: repo_path.clone(),
            }],
        };
        ferret_indexer_core::registry::save_config_to(&config_path, &config).unwrap();

        let result = resolve_repo_with_config(Some("myrepo"), &config_path);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), repo_path);
    }

    #[test]
    fn test_resolve_repo_by_path_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".ferret_index")).unwrap();

        let config_path = dir.path().join("nonexistent_repos.toml");
        let path_str = dir.path().to_string_lossy().to_string();

        let result = resolve_repo_with_config(Some(&path_str), &config_path);
        assert!(result.is_ok());
        let expected = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(result.unwrap(), expected);
    }

    #[test]
    fn test_resolve_repo_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("repos.toml");
        let config = ferret_indexer_core::registry::RepoConfig { repo: vec![] };
        ferret_indexer_core::registry::save_config_to(&config_path, &config).unwrap();

        let result = resolve_repo_with_config(Some("nosuch"), &config_path);
        assert!(result.is_err());
    }
}
