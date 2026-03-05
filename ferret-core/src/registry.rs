//! Repo registry configuration for multi-repo support.
//!
//! The registry is a TOML file (default `~/.config/ferret/repos.toml`) that
//! lists known repositories so the CLI and web server can find them by name.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{IndexError, Result};

/// Top-level configuration containing a list of registered repositories.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoConfig {
    /// The list of registered repositories.
    #[serde(default)]
    pub repo: Vec<RepoEntry>,
}

/// A single registered repository entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoEntry {
    /// Optional explicit name. If absent, derived from the last component of `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Absolute path to the repository root.
    pub path: PathBuf,
}

impl RepoEntry {
    /// Returns the explicit name if set, otherwise derives one from the last
    /// component of `path`.
    pub fn effective_name(&self) -> &str {
        if let Some(ref name) = self.name {
            return name.as_str();
        }
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
    }
}

impl RepoConfig {
    /// Finds a repo entry whose effective name matches `name`.
    pub fn find_by_name(&self, name: &str) -> Option<&RepoEntry> {
        self.repo.iter().find(|e| e.effective_name() == name)
    }

    /// Finds a repo entry whose path matches `path`.
    pub fn find_by_path(&self, path: &Path) -> Option<&RepoEntry> {
        self.repo.iter().find(|e| e.path == path)
    }
}

/// Returns the default config file path: `~/.config/ferret/repos.toml`.
pub fn config_file_path() -> PathBuf {
    let mut path = config_dir().join("ferret");
    path.push("repos.toml");
    path
}

/// Returns the user config directory (`$XDG_CONFIG_HOME` or `~/.config`).
fn config_dir() -> PathBuf {
    // Use $XDG_CONFIG_HOME if set, otherwise ~/.config
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config")
}

/// Loads a [`RepoConfig`] from the given file path. Returns a default (empty)
/// config if the file does not exist.
pub fn load_config_from(path: &Path) -> Result<RepoConfig> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let config: RepoConfig =
                toml::from_str(&contents).map_err(|e| IndexError::Config(e.to_string()))?;
            Ok(config)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RepoConfig::default()),
        Err(e) => Err(e.into()),
    }
}

/// Loads a [`RepoConfig`] from the default config file path.
pub fn load_config() -> Result<RepoConfig> {
    load_config_from(&config_file_path())
}

/// Saves a [`RepoConfig`] to the given file path, creating parent directories
/// as needed.
pub fn save_config_to(path: &Path, config: &RepoConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(config).map_err(|e| IndexError::Config(e.to_string()))?;
    // Atomic write: write to temp file, then rename into place (crash-safe).
    let tmp_path = path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, contents)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Saves a [`RepoConfig`] to the default config file path.
pub fn save_config(config: &RepoConfig) -> Result<()> {
    save_config_to(&config_file_path(), config)
}

/// Adds a repository to the config. Returns `false` if the path is already
/// registered or if the effective name would collide with an existing entry.
///
/// `name_override` sets an explicit name; if `None`, the name is derived from
/// the last path component.
pub fn add_repo(config: &mut RepoConfig, path: PathBuf, name_override: Option<String>) -> bool {
    // Reject duplicate path.
    if config.find_by_path(&path).is_some() {
        return false;
    }

    let entry = RepoEntry {
        name: name_override,
        path,
    };

    // Reject name collision.
    let eff = entry.effective_name();
    if config.repo.iter().any(|e| e.effective_name() == eff) {
        return false;
    }

    config.repo.push(entry);
    true
}

/// Removes a repository by effective name. Returns `true` if an entry was
/// removed.
pub fn remove_repo(config: &mut RepoConfig, name: &str) -> bool {
    let before = config.repo.len();
    config.repo.retain(|e| e.effective_name() != name);
    config.repo.len() < before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_config() {
        let config: RepoConfig = toml::from_str("").unwrap();
        assert!(config.repo.is_empty());
    }

    #[test]
    fn test_parse_config_with_repos() {
        let toml_str = r#"
[[repo]]
name = "myproject"
path = "/home/user/myproject"

[[repo]]
name = "other"
path = "/home/user/other"
"#;
        let config: RepoConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.repo.len(), 2);
        assert_eq!(config.repo[0].name.as_deref(), Some("myproject"));
        assert_eq!(config.repo[0].path, PathBuf::from("/home/user/myproject"));
        assert_eq!(config.repo[1].name.as_deref(), Some("other"));
        assert_eq!(config.repo[1].path, PathBuf::from("/home/user/other"));
    }

    #[test]
    fn test_name_is_optional_derived_from_path() {
        let toml_str = r#"
[[repo]]
path = "/home/user/myproject"
"#;
        let config: RepoConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.repo.len(), 1);
        assert!(config.repo[0].name.is_none());
        assert_eq!(config.repo[0].effective_name(), "myproject");
    }

    #[test]
    fn test_effective_name_explicit() {
        let entry = RepoEntry {
            name: Some("custom".to_string()),
            path: PathBuf::from("/home/user/myproject"),
        };
        assert_eq!(entry.effective_name(), "custom");
    }

    #[test]
    fn test_effective_name_derived() {
        let entry = RepoEntry {
            name: None,
            path: PathBuf::from("/home/user/myproject"),
        };
        assert_eq!(entry.effective_name(), "myproject");
    }

    #[test]
    fn test_find_by_name_found() {
        let config = RepoConfig {
            repo: vec![RepoEntry {
                name: Some("myproject".to_string()),
                path: PathBuf::from("/home/user/myproject"),
            }],
        };
        let found = config.find_by_name("myproject");
        assert!(found.is_some());
        assert_eq!(found.unwrap().path, PathBuf::from("/home/user/myproject"));
    }

    #[test]
    fn test_find_by_name_not_found() {
        let config = RepoConfig {
            repo: vec![RepoEntry {
                name: Some("myproject".to_string()),
                path: PathBuf::from("/home/user/myproject"),
            }],
        };
        assert!(config.find_by_name("other").is_none());
    }

    #[test]
    fn test_find_by_name_derived() {
        let config = RepoConfig {
            repo: vec![RepoEntry {
                name: None,
                path: PathBuf::from("/home/user/coolrepo"),
            }],
        };
        let found = config.find_by_name("coolrepo");
        assert!(found.is_some());
        assert_eq!(found.unwrap().path, PathBuf::from("/home/user/coolrepo"));
    }

    #[test]
    fn test_find_by_path_found() {
        let config = RepoConfig {
            repo: vec![RepoEntry {
                name: None,
                path: PathBuf::from("/home/user/myproject"),
            }],
        };
        let found = config.find_by_path(Path::new("/home/user/myproject"));
        assert!(found.is_some());
    }

    #[test]
    fn test_find_by_path_not_found() {
        let config = RepoConfig {
            repo: vec![RepoEntry {
                name: None,
                path: PathBuf::from("/home/user/myproject"),
            }],
        };
        assert!(config.find_by_path(Path::new("/other")).is_none());
    }

    #[test]
    fn test_load_nonexistent_config_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.toml");
        let config = load_config_from(&path).unwrap();
        assert!(config.repo.is_empty());
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("repos.toml");

        let mut config = RepoConfig::default();
        config.repo.push(RepoEntry {
            name: Some("alpha".to_string()),
            path: PathBuf::from("/repos/alpha"),
        });
        config.repo.push(RepoEntry {
            name: None,
            path: PathBuf::from("/repos/beta"),
        });

        save_config_to(&path, &config).unwrap();
        let loaded = load_config_from(&path).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn test_add_repo_idempotent_by_path() {
        let mut config = RepoConfig::default();
        let added1 = add_repo(&mut config, PathBuf::from("/repos/alpha"), None);
        assert!(added1);
        assert_eq!(config.repo.len(), 1);

        // Same path again -> false
        let added2 = add_repo(&mut config, PathBuf::from("/repos/alpha"), None);
        assert!(!added2);
        assert_eq!(config.repo.len(), 1);
    }

    #[test]
    fn test_add_repo_name_collision_without_override() {
        let mut config = RepoConfig::default();
        // Add /a/myrepo (derived name = "myrepo")
        assert!(add_repo(&mut config, PathBuf::from("/a/myrepo"), None));
        // Add /b/myrepo (derived name = "myrepo") -> collision
        assert!(!add_repo(&mut config, PathBuf::from("/b/myrepo"), None));
        assert_eq!(config.repo.len(), 1);
    }

    #[test]
    fn test_add_repo_name_collision_with_explicit_override() {
        let mut config = RepoConfig::default();
        // Add /a/myrepo (derived name = "myrepo")
        assert!(add_repo(&mut config, PathBuf::from("/a/myrepo"), None));
        // Add /b/myrepo with explicit name "myrepo-fork" -> succeeds
        assert!(add_repo(
            &mut config,
            PathBuf::from("/b/myrepo"),
            Some("myrepo-fork".to_string())
        ));
        assert_eq!(config.repo.len(), 2);
        assert_eq!(config.repo[1].effective_name(), "myrepo-fork");
    }

    #[test]
    fn test_remove_repo_found() {
        let mut config = RepoConfig::default();
        add_repo(&mut config, PathBuf::from("/repos/alpha"), None);
        add_repo(&mut config, PathBuf::from("/repos/beta"), None);
        assert_eq!(config.repo.len(), 2);

        assert!(remove_repo(&mut config, "alpha"));
        assert_eq!(config.repo.len(), 1);
        assert_eq!(config.repo[0].effective_name(), "beta");
    }

    #[test]
    fn test_remove_repo_not_found() {
        let mut config = RepoConfig::default();
        add_repo(&mut config, PathBuf::from("/repos/alpha"), None);
        assert!(!remove_repo(&mut config, "nonexistent"));
        assert_eq!(config.repo.len(), 1);
    }

    #[test]
    fn test_config_file_path_does_not_panic() {
        let path = config_file_path();
        assert!(path.ends_with("repos.toml"));
        assert!(path.to_str().unwrap().contains("ferret"));
    }
}
