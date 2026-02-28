//! Content verification for trigram search candidates.
//!
//! After trigram intersection produces candidate file IDs, this module verifies
//! that the query pattern actually matches in the file content. It supports
//! literal substring, regex, and case-insensitive matching, with configurable
//! context line extraction and adjacent context merging.

/// Precomputed index of newline positions for O(1) byte-offset-to-line mapping.
///
/// Constructed once per file content, then used for all match-to-line conversions.
/// Line numbers are 1-based. The index stores the byte offset of each `\n`.
#[derive(Debug)]
struct LineIndex {
    /// Byte offsets of each `\n` character in the content.
    /// `newline_offsets[i]` is the byte offset of the (i+1)-th newline.
    newline_offsets: Vec<usize>,
    /// Total content length in bytes.
    content_len: usize,
}

impl LineIndex {
    /// Build a line index from file content.
    fn new(content: &[u8]) -> Self {
        let newline_offsets: Vec<usize> = content
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'\n')
            .map(|(i, _)| i)
            .collect();
        LineIndex {
            newline_offsets,
            content_len: content.len(),
        }
    }

    /// Return the number of lines in the content.
    ///
    /// A trailing newline does not add an extra empty line.
    fn line_count(&self) -> usize {
        if self.content_len == 0 {
            return 0;
        }
        if self.newline_offsets.last() == Some(&(self.content_len - 1)) {
            // Content ends with \n -- the last "line" is empty, don't count it
            self.newline_offsets.len()
        } else {
            self.newline_offsets.len() + 1
        }
    }

    /// Return the 1-based line number for a byte offset.
    ///
    /// Uses binary search on newline offsets for O(log n) lookup.
    fn line_at_byte(&self, byte_offset: usize) -> u32 {
        // Number of newlines before this offset = line index (0-based)
        let line_0 = self.newline_offsets.partition_point(|&nl| nl < byte_offset);
        (line_0 + 1) as u32
    }

    /// Return the 1-based column number for a byte offset.
    fn column_at_byte(&self, byte_offset: usize) -> u32 {
        let line_0 = self.newline_offsets.partition_point(|&nl| nl < byte_offset);
        if line_0 == 0 {
            // First line: column = offset + 1
            (byte_offset + 1) as u32
        } else {
            // Column = offset - (previous newline offset)
            let prev_nl = self.newline_offsets[line_0 - 1];
            (byte_offset - prev_nl) as u32
        }
    }

    /// Return the content of a 1-based line number (without trailing newline).
    fn line_content<'a>(&self, content: &'a [u8], line_number: u32) -> &'a str {
        let line_0 = (line_number - 1) as usize;
        let start = if line_0 == 0 {
            0
        } else {
            self.newline_offsets[line_0 - 1] + 1
        };
        let end = if line_0 < self.newline_offsets.len() {
            self.newline_offsets[line_0]
        } else {
            self.content_len
        };
        // Strip trailing \r for Windows line endings
        let slice = &content[start..end];
        let s = std::str::from_utf8(slice).unwrap_or("");
        s.strip_suffix('\r').unwrap_or(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- LineIndex tests ----

    #[test]
    fn test_line_index_simple() {
        let content = b"line one\nline two\nline three\n";
        let idx = LineIndex::new(content);
        // 3 lines of content + trailing newline
        assert_eq!(idx.line_count(), 3);
    }

    #[test]
    fn test_line_index_byte_offset_to_line() {
        let content = b"aaa\nbbb\nccc\n";
        // Offsets: a=0,1,2  \n=3  b=4,5,6  \n=7  c=8,9,10  \n=11
        let idx = LineIndex::new(content);
        assert_eq!(idx.line_at_byte(0), 1); // 'a' -> line 1
        assert_eq!(idx.line_at_byte(2), 1); // last 'a' -> line 1
        assert_eq!(idx.line_at_byte(4), 2); // first 'b' -> line 2
        assert_eq!(idx.line_at_byte(8), 3); // first 'c' -> line 3
    }

    #[test]
    fn test_line_index_no_trailing_newline() {
        let content = b"aaa\nbbb";
        let idx = LineIndex::new(content);
        assert_eq!(idx.line_count(), 2);
        assert_eq!(idx.line_at_byte(0), 1);
        assert_eq!(idx.line_at_byte(4), 2);
    }

    #[test]
    fn test_line_index_get_line_content() {
        let content = b"fn main() {}\nfn helper() {}\nfn test() {}\n";
        let idx = LineIndex::new(content);
        assert_eq!(idx.line_content(content, 1), "fn main() {}");
        assert_eq!(idx.line_content(content, 2), "fn helper() {}");
        assert_eq!(idx.line_content(content, 3), "fn test() {}");
    }

    #[test]
    fn test_line_index_empty_content() {
        let content = b"";
        let idx = LineIndex::new(content);
        assert_eq!(idx.line_count(), 0);
    }

    #[test]
    fn test_line_index_single_line_no_newline() {
        let content = b"hello world";
        let idx = LineIndex::new(content);
        assert_eq!(idx.line_count(), 1);
        assert_eq!(idx.line_at_byte(0), 1);
        assert_eq!(idx.line_at_byte(10), 1);
        assert_eq!(idx.line_content(content, 1), "hello world");
    }

    #[test]
    fn test_line_index_column_at_byte() {
        let content = b"fn main() {}\n    println!(\"hello\");\n";
        let idx = LineIndex::new(content);
        // byte 0 = 'f', column 1 on line 1
        assert_eq!(idx.column_at_byte(0), 1);
        // byte 17 = 'p' in println, column 5 on line 2 (after 4 spaces)
        assert_eq!(idx.column_at_byte(17), 5);
    }
}
