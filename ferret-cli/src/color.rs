use nu_ansi_term::{Color, Style};

/// Configuration for ANSI color output.
pub struct ColorConfig {
    pub enabled: bool,
}

impl ColorConfig {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Format a file path with ANSI colors: dim directories, bold filename, cyan extension.
    pub fn format_file_path(&self, path: &str) -> String {
        if !self.enabled {
            return path.to_string();
        }

        // Split into directory and filename
        let (dir, file) = match path.rfind('/') {
            Some(pos) => (&path[..=pos], &path[pos + 1..]),
            None => ("", path),
        };

        // Split filename into stem and extension
        let (stem, ext) = match file.rfind('.') {
            Some(pos) => (&file[..pos], &file[pos..]), // includes the dot
            None => (file, ""),
        };

        let mut result = String::new();
        if !dir.is_empty() {
            result.push_str(&Style::new().dimmed().paint(dir).to_string());
        }
        result.push_str(&Style::new().bold().paint(stem).to_string());
        if !ext.is_empty() {
            result.push_str(&Color::Cyan.paint(ext).to_string());
        }
        result
    }

    /// Format a vimgrep-style search output line.
    ///
    /// Format: `file:line:col:content` with ANSI colors when enabled.
    /// Colors: magenta path, green line/col numbers, red bold match highlights.
    pub fn format_search_line(
        &self,
        path: &str,
        line: u32,
        col: usize,
        content: &str,
        ranges: &[(usize, usize)],
    ) -> String {
        if !self.enabled {
            return format!("{path}:{line}:{col}:{content}");
        }

        let colored_path = Color::Magenta.paint(path).to_string();
        let colored_line = Color::Green.paint(line.to_string()).to_string();
        let colored_col = Color::Green.paint(col.to_string()).to_string();
        let colored_content = self.highlight_ranges(content, ranges);

        format!("{colored_path}:{colored_line}:{colored_col}:{colored_content}")
    }

    /// Highlight byte ranges in content with red bold.
    pub fn highlight_ranges(&self, content: &str, ranges: &[(usize, usize)]) -> String {
        if !self.enabled || ranges.is_empty() {
            return content.to_string();
        }

        let mut result = String::new();
        let mut last_end = 0;
        let style = Style::new().bold().fg(Color::Red);

        for &(start, end) in ranges {
            let start = start.min(content.len());
            let end = end.min(content.len());
            if start > last_end {
                result.push_str(&content[last_end..start]);
            }
            result.push_str(&style.paint(&content[start..end]).to_string());
            last_end = end;
        }
        if last_end < content.len() {
            result.push_str(&content[last_end..]);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_color_config_from_always() {
        let config = ColorConfig::new(true);
        assert!(config.enabled);
    }

    #[test]
    fn test_color_config_from_never() {
        let config = ColorConfig::new(false);
        assert!(!config.enabled);
    }

    #[test]
    fn test_format_file_path_no_color() {
        let config = ColorConfig::new(false);
        assert_eq!(config.format_file_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_format_file_path_with_color() {
        let config = ColorConfig::new(true);
        let result = config.format_file_path("src/main.rs");
        // Should contain ANSI escape codes
        assert!(result.contains("\x1b["));
        // Should still contain the path components
        assert!(result.contains("src/"));
        assert!(result.contains("main"));
        assert!(result.contains(".rs"));
    }

    #[test]
    fn test_format_file_path_no_extension() {
        let config = ColorConfig::new(true);
        let result = config.format_file_path("Makefile");
        assert!(result.contains("Makefile"));
    }

    #[test]
    fn test_format_file_path_no_directory() {
        let config = ColorConfig::new(true);
        let result = config.format_file_path("main.rs");
        assert!(result.contains("main"));
    }

    #[test]
    fn test_format_search_line_no_color() {
        let config = ColorConfig::new(false);
        let result = config.format_search_line("src/main.rs", 10, 5, "let x = 1;", &[]);
        assert_eq!(result, "src/main.rs:10:5:let x = 1;");
    }

    #[test]
    fn test_format_search_line_with_color() {
        let config = ColorConfig::new(true);
        let result = config.format_search_line("src/main.rs", 10, 5, "let x = 1;", &[(4, 5)]);
        assert!(result.contains("\x1b["));
    }

    #[test]
    fn test_highlight_ranges_in_content() {
        let config = ColorConfig::new(true);
        let result = config.highlight_ranges("hello world", &[(0, 5)]);
        assert!(result.contains("\x1b["));
        assert!(result.contains("hello"));
        assert!(result.contains("world"));
    }

    #[test]
    fn test_highlight_ranges_no_color() {
        let config = ColorConfig::new(false);
        let result = config.highlight_ranges("hello world", &[(0, 5)]);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_highlight_ranges_multiple() {
        let config = ColorConfig::new(true);
        let result = config.highlight_ranges("aXbXc", &[(1, 2), (3, 4)]);
        assert!(result.contains("a"));
        assert!(result.contains("b"));
        assert!(result.contains("c"));
    }
}
