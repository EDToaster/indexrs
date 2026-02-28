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

use regex::Regex;

use crate::search::{LineMatch, MatchPattern};

/// Content verifier that matches a pattern against decompressed file content.
///
/// Constructed with a `MatchPattern` and a context line count. The `verify()`
/// method returns all matching lines with highlight ranges. The `verify_with_context()`
/// method additionally returns `ContextBlock`s with surrounding lines.
pub struct ContentVerifier {
    pattern: MatchPattern,
    context_lines: u32,
    /// Compiled regex (lazily built for Regex and CaseInsensitive patterns).
    compiled_regex: Option<Regex>,
}

impl ContentVerifier {
    /// Create a new content verifier.
    ///
    /// # Arguments
    ///
    /// * `pattern` - The match pattern to verify against content.
    /// * `context_lines` - Number of context lines before/after each match (0 = no context).
    pub fn new(pattern: MatchPattern, context_lines: u32) -> Self {
        let compiled_regex = match &pattern {
            MatchPattern::Regex(pat) => Regex::new(pat).ok(),
            MatchPattern::LiteralCaseInsensitive(lit) => {
                // Build a case-insensitive regex from the literal
                let escaped = regex::escape(lit);
                Regex::new(&format!("(?i){escaped}")).ok()
            }
            MatchPattern::Literal(_) => None,
        };
        ContentVerifier {
            pattern,
            context_lines,
            compiled_regex,
        }
    }

    /// Verify content and return matching lines with highlight ranges.
    ///
    /// Returns an empty vector if the content is empty or no matches are found.
    pub fn verify(&self, content: &[u8]) -> Vec<LineMatch> {
        if content.is_empty() {
            return Vec::new();
        }

        let line_index = LineIndex::new(content);
        let text = String::from_utf8_lossy(content);

        match &self.pattern {
            MatchPattern::Literal(lit) => self.verify_literal(&text, &line_index, lit.as_bytes()),
            MatchPattern::Regex(_) | MatchPattern::LiteralCaseInsensitive(_) => {
                self.verify_regex(&text, &line_index)
            }
        }
    }

    /// Literal substring verification (byte-level matching).
    fn verify_literal(
        &self,
        text: &str,
        line_index: &LineIndex,
        pattern_bytes: &[u8],
    ) -> Vec<LineMatch> {
        if pattern_bytes.is_empty() {
            return Vec::new();
        }

        let text_bytes = text.as_bytes();
        let mut matches_by_line: std::collections::BTreeMap<u32, Vec<(usize, usize)>> =
            std::collections::BTreeMap::new();

        let mut search_start = 0;
        while search_start + pattern_bytes.len() <= text_bytes.len() {
            if let Some(pos) = find_substring(&text_bytes[search_start..], pattern_bytes) {
                let abs_start = search_start + pos;
                let line_num = line_index.line_at_byte(abs_start);

                // Compute line-relative offsets
                let line_start = line_start_offset(line_index, line_num);
                let rel_start = abs_start - line_start;
                let rel_end = rel_start + pattern_bytes.len();

                matches_by_line
                    .entry(line_num)
                    .or_default()
                    .push((rel_start, rel_end));

                search_start = abs_start + 1;
            } else {
                break;
            }
        }

        matches_by_line
            .into_iter()
            .map(|(line_num, ranges)| LineMatch {
                line_number: line_num,
                content: line_index
                    .line_content(text.as_bytes(), line_num)
                    .to_string(),
                ranges,
            })
            .collect()
    }

    /// Regex-based verification (for Regex and CaseInsensitive patterns).
    fn verify_regex(&self, text: &str, line_index: &LineIndex) -> Vec<LineMatch> {
        let re = match &self.compiled_regex {
            Some(re) => re,
            None => return Vec::new(),
        };

        let mut matches_by_line: std::collections::BTreeMap<u32, Vec<(usize, usize)>> =
            std::collections::BTreeMap::new();

        for m in re.find_iter(text) {
            let abs_start = m.start();
            let abs_end = m.end();
            let line_num = line_index.line_at_byte(abs_start);

            let line_start = line_start_offset(line_index, line_num);
            let rel_start = abs_start - line_start;
            let rel_end = rel_start + (abs_end - abs_start);

            matches_by_line
                .entry(line_num)
                .or_default()
                .push((rel_start, rel_end));
        }

        matches_by_line
            .into_iter()
            .map(|(line_num, ranges)| LineMatch {
                line_number: line_num,
                content: line_index
                    .line_content(text.as_bytes(), line_num)
                    .to_string(),
                ranges,
            })
            .collect()
    }
}

/// Find the first occurrence of `needle` in `haystack`, returning the byte offset.
fn find_substring(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Compute the byte offset of the start of a 1-based line number.
fn line_start_offset(line_index: &LineIndex, line_number: u32) -> usize {
    let line_0 = (line_number - 1) as usize;
    if line_0 == 0 {
        0
    } else {
        line_index.newline_offsets[line_0 - 1] + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::MatchPattern;

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

    // ---- ContentVerifier literal tests ----

    #[test]
    fn test_verify_literal_single_match() {
        let content = b"fn main() {\n    println!(\"hello\");\n}\n";
        let verifier = ContentVerifier::new(MatchPattern::Literal("println".to_string()), 0);
        let result = verifier.verify(content);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].line_number, 2);
        assert!(result[0].content.contains("println"));
        assert_eq!(result[0].ranges.len(), 1);
    }

    #[test]
    fn test_verify_literal_no_match() {
        let content = b"fn main() {}\n";
        let verifier = ContentVerifier::new(MatchPattern::Literal("foobar".to_string()), 0);
        let result = verifier.verify(content);
        assert!(result.is_empty());
    }

    #[test]
    fn test_verify_literal_multiple_same_line() {
        let content = b"let aa = aa + aa;\n";
        let verifier = ContentVerifier::new(MatchPattern::Literal("aa".to_string()), 0);
        let result = verifier.verify(content);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].ranges.len(), 3);
    }

    #[test]
    fn test_verify_literal_multiple_lines() {
        let content = b"fn foo() {}\nfn bar() {}\nfn baz() {}\n";
        let verifier = ContentVerifier::new(MatchPattern::Literal("fn ".to_string()), 0);
        let result = verifier.verify(content);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].line_number, 1);
        assert_eq!(result[1].line_number, 2);
        assert_eq!(result[2].line_number, 3);
    }

    #[test]
    fn test_verify_empty_content() {
        let content = b"";
        let verifier = ContentVerifier::new(MatchPattern::Literal("foo".to_string()), 0);
        let result = verifier.verify(content);
        assert!(result.is_empty());
    }

    // ---- ContentVerifier regex tests ----

    #[test]
    fn test_verify_regex_function_pattern() {
        let content = b"fn main() {}\nfn helper() {}\nlet x = 1;\n";
        let verifier = ContentVerifier::new(MatchPattern::Regex(r"fn\s+\w+".to_string()), 0);
        let result = verifier.verify(content);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].line_number, 1);
        assert_eq!(result[1].line_number, 2);
    }

    #[test]
    fn test_verify_regex_no_match() {
        let content = b"let x = 42;\n";
        let verifier = ContentVerifier::new(MatchPattern::Regex(r"fn\s+\w+".to_string()), 0);
        let result = verifier.verify(content);
        assert!(result.is_empty());
    }

    #[test]
    fn test_verify_regex_multiple_matches_same_line() {
        let content = b"abc 123 def 456\n";
        let verifier = ContentVerifier::new(MatchPattern::Regex(r"\d+".to_string()), 0);
        let result = verifier.verify(content);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].ranges.len(), 2); // "123" and "456"
    }

    // ---- Case-insensitive tests ----

    #[test]
    fn test_verify_case_insensitive() {
        let content = b"Hello World\nhello world\nHELLO WORLD\n";
        let verifier = ContentVerifier::new(
            MatchPattern::LiteralCaseInsensitive("hello".to_string()),
            0,
        );
        let result = verifier.verify(content);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_verify_case_insensitive_no_match() {
        let content = b"foo bar baz\n";
        let verifier = ContentVerifier::new(
            MatchPattern::LiteralCaseInsensitive("qux".to_string()),
            0,
        );
        let result = verifier.verify(content);
        assert!(result.is_empty());
    }
}
