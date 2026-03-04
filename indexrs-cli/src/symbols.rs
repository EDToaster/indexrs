#[cfg(feature = "symbols")]
use nu_ansi_term::{Color, Style};

#[cfg(feature = "symbols")]
use crate::color::ColorConfig;

/// Format a symbol match as `kind:name:file:line` (fzf-compatible).
///
/// With colors: yellow bold kind, white bold name, magenta path, green line number.
/// Lines are 0-indexed in the index, so add 1 for display.
#[cfg(feature = "symbols")]
pub fn format_symbol_line(
    m: &indexrs_core::symbol_index::SymbolMatch,
    color: &ColorConfig,
    path_rewriter: &crate::paths::PathRewriter,
) -> String {
    let kind_label = m.kind.short_label();
    let display_line = m.line + 1; // 0-based -> 1-based for display
    let display_path = path_rewriter.rewrite(&m.path);

    if !color.enabled {
        return format!(
            "{kind_label}:{name}:{display_path}:{display_line}",
            name = m.name
        );
    }

    let colored_kind = Style::new().bold().fg(Color::Yellow).paint(kind_label);
    let colored_name = Style::new().bold().fg(Color::White).paint(&m.name);
    let colored_path = Color::Magenta.paint(&display_path);
    let colored_line = Color::Green.paint(display_line.to_string());

    format!("{colored_kind}:{colored_name}:{colored_path}:{colored_line}")
}

#[cfg(test)]
#[cfg(feature = "symbols")]
mod tests {
    use super::*;
    use crate::paths::PathRewriter;
    use indexrs_core::symbol_index::SymbolMatch;
    use indexrs_core::types::{FileId, SegmentId, SymbolKind};

    fn make_match() -> SymbolMatch {
        SymbolMatch {
            name: "main".to_string(),
            kind: SymbolKind::Function,
            path: "src/main.rs".to_string(),
            line: 9, // 0-based
            column: 0,
            file_id: FileId(0),
            segment_id: SegmentId(0),
            score: 1.0,
        }
    }

    #[test]
    fn test_format_symbol_line_no_color() {
        let color = ColorConfig::new(false);
        let rewriter = PathRewriter::identity();
        let m = make_match();
        let result = format_symbol_line(&m, &color, &rewriter);
        assert_eq!(result, "fn:main:src/main.rs:10");
    }

    #[test]
    fn test_format_symbol_line_with_color() {
        let color = ColorConfig::new(true);
        let rewriter = PathRewriter::identity();
        let m = make_match();
        let result = format_symbol_line(&m, &color, &rewriter);
        // Should contain ANSI escape codes
        assert!(result.contains("\x1b["));
        // Should still contain the key components
        assert!(result.contains("fn"));
        assert!(result.contains("main"));
        assert!(result.contains("src/main.rs"));
        assert!(result.contains("10"));
    }

    #[test]
    fn test_format_symbol_line_zero_based_to_one_based() {
        let color = ColorConfig::new(false);
        let rewriter = PathRewriter::identity();
        let mut m = make_match();
        m.line = 0; // 0-based line 0 -> display line 1
        let result = format_symbol_line(&m, &color, &rewriter);
        assert!(result.ends_with(":1"));
    }

    #[test]
    fn test_format_symbol_line_with_path_rewriter() {
        let color = ColorConfig::new(false);
        let rewriter = PathRewriter::new(
            std::path::Path::new("/repo"),
            std::path::Path::new("/repo/src"),
        );
        let m = make_match();
        let result = format_symbol_line(&m, &color, &rewriter);
        // "src/main.rs" rewritten relative to "src/" should become "main.rs"
        assert_eq!(result, "fn:main:main.rs:10");
    }
}
