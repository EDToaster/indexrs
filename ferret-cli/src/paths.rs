use std::path::{Path, PathBuf};

pub(crate) struct PathRewriter {
    transform: PathTransform,
}

enum PathTransform {
    Identity,
    RelativeTo { cwd_from_root: PathBuf },
    Absolute { repo_root: PathBuf },
}

impl PathRewriter {
    pub(crate) fn new(repo_root: &Path, cwd: &Path) -> Self {
        // Canonicalize both to handle symlinks (e.g., macOS /var -> /private/var).
        let repo_root = repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.to_path_buf());
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

        if cwd == repo_root {
            return Self::identity();
        }
        match cwd.strip_prefix(&repo_root) {
            Ok(rel) => Self {
                transform: PathTransform::RelativeTo {
                    cwd_from_root: rel.to_path_buf(),
                },
            },
            Err(_) => Self {
                transform: PathTransform::Absolute {
                    repo_root: repo_root.to_path_buf(),
                },
            },
        }
    }

    pub(crate) fn identity() -> Self {
        Self {
            transform: PathTransform::Identity,
        }
    }

    pub(crate) fn rewrite(&self, repo_relative_path: &str) -> String {
        match &self.transform {
            PathTransform::Identity => repo_relative_path.to_string(),
            PathTransform::RelativeTo { cwd_from_root } => {
                diff_relative_paths(Path::new(repo_relative_path), cwd_from_root)
                    .to_string_lossy()
                    .into_owned()
            }
            PathTransform::Absolute { repo_root } => repo_root
                .join(repo_relative_path)
                .to_string_lossy()
                .into_owned(),
        }
    }
}

fn diff_relative_paths(target: &Path, base: &Path) -> PathBuf {
    let mut target_components = target.components().peekable();
    let mut base_components = base.components().peekable();

    // Skip common prefix.
    while let (Some(t), Some(b)) = (target_components.peek(), base_components.peek()) {
        if t != b {
            break;
        }
        target_components.next();
        base_components.next();
    }

    // One `..` per remaining base component.
    let mut result = PathBuf::new();
    for _ in base_components {
        result.push("..");
    }

    // Append remaining target components.
    for component in target_components {
        result.push(component);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- diff_relative_paths --

    #[test]
    fn test_diff_same_dir() {
        let result = diff_relative_paths(Path::new("src/main.rs"), Path::new("src"));
        assert_eq!(result, PathBuf::from("main.rs"));
    }

    #[test]
    fn test_diff_parent_dir() {
        let result = diff_relative_paths(Path::new("README.md"), Path::new("src"));
        assert_eq!(result, PathBuf::from("../README.md"));
    }

    #[test]
    fn test_diff_cross_dir() {
        let result = diff_relative_paths(Path::new("src/core/mod.rs"), Path::new("src/cli"));
        assert_eq!(result, PathBuf::from("../core/mod.rs"));
    }

    #[test]
    fn test_diff_deeply_nested() {
        let result = diff_relative_paths(Path::new("a/b/c.rs"), Path::new("x/y/z"));
        assert_eq!(result, PathBuf::from("../../../a/b/c.rs"));
    }

    #[test]
    fn test_diff_empty_base() {
        let result = diff_relative_paths(Path::new("src/main.rs"), Path::new(""));
        assert_eq!(result, PathBuf::from("src/main.rs"));
    }

    // -- PathRewriter::new mode selection --

    #[test]
    fn test_rewriter_identity_when_cwd_is_repo_root() {
        let rw = PathRewriter::new(Path::new("/repo"), Path::new("/repo"));
        assert_eq!(rw.rewrite("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_rewriter_relative_when_cwd_inside_repo() {
        let rw = PathRewriter::new(Path::new("/repo"), Path::new("/repo/src"));
        assert_eq!(rw.rewrite("src/main.rs"), "main.rs");
        assert_eq!(rw.rewrite("README.md"), "../README.md");
    }

    #[test]
    fn test_rewriter_relative_nested_subdir() {
        let rw = PathRewriter::new(Path::new("/repo"), Path::new("/repo/src/cli"));
        assert_eq!(rw.rewrite("src/cli/main.rs"), "main.rs");
        assert_eq!(rw.rewrite("src/core/mod.rs"), "../core/mod.rs");
        assert_eq!(rw.rewrite("README.md"), "../../README.md");
    }

    #[test]
    fn test_rewriter_absolute_when_cwd_outside_repo() {
        let rw = PathRewriter::new(Path::new("/repo"), Path::new("/tmp"));
        assert_eq!(rw.rewrite("src/main.rs"), "/repo/src/main.rs");
    }

    #[test]
    fn test_rewriter_identity_passthrough() {
        let rw = PathRewriter::identity();
        assert_eq!(rw.rewrite("src/main.rs"), "src/main.rs");
    }
}
