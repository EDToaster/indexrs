use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use indexrs_core::error::IndexError;

pub struct PreviewOptions {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub context: Option<usize>,
    pub highlight_line: Option<usize>,
    pub color_enabled: bool,
}

/// Check whether `bat` is installed and available on PATH.
pub fn is_bat_available() -> bool {
    Command::new("bat")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Preview a file using `bat` with syntax highlighting and line ranges.
pub fn run_bat_preview(opts: &PreviewOptions) -> Result<(), IndexError> {
    let mut cmd = Command::new("bat");
    cmd.arg("--style=numbers,header").arg("--color=always");

    if let Some(hl) = opts.highlight_line.or(opts.line) {
        cmd.arg(format!("--highlight-line={hl}"));
    }

    if let Some(line) = opts.line {
        let ctx = opts.context.unwrap_or(20);
        let start = line.saturating_sub(ctx);
        let end = line + ctx;
        cmd.arg(format!("--line-range={start}:{end}"));
    }

    cmd.arg("--").arg(&opts.file);

    let status = cmd.status().map_err(IndexError::Io)?;
    if !status.success() {
        return Err(IndexError::Io(std::io::Error::other(format!(
            "bat exited with status {status}"
        ))));
    }
    Ok(())
}

/// Render a file preview using the built-in line-number renderer (no syntax highlighting).
///
/// Centers the output on `opts.line` if specified, showing `opts.context` lines
/// above and below. Falls back to `FZF_PREVIEW_LINES` env var or the full file.
pub fn render_builtin_preview<W: Write>(
    opts: &PreviewOptions,
    out: &mut W,
) -> Result<(), IndexError> {
    let content = std::fs::read_to_string(&opts.file).map_err(IndexError::Io)?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let preview_lines = opts
        .context
        .map(|c| c * 2 + 1)
        .or_else(|| {
            std::env::var("FZF_PREVIEW_LINES")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(total_lines);

    let (start, end) = if let Some(center) = opts.line {
        let center = center.saturating_sub(1); // Convert to 0-indexed
        let half = preview_lines / 2;
        let start = center.saturating_sub(half);
        let end = (start + preview_lines).min(total_lines);
        (start, end)
    } else {
        (0, preview_lines.min(total_lines))
    };

    let line_num_width = format!("{}", end).len();

    for (i, line_content) in lines.iter().enumerate().take(end).skip(start) {
        let line_num = i + 1;
        let is_highlighted = opts.highlight_line.is_some_and(|hl| hl == line_num);

        if is_highlighted && opts.color_enabled {
            writeln!(
                out,
                "\x1b[7m{line_num:>line_num_width$} {line_content}\x1b[0m",
            )
            .map_err(IndexError::Io)?;
        } else {
            writeln!(out, "{line_num:>line_num_width$} {line_content}").map_err(IndexError::Io)?;
        }
    }

    Ok(())
}

/// Run the preview command: use `bat` if available, otherwise fall back to built-in renderer.
pub fn run_preview(opts: &PreviewOptions) -> Result<(), IndexError> {
    if is_bat_available() {
        run_bat_preview(opts)
    } else {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        render_builtin_preview(opts, &mut out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_bat_available_detection() {
        // Just verify this doesn't panic; result depends on system
        let _ = is_bat_available();
    }

    #[test]
    fn test_render_preview_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        fs::write(&file, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

        let mut buf = Vec::new();
        let opts = PreviewOptions {
            file: file.clone(),
            line: Some(2),
            context: Some(5),
            highlight_line: Some(2),
            color_enabled: false,
        };

        render_builtin_preview(&opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("println"));
        assert!(output.contains("1"));
        assert!(output.contains("2"));
    }

    #[test]
    fn test_render_preview_centers_on_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        let content: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        fs::write(&file, &content).unwrap();

        let mut buf = Vec::new();
        let opts = PreviewOptions {
            file: file.clone(),
            line: Some(25),
            context: Some(3),
            highlight_line: None,
            color_enabled: false,
        };

        render_builtin_preview(&opts, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("line 25"));
        assert!(output.contains("line 22"));
        assert!(output.contains("line 28"));
    }

    #[test]
    fn test_render_preview_file_not_found() {
        let mut buf = Vec::new();
        let opts = PreviewOptions {
            file: PathBuf::from("/nonexistent/file.rs"),
            line: None,
            context: None,
            highlight_line: None,
            color_enabled: false,
        };

        let result = render_builtin_preview(&opts, &mut buf);
        assert!(result.is_err());
    }
}
