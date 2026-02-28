use std::io::{self, BufWriter, Write};

/// Process exit codes per the fzf convention.
///
/// - 0: Results found
/// - 1: No results found (not an error)
/// - 2: Error occurred
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    NoResults = 1,
    Error = 2,
}

/// Streaming line writer with per-line flush for the first N lines,
/// then batch flush for performance.
///
/// Generic over `W: Write` for testability (use `Vec<u8>` in tests,
/// `Stdout` in production).
pub struct StreamingWriter<W: Write> {
    writer: BufWriter<W>,
    count: usize,
    flush_threshold: usize,
}

impl<W: Write> StreamingWriter<W> {
    /// Create a new streaming writer wrapping the given output.
    pub fn new(inner: W) -> Self {
        Self {
            writer: BufWriter::new(inner),
            count: 0,
            flush_threshold: 1000,
        }
    }

    /// Write a single line (appends newline) and flush if below threshold.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        writeln!(self.writer, "{line}")?;
        self.count += 1;
        if self.count <= self.flush_threshold {
            self.writer.flush()?;
        }
        Ok(())
    }

    /// Return the number of lines written so far.
    #[allow(dead_code)]
    pub fn lines_written(&self) -> usize {
        self.count
    }

    /// Flush any remaining buffered output.
    pub fn finish(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Install the default SIGPIPE handler so broken pipes exit cleanly.
///
/// When fzf kills a reload process or the user pipes to `head`, Rust's
/// default SIGPIPE handler prints an error. This restores the OS default
/// (immediate termination) for clean fzf integration.
pub fn setup_sigpipe() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_code_values() {
        assert_eq!(ExitCode::Success as i32, 0);
        assert_eq!(ExitCode::NoResults as i32, 1);
        assert_eq!(ExitCode::Error as i32, 2);
    }

    #[test]
    fn test_streaming_writer_to_vec() {
        let mut buf = Vec::new();
        {
            let mut writer = StreamingWriter::new(&mut buf);
            writer.write_line("hello").unwrap();
            writer.write_line("world").unwrap();
            writer.finish().unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "hello\nworld\n");
    }

    #[test]
    fn test_streaming_writer_count() {
        let mut buf = Vec::new();
        let mut writer = StreamingWriter::new(&mut buf);
        assert_eq!(writer.lines_written(), 0);
        writer.write_line("a").unwrap();
        assert_eq!(writer.lines_written(), 1);
        writer.write_line("b").unwrap();
        assert_eq!(writer.lines_written(), 2);
        writer.finish().unwrap();
    }

    #[test]
    fn test_streaming_writer_empty() {
        let mut buf = Vec::new();
        {
            let mut writer = StreamingWriter::new(&mut buf);
            writer.finish().unwrap();
        }
        assert!(buf.is_empty());
    }
}
