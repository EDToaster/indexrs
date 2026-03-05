# Candidate Verification (Regex/Literal Match) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the basic substring-only content verification in `multi_search.rs` with a full verification module (`verify.rs`) that supports literal, regex, and case-insensitive matching against decompressed file content. Add context line extraction (configurable N lines before/after), adjacent context merging, and structured output types that preserve backward compatibility with `SearchResult`/`FileMatch`/`LineMatch`.

**Architecture:** The new `verify.rs` module provides a `ContentVerifier` that takes a `MatchPattern` enum (Literal, Regex, LiteralCaseInsensitive) and verifies candidate files against actual content. Internally it: (1) builds a `LineIndex` (precomputed newline byte offsets) for O(1) byte-offset-to-line-number conversion, (2) runs the pattern match to get byte-offset ranges, (3) maps byte offsets to line numbers and column positions to build `LineMatch` entries, (4) collects context lines around each match, and (5) merges adjacent context blocks when matches are within `2*context_lines` of each other. The existing `verify_content_matches()` in `multi_search.rs` is replaced by delegating to `ContentVerifier`. The `search.rs` types are extended with `ContextBlock` and `ContextLine` for rich context output. `multi_search.rs` is updated to accept a `MatchPattern` instead of a raw `&str`.

**Tech Stack:** Rust 2024, `regex` crate (already in Cargo.toml), existing `ferret-indexer-core` modules (search, multi_search, content, metadata, segment, types, error), `tempfile` (dev)

**Prerequisite:** ASCII case-fold trigrams plan (`2026-02-27-ascii-casefold-trigrams.md`) must be implemented first. The index stores lowercase-folded trigrams, so the verification step receives candidates found via case-insensitive trigram lookup. This means:
- For **case-insensitive queries** (the default): candidates from the folded index are correct, and `LiteralCaseInsensitive` verification confirms them.
- For **case-sensitive queries** (`case:yes`): candidates from the folded index may include files where the case doesn't match (false positives). The `Literal` verification mode filters these out by doing exact byte comparison. This is correct behavior — slightly more verification work, but always produces correct results.

---

## Task 1: Add `MatchPattern` enum and context types to `search.rs`

**Files:**
- Modify: `ferret-indexer-core/src/search.rs`

### Step 1: Write the failing tests

Add tests to `ferret-indexer-core/src/search.rs` at the end of the existing `mod tests` block:

```rust
#[test]
fn test_match_pattern_literal() {
    let pat = MatchPattern::Literal("println".to_string());
    assert!(matches!(pat, MatchPattern::Literal(_)));
}

#[test]
fn test_match_pattern_regex() {
    let pat = MatchPattern::Regex("fn\\s+\\w+".to_string());
    assert!(matches!(pat, MatchPattern::Regex(_)));
}

#[test]
fn test_match_pattern_case_insensitive() {
    let pat = MatchPattern::LiteralCaseInsensitive("Println".to_string());
    assert!(matches!(pat, MatchPattern::LiteralCaseInsensitive(_)));
}

#[test]
fn test_context_line_construction() {
    let cl = ContextLine {
        line_number: 5,
        content: "use std::io;".to_string(),
    };
    assert_eq!(cl.line_number, 5);
    assert_eq!(cl.content, "use std::io;");
}

#[test]
fn test_context_block_construction() {
    let block = ContextBlock {
        before: vec![ContextLine {
            line_number: 1,
            content: "// before".to_string(),
        }],
        matches: vec![LineMatch {
            line_number: 2,
            content: "fn main() {}".to_string(),
            ranges: vec![(0, 2)],
        }],
        after: vec![ContextLine {
            line_number: 3,
            content: "// after".to_string(),
        }],
    };
    assert_eq!(block.before.len(), 1);
    assert_eq!(block.matches.len(), 1);
    assert_eq!(block.after.len(), 1);
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_match_pattern -v`

Expected: FAIL -- `MatchPattern`, `ContextLine`, `ContextBlock` do not exist.

### Step 3: Implement the types

Add to `ferret-indexer-core/src/search.rs`, after the existing `use` statements and before `LineMatch`:

```rust
/// The pattern type used for content verification.
///
/// Produced by the query parser and consumed by the content verifier.
/// Determines which matching strategy is used during candidate verification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MatchPattern {
    /// Exact byte-level substring match.
    Literal(String),
    /// Regex pattern (compiled with the `regex` crate).
    Regex(String),
    /// Case-insensitive literal match (lowercased comparison).
    LiteralCaseInsensitive(String),
}
```

Add after `FileMatch`, before `SearchResult`:

```rust
/// A non-matching line shown as context around a match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextLine {
    /// 1-based line number within the file.
    pub line_number: u32,
    /// The full text content of the line.
    pub content: String,
}

/// A group of adjacent matches with their surrounding context lines.
///
/// When multiple matches are close together (within `2 * context_lines`),
/// they are merged into a single `ContextBlock` to avoid duplicate context
/// lines and provide a contiguous reading experience.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextBlock {
    /// Context lines before the first match in this block.
    pub before: Vec<ContextLine>,
    /// The matching lines within this block.
    pub matches: Vec<LineMatch>,
    /// Context lines after the last match in this block.
    pub after: Vec<ContextLine>,
}
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_match_pattern test_context -v`

Expected: PASS.

### Step 5: Add re-exports to lib.rs

Update `ferret-indexer-core/src/lib.rs` to export the new types:

```rust
pub use search::{ContextBlock, ContextLine, FileMatch, LineMatch, MatchPattern, SearchResult};
```

### Step 6: Verify full workspace compiles

Run: `cargo check --workspace`

---

## Task 2: Create `verify.rs` with `LineIndex` for byte-offset-to-line mapping

**Files:**
- Create: `ferret-indexer-core/src/verify.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Write the failing tests

Create `ferret-indexer-core/src/verify.rs` with a module doc comment and tests for `LineIndex`:

```rust
//! Content verification for trigram search candidates.
//!
//! After trigram intersection produces candidate file IDs, this module verifies
//! that the query pattern actually matches in the file content. It supports
//! literal substring, regex, and case-insensitive matching, with configurable
//! context line extraction and adjacent context merging.

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
        assert_eq!(idx.line_at_byte(0), 1);   // 'a' -> line 1
        assert_eq!(idx.line_at_byte(2), 1);   // last 'a' -> line 1
        assert_eq!(idx.line_at_byte(4), 2);   // first 'b' -> line 2
        assert_eq!(idx.line_at_byte(8), 3);   // first 'c' -> line 3
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
```

### Step 2: Register the module in lib.rs

Add to `ferret-indexer-core/src/lib.rs`:

```rust
pub mod verify;
```

### Step 3: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_line_index -v`

Expected: FAIL -- `LineIndex` does not exist.

### Step 4: Implement `LineIndex`

Add to `ferret-indexer-core/src/verify.rs`, above the test module:

```rust
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
            .filter(|(_, &b)| b == b'\n')
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
```

### Step 5: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_line_index -v`

Expected: PASS.

---

## Task 3: Implement `ContentVerifier` with literal and regex matching

**Files:**
- Modify: `ferret-indexer-core/src/verify.rs`

### Step 1: Write the failing tests

Add to `verify.rs` test module:

```rust
use crate::search::{LineMatch, MatchPattern};

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
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_verify_literal test_verify_regex test_verify_case -v`

Expected: FAIL -- `ContentVerifier` does not exist.

### Step 3: Implement `ContentVerifier`

Add to `ferret-indexer-core/src/verify.rs`, after `LineIndex`:

```rust
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
                let abs_end = abs_start + pattern_bytes.len();
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
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_verify_literal test_verify_regex test_verify_case -v`

Expected: PASS.

---

## Task 4: Implement context line extraction and merging

**Files:**
- Modify: `ferret-indexer-core/src/verify.rs`

### Step 1: Write the failing tests

Add to `verify.rs` test module:

```rust
use crate::search::{ContextBlock, ContextLine};

// ---- Context line tests ----

#[test]
fn test_context_single_match() {
    let content = b"line 1\nline 2\nline 3\nMATCH\nline 5\nline 6\nline 7\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("MATCH".to_string()), 2);
    let blocks = verifier.verify_with_context(content);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].before.len(), 2); // lines 2, 3
    assert_eq!(blocks[0].before[0].line_number, 2);
    assert_eq!(blocks[0].before[1].line_number, 3);
    assert_eq!(blocks[0].matches.len(), 1);
    assert_eq!(blocks[0].matches[0].line_number, 4);
    assert_eq!(blocks[0].after.len(), 2); // lines 5, 6
    assert_eq!(blocks[0].after[0].line_number, 5);
    assert_eq!(blocks[0].after[1].line_number, 6);
}

#[test]
fn test_context_at_file_start() {
    let content = b"MATCH\nline 2\nline 3\nline 4\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("MATCH".to_string()), 2);
    let blocks = verifier.verify_with_context(content);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].before.len(), 0); // no lines before line 1
    assert_eq!(blocks[0].after.len(), 2);
}

#[test]
fn test_context_at_file_end() {
    let content = b"line 1\nline 2\nMATCH\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("MATCH".to_string()), 2);
    let blocks = verifier.verify_with_context(content);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].before.len(), 2);
    assert_eq!(blocks[0].after.len(), 0); // no lines after last line
}

#[test]
fn test_context_merging_adjacent_matches() {
    // Two matches within 2*context_lines of each other should merge
    let content = b"line 1\nMATCH1\nline 3\nMATCH2\nline 5\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("MATCH".to_string()), 1);
    let blocks = verifier.verify_with_context(content);
    // With context_lines=1, MATCH1 (line 2) context = [1, 3]
    // MATCH2 (line 4) context = [3, 5]
    // They overlap at line 3, so should merge into one block
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].matches.len(), 2);
}

#[test]
fn test_context_separate_blocks() {
    // Two matches far apart should produce separate blocks
    let content = b"MATCH1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nMATCH2\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("MATCH".to_string()), 1);
    let blocks = verifier.verify_with_context(content);
    // MATCH1 (line 1) context after = [2], MATCH2 (line 8) context before = [7]
    // Gap between line 2 and line 7 -- separate blocks
    assert_eq!(blocks.len(), 2);
}

#[test]
fn test_context_zero_lines() {
    let content = b"line 1\nMATCH\nline 3\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("MATCH".to_string()), 0);
    let blocks = verifier.verify_with_context(content);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].before.len(), 0);
    assert_eq!(blocks[0].after.len(), 0);
    assert_eq!(blocks[0].matches.len(), 1);
}

#[test]
fn test_context_no_matches() {
    let content = b"line 1\nline 2\nline 3\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("NOMATCH".to_string()), 2);
    let blocks = verifier.verify_with_context(content);
    assert!(blocks.is_empty());
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_context -v`

Expected: FAIL -- `verify_with_context()` does not exist.

### Step 3: Implement `verify_with_context()`

Add to `ContentVerifier` impl block in `verify.rs`:

```rust
/// Verify content and return context blocks with surrounding lines.
///
/// Each `ContextBlock` contains one or more matching lines plus up to
/// `context_lines` before/after. When matches are close together (within
/// `2 * context_lines`), their blocks are merged to avoid duplicate lines.
///
/// Returns an empty vector if no matches are found.
pub fn verify_with_context(&self, content: &[u8]) -> Vec<ContextBlock> {
    let line_matches = self.verify(content);
    if line_matches.is_empty() {
        return Vec::new();
    }

    let line_index = LineIndex::new(content);
    let total_lines = line_index.line_count() as u32;

    if self.context_lines == 0 {
        // No context: each match is its own block with empty before/after
        return line_matches
            .into_iter()
            .map(|m| ContextBlock {
                before: Vec::new(),
                matches: vec![m],
                after: Vec::new(),
            })
            .collect();
    }

    // Group matches into ranges that should be merged
    let mut groups: Vec<Vec<LineMatch>> = Vec::new();

    for m in line_matches {
        let should_merge = groups.last().map_or(false, |group| {
            let last_line = group.last().unwrap().line_number;
            // Merge if the gap between the last match and this match
            // is within 2 * context_lines (their contexts would overlap)
            m.line_number <= last_line + 2 * self.context_lines + 1
        });

        if should_merge {
            groups.last_mut().unwrap().push(m);
        } else {
            groups.push(vec![m]);
        }
    }

    // Build context blocks from groups
    groups
        .into_iter()
        .map(|matches| {
            let first_match_line = matches.first().unwrap().line_number;
            let last_match_line = matches.last().unwrap().line_number;

            // Before context: lines before the first match
            let before_start = first_match_line.saturating_sub(self.context_lines).max(1);
            let before: Vec<ContextLine> = (before_start..first_match_line)
                .map(|ln| ContextLine {
                    line_number: ln,
                    content: line_index.line_content(content, ln).to_string(),
                })
                .collect();

            // After context: lines after the last match
            let after_end = (last_match_line + self.context_lines).min(total_lines);
            let after: Vec<ContextLine> = (last_match_line + 1..=after_end)
                .map(|ln| ContextLine {
                    line_number: ln,
                    content: line_index.line_content(content, ln).to_string(),
                })
                .collect();

            ContextBlock {
                before,
                matches,
                after,
            }
        })
        .collect()
}
```

### Step 4: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_context -v`

Expected: PASS.

---

## Task 5: Integrate `ContentVerifier` into `multi_search.rs`

**Files:**
- Modify: `ferret-indexer-core/src/multi_search.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Write the failing tests

Add to the test module in `multi_search.rs`:

```rust
use crate::search::MatchPattern;

#[test]
fn test_search_segments_with_pattern_literal() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() {\n    println!(\"hello\");\n}\n".to_vec(),
            mtime: 0,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);
    let pattern = MatchPattern::Literal("println".to_string());
    let result = search_segments_with_pattern(&snapshot, &pattern).unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].lines[0].line_number, 2);
}

#[test]
fn test_search_segments_with_pattern_regex() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"fn main() {}\nfn helper() {}\nlet x = 1;\n".to_vec(),
            mtime: 0,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);
    let pattern = MatchPattern::Regex(r"fn\s+\w+".to_string());
    let result = search_segments_with_pattern(&snapshot, &pattern).unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].lines.len(), 2); // main and helper
}

#[test]
fn test_search_segments_with_pattern_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join(".ferret_index/segments");
    std::fs::create_dir_all(&base_dir).unwrap();

    let seg = build_segment(
        &base_dir,
        SegmentId(0),
        vec![InputFile {
            path: "main.rs".to_string(),
            content: b"Hello World\nhello world\nHELLO WORLD\n".to_vec(),
            mtime: 0,
        }],
    );

    let snapshot: SegmentList = Arc::new(vec![seg]);
    let pattern = MatchPattern::LiteralCaseInsensitive("hello".to_string());
    let result = search_segments_with_pattern(&snapshot, &pattern).unwrap();
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].lines.len(), 3);
}
```

### Step 2: Run tests to verify they fail

Run: `cargo test -p ferret-indexer-core -- test_search_segments_with_pattern -v`

Expected: FAIL -- `search_segments_with_pattern` does not exist.

### Step 3: Implement `search_segments_with_pattern`

Add to `multi_search.rs`:

```rust
use crate::search::MatchPattern;
use crate::verify::ContentVerifier;

/// Search a single segment using a `MatchPattern` for verification.
fn search_single_segment_with_pattern(
    segment: &Segment,
    pattern: &MatchPattern,
    tombstones: &TombstoneSet,
) -> Result<Vec<FileMatch>, IndexError> {
    // Extract the literal text for trigram candidate filtering.
    // For regex patterns, we extract a literal prefix if possible;
    // for now, use the raw pattern string for trigram extraction.
    let trigram_query = match pattern {
        MatchPattern::Literal(s) => s.as_str(),
        MatchPattern::Regex(s) => s.as_str(),
        MatchPattern::LiteralCaseInsensitive(s) => s.as_str(),
    };

    if trigram_query.len() < 3 {
        return Ok(Vec::new());
    }

    let candidates = find_candidates(segment.trigram_reader(), trigram_query)?;
    let verifier = ContentVerifier::new(pattern.clone(), 0);
    let mut file_matches = Vec::new();

    for file_id in candidates {
        if tombstones.contains(file_id) {
            continue;
        }

        let meta = match segment.get_metadata(file_id)? {
            Some(m) => m,
            None => continue,
        };

        let content = segment
            .content_reader()
            .read_content(meta.content_offset, meta.content_len)?;

        let line_matches = verifier.verify(&content);
        if line_matches.is_empty() {
            continue;
        }

        let total_match_ranges: usize = line_matches.iter().map(|lm| lm.ranges.len()).sum();
        let line_count = meta.line_count.max(1) as f64;
        let score = (total_match_ranges as f64 / line_count).min(1.0);

        file_matches.push(FileMatch {
            file_id,
            path: PathBuf::from(&meta.path),
            language: meta.language,
            lines: line_matches,
            score,
        });
    }

    Ok(file_matches)
}

/// Search across multiple segments using a `MatchPattern`.
///
/// This is the pattern-aware version of `search_segments()`. It supports
/// literal, regex, and case-insensitive matching via `ContentVerifier`.
///
/// Behavior is identical to `search_segments()`: searches all segments,
/// filters tombstones, deduplicates by path (newest segment wins), and
/// sorts by relevance score.
pub fn search_segments_with_pattern(
    snapshot: &SegmentList,
    pattern: &MatchPattern,
) -> Result<SearchResult, IndexError> {
    let start = Instant::now();

    if snapshot.is_empty() {
        return Ok(SearchResult {
            total_count: 0,
            files: Vec::new(),
            duration: start.elapsed(),
        });
    }

    let mut merged: HashMap<PathBuf, (SegmentId, FileMatch)> = HashMap::new();

    for segment in snapshot.iter() {
        let tombstones = segment.load_tombstones()?;
        let file_matches =
            search_single_segment_with_pattern(segment, pattern, &tombstones)?;

        for fm in file_matches {
            let seg_id = segment.segment_id();
            match merged.get(&fm.path) {
                Some((existing_seg_id, _)) if *existing_seg_id >= seg_id => {}
                _ => {
                    merged.insert(fm.path.clone(), (seg_id, fm));
                }
            }
        }
    }

    let mut files: Vec<FileMatch> = merged.into_values().map(|(_, fm)| fm).collect();
    files.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total_count: usize = files.iter().map(|f| f.lines.len()).sum();

    Ok(SearchResult {
        total_count,
        files,
        duration: start.elapsed(),
    })
}
```

### Step 4: Update lib.rs exports

Add to `ferret-indexer-core/src/lib.rs`:

```rust
pub use multi_search::{search_segments, search_segments_with_pattern};
pub use verify::ContentVerifier;
```

### Step 5: Run tests to verify they pass

Run: `cargo test -p ferret-indexer-core -- test_search_segments_with_pattern -v`

Expected: PASS.

### Step 6: Verify all existing tests still pass

Run: `cargo test --workspace`

Expected: PASS -- the original `search_segments()` is unchanged and still works.

---

## Task 6: Add edge case tests and run full suite

**Files:**
- Modify: `ferret-indexer-core/src/verify.rs`

### Step 1: Add comprehensive edge case tests

Add to `verify.rs` test module:

```rust
// ---- Edge cases ----

#[test]
fn test_verify_literal_overlapping_matches() {
    // "aaa" in "aaaa" should find positions 0 and 1
    let content = b"aaaa\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("aaa".to_string()), 0);
    let result = verifier.verify(content);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].ranges.len(), 2);
    assert_eq!(result[0].ranges[0], (0, 3));
    assert_eq!(result[0].ranges[1], (1, 4));
}

#[test]
fn test_verify_regex_multiline_not_crossing_lines() {
    // Regex matches should not span across lines
    let content = b"fn main\n() {}\n";
    let verifier = ContentVerifier::new(MatchPattern::Regex(r"fn\s+\w+".to_string()), 0);
    let result = verifier.verify(content);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].line_number, 1);
    // "fn main" matches on line 1
    assert_eq!(result[0].ranges[0], (0, 7));
}

#[test]
fn test_verify_non_utf8_content() {
    // Binary-ish content with invalid UTF-8 should still work via lossy conversion
    let content = &[0xFF, 0xFE, b'\n', b'h', b'e', b'l', b'l', b'o', b'\n'];
    let verifier = ContentVerifier::new(MatchPattern::Literal("hello".to_string()), 0);
    let result = verifier.verify(content);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].line_number, 2);
}

#[test]
fn test_verify_regex_invalid_pattern() {
    // Invalid regex should return no matches (not panic)
    let content = b"fn main() {}\n";
    let verifier = ContentVerifier::new(MatchPattern::Regex("[invalid".to_string()), 0);
    let result = verifier.verify(content);
    assert!(result.is_empty());
}

#[test]
fn test_verify_literal_empty_pattern() {
    let content = b"fn main() {}\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal(String::new()), 0);
    let result = verifier.verify(content);
    assert!(result.is_empty());
}

#[test]
fn test_context_large_context_window() {
    // Context window larger than file should clamp to file boundaries
    let content = b"line 1\nMATCH\nline 3\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("MATCH".to_string()), 100);
    let blocks = verifier.verify_with_context(content);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].before.len(), 1); // only line 1
    assert_eq!(blocks[0].after.len(), 1);  // only line 3
}

#[test]
fn test_context_three_close_matches_merge() {
    let content = b"MATCH1\nMATCH2\nMATCH3\n";
    let verifier = ContentVerifier::new(MatchPattern::Literal("MATCH".to_string()), 1);
    let blocks = verifier.verify_with_context(content);
    // All 3 matches are adjacent, should merge into 1 block
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].matches.len(), 3);
}
```

### Step 2: Run all tests

Run: `cargo test --workspace`

Expected: PASS.

### Step 3: Run clippy and fmt

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --all -- --check`

Expected: PASS (no warnings, properly formatted).

---

## Summary

This plan adds candidate verification with the following components:

1. **`search.rs` types** (Task 1): `MatchPattern` enum (Literal, Regex, LiteralCaseInsensitive), `ContextLine`, `ContextBlock`.

2. **`verify.rs` module** (Tasks 2-4): `LineIndex` for O(log n) byte-offset-to-line conversion, `ContentVerifier` with `verify()` and `verify_with_context()` methods, literal/regex/case-insensitive matching, context line collection with adjacent block merging.

3. **`multi_search.rs` integration** (Task 5): `search_segments_with_pattern()` that uses `ContentVerifier` for pattern-aware search. The original `search_segments()` remains unchanged for backward compatibility.

4. **Edge cases** (Task 6): Overlapping matches, invalid regex, non-UTF8 content, empty patterns, boundary conditions.

### Interface for Downstream Consumers

- **HHC-46 (Query Parser)** produces a `MatchPattern` from the parsed query AST.
- **HHC-48 (Query Planner)** calls `search_segments_with_pattern()` with the pattern.
- **HHC-50 (Result Formatting)** consumes `ContextBlock`s from `verify_with_context()` for rich display.
- **HHC-51 (Ranking)** uses the `score` field from `FileMatch` (unchanged interface).
