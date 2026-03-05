//! Binary file detection and filtering.
//!
//! Provides heuristics to identify binary files by content (null-byte check)
//! and by file extension, so the indexer can skip non-text files.

use std::path::Path;

/// Maximum number of bytes to inspect when checking for binary content.
const BINARY_CHECK_LENGTH: usize = 8_192;

/// Default maximum file size (1 MB) for indexing.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1_048_576;

/// Known binary file extensions (lowercase, without leading dot).
const BINARY_EXTENSIONS: &[&str] = &[
    // Images
    "png", "jpg", "jpeg", "gif", "bmp", "ico", // Compiled / object files
    "wasm", "o", "obj", "a", "lib", "so", "dylib", "dll", "exe", "bin", "class",
    // Archives
    "jar", "zip", "gz", "tar", "7z", "rar", "zst", // Documents
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", // Media
    "mp3", "mp4", "wav", "avi", "mov", // Fonts
    "ttf", "otf", "woff", "woff2", "eot", // Python bytecode
    "pyc", "pyo", // macOS metadata
    "DS_Store",
];

/// Returns `true` if `content` appears to be binary.
///
/// Checks for null bytes in the first 8 KB of the content,
/// matching the heuristic used by ripgrep.
pub fn is_binary_content(content: &[u8]) -> bool {
    let check_len = content.len().min(BINARY_CHECK_LENGTH);
    content[..check_len].contains(&0)
}

/// Returns `true` if the given file extension (without dot) is a known binary format.
pub fn is_binary_extension(ext: &str) -> bool {
    BINARY_EXTENSIONS
        .iter()
        .any(|&e| e.eq_ignore_ascii_case(ext))
}

/// Returns `true` if the file path has a known binary extension.
pub fn is_binary_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_binary_extension)
}

/// Determines whether a file should be indexed.
///
/// Returns `false` (and logs at debug level) if any of these hold:
/// - The file's extension is a known binary format
/// - The content length exceeds `max_size`
/// - The content contains null bytes in the first 8 KB
pub fn should_index_file(path: &Path, content: &[u8], max_size: u64) -> bool {
    if is_binary_path(path) {
        tracing::debug!(path = %path.display(), "skipping file: binary extension");
        return false;
    }

    if content.len() as u64 > max_size {
        tracing::debug!(
            path = %path.display(),
            size = content.len(),
            max_size,
            "skipping file: exceeds size limit"
        );
        return false;
    }

    if is_binary_content(content) {
        tracing::debug!(path = %path.display(), "skipping file: binary content detected");
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_binary_content tests ---

    #[test]
    fn text_content_is_not_binary() {
        let content = b"Hello, world!\nThis is plain text.\n";
        assert!(!is_binary_content(content));
    }

    #[test]
    fn content_with_null_byte_is_binary() {
        let content = b"Hello\x00World";
        assert!(is_binary_content(content));
    }

    #[test]
    fn empty_content_is_not_binary() {
        let content: &[u8] = b"";
        assert!(!is_binary_content(content));
    }

    #[test]
    fn null_only_after_8kb_is_not_binary() {
        // 8192 bytes of 'a' followed by a null byte
        let mut content = vec![b'a'; 8192];
        content.push(0);
        assert!(!is_binary_content(&content));
    }

    #[test]
    fn null_at_end_of_8kb_window_is_binary() {
        // 8191 bytes of 'a' then a null — still within the 8KB window
        let mut content = vec![b'a'; 8191];
        content.push(0);
        assert!(is_binary_content(&content));
    }

    // --- is_binary_extension tests ---

    #[test]
    fn known_binary_extensions_detected() {
        for ext in &["png", "jpg", "exe", "zip", "pdf", "wasm", "pyc", "DS_Store"] {
            assert!(is_binary_extension(ext), "{ext} should be binary");
        }
    }

    #[test]
    fn source_code_extensions_not_binary() {
        for ext in &[
            "rs", "py", "js", "ts", "go", "c", "h", "toml", "json", "md", "txt",
        ] {
            assert!(!is_binary_extension(ext), "{ext} should not be binary");
        }
    }

    #[test]
    fn extension_check_is_case_insensitive() {
        assert!(is_binary_extension("PNG"));
        assert!(is_binary_extension("Jpg"));
    }

    // --- is_binary_path tests ---

    #[test]
    fn binary_path_detected() {
        assert!(is_binary_path(Path::new("image.png")));
        assert!(is_binary_path(Path::new("/some/dir/file.exe")));
    }

    #[test]
    fn text_path_not_binary() {
        assert!(!is_binary_path(Path::new("main.rs")));
        assert!(!is_binary_path(Path::new("/src/lib.rs")));
    }

    #[test]
    fn path_without_extension_not_binary() {
        assert!(!is_binary_path(Path::new("Makefile")));
    }

    // --- should_index_file tests ---

    #[test]
    fn should_index_normal_text_file() {
        let path = Path::new("src/main.rs");
        let content = b"fn main() {}\n";
        assert!(should_index_file(path, content, DEFAULT_MAX_FILE_SIZE));
    }

    #[test]
    fn should_skip_binary_extension() {
        let path = Path::new("image.png");
        let content = b"not actually binary content";
        assert!(!should_index_file(path, content, DEFAULT_MAX_FILE_SIZE));
    }

    #[test]
    fn should_skip_binary_content() {
        let path = Path::new("src/data.txt");
        let content = b"text\x00binary";
        assert!(!should_index_file(path, content, DEFAULT_MAX_FILE_SIZE));
    }

    #[test]
    fn should_skip_oversized_file() {
        let path = Path::new("src/huge.rs");
        let content = vec![b'a'; 2_000_000]; // 2 MB
        assert!(!should_index_file(path, &content, DEFAULT_MAX_FILE_SIZE));
    }

    #[test]
    fn should_index_file_at_exact_size_limit() {
        let path = Path::new("src/big.rs");
        let content = vec![b'a'; DEFAULT_MAX_FILE_SIZE as usize];
        assert!(should_index_file(path, &content, DEFAULT_MAX_FILE_SIZE));
    }

    #[test]
    fn should_skip_file_one_byte_over_limit() {
        let path = Path::new("src/big.rs");
        let content = vec![b'a'; DEFAULT_MAX_FILE_SIZE as usize + 1];
        assert!(!should_index_file(path, &content, DEFAULT_MAX_FILE_SIZE));
    }
}
