//! Syntax highlighting token types and storage.
//!
//! Tokens are classified into 16 categories (4 bits) and stored as
//! run-length encoded (len, kind) pairs with a per-line offset index.
//! This enables O(1) lookup of any line's highlighting tokens.

/// 16-category token classification. Fits in 4 bits (values 0–15).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TokenKind {
    Plain = 0,
    Keyword = 1,
    String = 2,
    Comment = 3,
    Number = 4,
    Function = 5,
    Type = 6,
    Variable = 7,
    Operator = 8,
    Punctuation = 9,
    Macro = 10,
    Attribute = 11,
    Constant = 12,
    Module = 13,
    Label = 14,
    Other = 15,
}

impl TokenKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Plain,
            1 => Self::Keyword,
            2 => Self::String,
            3 => Self::Comment,
            4 => Self::Number,
            5 => Self::Function,
            6 => Self::Type,
            7 => Self::Variable,
            8 => Self::Operator,
            9 => Self::Punctuation,
            10 => Self::Macro,
            11 => Self::Attribute,
            12 => Self::Constant,
            13 => Self::Module,
            14 => Self::Label,
            _ => Self::Other,
        }
    }
}

/// A single token: byte length + kind.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Token {
    pub len: usize,
    pub kind: TokenKind,
}

/// Per-file highlight data: line offsets into a flat RLE token buffer.
#[derive(Debug, Clone)]
pub struct FileHighlight {
    /// RLE-encoded tokens: (len: u16, kind: u8) triples, concatenated for all lines.
    pub token_data: Vec<u8>,
    /// Byte offset into `token_data` where each line's tokens start.
    /// Length = number of lines. The tokens for line `i` span from
    /// `line_offsets[i]` to `line_offsets[i+1]` (or end of `token_data`).
    pub line_offsets: Vec<u32>,
}

/// Encode tokens as RLE: adjacent same-kind tokens merged,
/// each run stored as (len: u16 LE, kind: u8) = 3 bytes.
pub fn encode_rle(tokens: &[Token]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut iter = tokens.iter().peekable();
    while let Some(token) = iter.next() {
        let mut len = token.len;
        let kind = token.kind;
        // Merge adjacent same-kind
        while let Some(next) = iter.peek() {
            if next.kind == kind {
                len += next.len;
                iter.next();
            } else {
                break;
            }
        }
        // Split runs > u16::MAX
        while len > 0 {
            let chunk = len.min(u16::MAX as usize);
            buf.extend_from_slice(&(chunk as u16).to_le_bytes());
            buf.push(kind as u8);
            len -= chunk;
        }
    }
    buf
}

/// Decode RLE token data back into Token list.
pub fn decode_rle(data: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut pos = 0;
    while pos + 2 < data.len() {
        let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        let kind = TokenKind::from_u8(data[pos + 2]);
        tokens.push(Token { len, kind });
        pos += 3;
    }
    tokens
}

/// Build a `FileHighlight` from per-line token lists.
pub fn build_file_highlight(line_tokens: &[Vec<Token>]) -> FileHighlight {
    let mut token_data = Vec::new();
    let mut line_offsets = Vec::with_capacity(line_tokens.len());
    for tokens in line_tokens {
        line_offsets.push(token_data.len() as u32);
        let rle = encode_rle(tokens);
        token_data.extend_from_slice(&rle);
    }
    FileHighlight {
        token_data,
        line_offsets,
    }
}

impl FileHighlight {
    /// Get tokens for a specific line (0-indexed).
    pub fn tokens_for_line(&self, line: usize) -> Vec<Token> {
        if line >= self.line_offsets.len() {
            return Vec::new();
        }
        let start = self.line_offsets[line] as usize;
        let end = if line + 1 < self.line_offsets.len() {
            self.line_offsets[line + 1] as usize
        } else {
            self.token_data.len()
        };
        if start >= self.token_data.len() || start >= end {
            return Vec::new();
        }
        decode_rle(&self.token_data[start..end])
    }
}

use std::sync::LazyLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use crate::types::Language;

/// Shared syntect SyntaxSet — loaded once, reused across all tokenization calls.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Maximum number of lines to tokenize per file. Files exceeding this limit
/// are skipped entirely (tokenize_file returns None) to avoid spending
/// excessive CPU time on large/generated files.
const MAX_TOKENIZE_LINES: usize = 5_000;

/// Maximum file size in bytes to tokenize. Files exceeding this are skipped.
const MAX_TOKENIZE_BYTES: usize = 200_000;

/// Pre-interned scope prefixes for zero-allocation classification in the hot path.
/// Each `Scope::new()` call interns atoms once; subsequent `is_prefix_of()` checks
/// are pure integer comparisons with no allocation or locking.
struct ScopePrefixes {
    comment: Scope,
    string: Scope,
    constant_numeric: Scope,
    constant: Scope,
    keyword: Scope,
    storage_type: Scope,
    storage: Scope,
    entity_name_function: Scope,
    entity_name_type: Scope,
    entity_name_macro: Scope,
    entity_name: Scope,
    variable: Scope,
    support_function: Scope,
    support_type: Scope,
    support_macro: Scope,
    punctuation: Scope,
    meta_attribute: Scope,
}

static SCOPE_PREFIXES: LazyLock<ScopePrefixes> = LazyLock::new(|| ScopePrefixes {
    comment: Scope::new("comment").unwrap(),
    string: Scope::new("string").unwrap(),
    constant_numeric: Scope::new("constant.numeric").unwrap(),
    constant: Scope::new("constant").unwrap(),
    keyword: Scope::new("keyword").unwrap(),
    storage_type: Scope::new("storage.type").unwrap(),
    storage: Scope::new("storage").unwrap(),
    entity_name_function: Scope::new("entity.name.function").unwrap(),
    entity_name_type: Scope::new("entity.name.type").unwrap(),
    entity_name_macro: Scope::new("entity.name.macro").unwrap(),
    entity_name: Scope::new("entity.name").unwrap(),
    variable: Scope::new("variable").unwrap(),
    support_function: Scope::new("support.function").unwrap(),
    support_type: Scope::new("support.type").unwrap(),
    support_macro: Scope::new("support.macro").unwrap(),
    punctuation: Scope::new("punctuation").unwrap(),
    meta_attribute: Scope::new("meta.attribute").unwrap(),
});

/// Map our Language enum to syntect file extensions.
fn language_to_syntect_ext(lang: Language) -> Option<&'static str> {
    match lang {
        Language::Rust => Some("rs"),
        Language::Python => Some("py"),
        Language::JavaScript => Some("js"),
        Language::TypeScript => Some("ts"),
        Language::Go => Some("go"),
        Language::C => Some("c"),
        Language::Cpp => Some("cpp"),
        Language::Java => Some("java"),
        Language::Ruby => Some("rb"),
        Language::Swift => Some("swift"),
        Language::Kotlin => Some("kt"),
        Language::Scala => Some("scala"),
        Language::Haskell => Some("hs"),
        Language::Lua => Some("lua"),
        Language::Perl => Some("pl"),
        Language::CSharp => Some("cs"),
        Language::Shell => Some("sh"),
        Language::Html => Some("html"),
        Language::Css => Some("css"),
        Language::Sql => Some("sql"),
        Language::Json => Some("json"),
        Language::Xml => Some("xml"),
        Language::Yaml => Some("yaml"),
        Language::Toml => Some("toml"),
        Language::Markdown => Some("md"),
        Language::Protobuf => Some("proto"),
        Language::OCaml => Some("ml"),
        Language::Clojure => Some("clj"),
        Language::R => Some("r"),
        Language::Groovy => Some("groovy"),
        Language::Dart => Some("dart"),
        Language::Elixir => Some("ex"),
        Language::Erlang => Some("erl"),
        Language::Scss => Some("scss"),
        Language::Less => Some("less"),
        Language::PowerShell => Some("ps1"),
        Language::Julia => Some("jl"),
        _ => None,
    }
}

/// Classify a syntect scope stack into our 16-category TokenKind.
///
/// Uses pre-interned [`Scope`] prefixes and [`Scope::is_prefix_of`] for
/// zero-allocation, lock-free classification in the hot path.
fn classify_scope(stack: &ScopeStack) -> TokenKind {
    let p = &*SCOPE_PREFIXES;
    for &scope in stack.as_slice().iter().rev() {
        if p.comment.is_prefix_of(scope) {
            return TokenKind::Comment;
        }
        if p.string.is_prefix_of(scope) {
            return TokenKind::String;
        }
        if p.constant_numeric.is_prefix_of(scope) {
            return TokenKind::Number;
        }
        if p.constant.is_prefix_of(scope) {
            return TokenKind::Constant;
        }
        if p.keyword.is_prefix_of(scope) {
            return TokenKind::Keyword;
        }
        if p.storage_type.is_prefix_of(scope) {
            return TokenKind::Type;
        }
        if p.storage.is_prefix_of(scope) {
            return TokenKind::Keyword;
        }
        if p.entity_name_function.is_prefix_of(scope) {
            return TokenKind::Function;
        }
        if p.entity_name_type.is_prefix_of(scope) {
            return TokenKind::Type;
        }
        if p.entity_name_macro.is_prefix_of(scope) {
            return TokenKind::Macro;
        }
        if p.entity_name.is_prefix_of(scope) {
            return TokenKind::Function;
        }
        if p.variable.is_prefix_of(scope) {
            return TokenKind::Variable;
        }
        if p.support_function.is_prefix_of(scope) {
            return TokenKind::Function;
        }
        if p.support_type.is_prefix_of(scope) {
            return TokenKind::Type;
        }
        if p.support_macro.is_prefix_of(scope) {
            return TokenKind::Macro;
        }
        if p.punctuation.is_prefix_of(scope) {
            return TokenKind::Punctuation;
        }
        if p.meta_attribute.is_prefix_of(scope) {
            return TokenKind::Attribute;
        }
    }
    TokenKind::Plain
}

/// Tokenize file content into per-line token lists using syntect.
///
/// Returns `Some(line_tokens)` if the language is recognized by syntect,
/// or `None` if unsupported. Callers should skip highlight storage for `None`.
pub fn tokenize_file(content: &[u8], language: Language) -> Option<Vec<Vec<Token>>> {
    // Skip large files to avoid excessive CPU time on generated/minified content.
    if content.len() > MAX_TOKENIZE_BYTES {
        return None;
    }
    let line_count = content.iter().filter(|&&b| b == b'\n').count();
    if line_count > MAX_TOKENIZE_LINES {
        return None;
    }

    let ext = language_to_syntect_ext(language)?;
    let ss = &*SYNTAX_SET;
    let syntax = ss.find_syntax_by_extension(ext)?;
    let text = String::from_utf8_lossy(content);

    let mut parse_state = ParseState::new(syntax);
    let mut scope_stack = ScopeStack::new();
    let mut all_line_tokens = Vec::new();

    for line in syntect::util::LinesWithEndings::from(&text) {
        let ops = match parse_state.parse_line(line, ss) {
            Ok(ops) => ops,
            Err(_) => {
                // Parse error — emit entire line as Plain
                all_line_tokens.push(vec![Token {
                    len: line.len(),
                    kind: TokenKind::Plain,
                }]);
                continue;
            }
        };

        let mut line_tokens = Vec::new();
        let mut pos = 0;

        for (offset, op) in &ops {
            let offset = *offset;
            if offset > pos {
                let kind = classify_scope(&scope_stack);
                line_tokens.push(Token {
                    len: offset - pos,
                    kind,
                });
            }
            let _ = scope_stack.apply(op);
            pos = offset;
        }
        // Remaining text on line
        if pos < line.len() {
            let kind = classify_scope(&scope_stack);
            line_tokens.push(Token {
                len: line.len() - pos,
                kind,
            });
        }
        all_line_tokens.push(line_tokens);
    }

    Some(all_line_tokens)
}

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use memmap2::Mmap;

/// Serialize a `FileHighlight` into a binary block (before zstd compression).
///
/// Format:
/// ```text
/// [line_count: u32 LE]
/// [line_offsets: u32 LE × line_count]
/// [token_data: u8...]
/// ```
pub(crate) fn serialize_file_highlight(fh: &FileHighlight) -> Vec<u8> {
    let line_count = fh.line_offsets.len() as u32;
    let header_size = 4 + fh.line_offsets.len() * 4;
    let mut buf = Vec::with_capacity(header_size + fh.token_data.len());
    buf.extend_from_slice(&line_count.to_le_bytes());
    for &off in &fh.line_offsets {
        buf.extend_from_slice(&off.to_le_bytes());
    }
    buf.extend_from_slice(&fh.token_data);
    buf
}

fn deserialize_file_highlight(data: &[u8]) -> Option<FileHighlight> {
    if data.len() < 4 {
        return None;
    }
    let line_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let offsets_end = 4 + line_count * 4;
    if data.len() < offsets_end {
        return None;
    }
    let mut line_offsets = Vec::with_capacity(line_count);
    for i in 0..line_count {
        let base = 4 + i * 4;
        let off = u32::from_le_bytes([data[base], data[base + 1], data[base + 2], data[base + 3]]);
        line_offsets.push(off);
    }
    let token_data = data[offsets_end..].to_vec();
    Some(FileHighlight {
        token_data,
        line_offsets,
    })
}

/// Writer for the per-segment `highlights.zst` file.
///
/// Mirrors `ContentStoreWriter`: appends independently zstd-compressed blocks,
/// returns `(offset, compressed_len, line_count)` per file for metadata storage.
pub struct HighlightStoreWriter {
    writer: BufWriter<File>,
    current_offset: u64,
}

impl HighlightStoreWriter {
    pub fn new(path: &Path) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            current_offset: 0,
        })
    }

    /// Add a file's highlight data. Returns `(offset, compressed_len, line_count)`.
    pub fn add_file(&mut self, fh: &FileHighlight) -> std::io::Result<(u64, u32, u32)> {
        let serialized = serialize_file_highlight(fh);
        let compressed = zstd::bulk::compress(&serialized, 3).map_err(std::io::Error::other)?;

        let offset = self.current_offset;
        let compressed_len: u32 = compressed
            .len()
            .try_into()
            .map_err(|_| std::io::Error::other("compressed highlight block exceeds u32::MAX"))?;
        let line_count = fh.line_offsets.len() as u32;

        self.writer.write_all(&compressed)?;
        self.current_offset += compressed_len as u64;

        Ok((offset, compressed_len, line_count))
    }

    /// Add pre-compressed highlight data (for compaction copy-through).
    pub fn add_raw(&mut self, compressed: &[u8]) -> std::io::Result<(u64, u32)> {
        let offset = self.current_offset;
        let compressed_len: u32 = compressed
            .len()
            .try_into()
            .map_err(|_| std::io::Error::other("compressed highlight block exceeds u32::MAX"))?;
        self.writer.write_all(compressed)?;
        self.current_offset += compressed_len as u64;
        Ok((offset, compressed_len))
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// Reader for the per-segment `highlights.zst` file.
pub struct HighlightStoreReader {
    mmap: Mmap,
}

impl HighlightStoreReader {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self { mmap })
    }

    /// Read a file's highlight data given its offset, compressed length, and line count.
    pub fn read_file(
        &self,
        offset: u64,
        compressed_len: u32,
        _line_count: u32,
    ) -> Result<FileHighlight, crate::IndexError> {
        let start = offset as usize;
        let end = start + compressed_len as usize;
        if end > self.mmap.len() {
            return Err(crate::IndexError::IndexCorruption(
                "highlight block out of bounds".to_string(),
            ));
        }
        let compressed = &self.mmap[start..end];
        let decompressed = zstd::bulk::decompress(compressed, 10 * 1024 * 1024)
            .map_err(|e| crate::IndexError::IndexCorruption(format!("highlight zstd: {e}")))?;
        deserialize_file_highlight(&decompressed).ok_or_else(|| {
            crate::IndexError::IndexCorruption("malformed highlight block".to_string())
        })
    }

    /// Read raw compressed bytes for a file (for compaction copy-through).
    pub fn read_raw(&self, offset: u64, compressed_len: u32) -> Result<Vec<u8>, crate::IndexError> {
        let start = offset as usize;
        let end = start + compressed_len as usize;
        if end > self.mmap.len() {
            return Err(crate::IndexError::IndexCorruption(
                "highlight block out of bounds".to_string(),
            ));
        }
        Ok(self.mmap[start..end].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_kind_roundtrip() {
        for v in 0..=15u8 {
            let kind = TokenKind::from_u8(v);
            assert_eq!(kind as u8, v);
        }
        // Out of range maps to Other
        assert_eq!(TokenKind::from_u8(255), TokenKind::Other);
    }

    #[test]
    fn test_rle_encode_decode_roundtrip() {
        let tokens = vec![
            Token {
                len: 2,
                kind: TokenKind::Keyword,
            },
            Token {
                len: 1,
                kind: TokenKind::Plain,
            },
            Token {
                len: 4,
                kind: TokenKind::Function,
            },
            Token {
                len: 2,
                kind: TokenKind::Punctuation,
            },
        ];
        let encoded = encode_rle(&tokens);
        let decoded = decode_rle(&encoded);
        assert_eq!(tokens, decoded);
    }

    #[test]
    fn test_rle_merges_adjacent_same_kind() {
        let tokens = vec![
            Token {
                len: 3,
                kind: TokenKind::Plain,
            },
            Token {
                len: 5,
                kind: TokenKind::Plain,
            },
        ];
        let encoded = encode_rle(&tokens);
        let decoded = decode_rle(&encoded);
        // Should merge into a single (8, Plain)
        assert_eq!(
            decoded,
            vec![Token {
                len: 8,
                kind: TokenKind::Plain
            }]
        );
    }

    #[test]
    fn test_file_highlight_line_lookup() {
        let line0 = vec![
            Token {
                len: 2,
                kind: TokenKind::Keyword,
            },
            Token {
                len: 5,
                kind: TokenKind::Function,
            },
        ];
        let line1 = vec![Token {
            len: 10,
            kind: TokenKind::Comment,
        }];
        let fh = build_file_highlight(&[line0.clone(), line1.clone()]);
        assert_eq!(fh.tokens_for_line(0), line0);
        assert_eq!(fh.tokens_for_line(1), line1);
        assert_eq!(fh.tokens_for_line(2), vec![]); // out of bounds
    }

    #[test]
    fn test_highlight_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("highlights.zst");

        // Build highlight data for two files
        let fh0 = build_file_highlight(&[
            vec![
                Token {
                    len: 2,
                    kind: TokenKind::Keyword,
                },
                Token {
                    len: 5,
                    kind: TokenKind::Plain,
                },
            ],
            vec![Token {
                len: 10,
                kind: TokenKind::String,
            }],
        ]);
        let fh1 = build_file_highlight(&[vec![Token {
            len: 3,
            kind: TokenKind::Comment,
        }]]);

        // Write
        let mut writer = HighlightStoreWriter::new(&path).unwrap();
        let (off0, len0, lines0) = writer.add_file(&fh0).unwrap();
        let (off1, len1, lines1) = writer.add_file(&fh1).unwrap();
        writer.finish().unwrap();

        assert_eq!(lines0, 2);
        assert_eq!(lines1, 1);

        // Read
        let reader = HighlightStoreReader::open(&path).unwrap();
        let read0 = reader.read_file(off0, len0, lines0).unwrap();
        let read1 = reader.read_file(off1, len1, lines1).unwrap();

        assert_eq!(read0.tokens_for_line(0), fh0.tokens_for_line(0));
        assert_eq!(read0.tokens_for_line(1), fh0.tokens_for_line(1));
        assert_eq!(read1.tokens_for_line(0), fh1.tokens_for_line(0));
    }

    #[test]
    fn test_tokenize_rust_file() {
        let code = b"fn main() {\n    let x = 42;\n}\n";
        let tokens = tokenize_file(code, Language::Rust).unwrap();
        // Should have 3 lines of tokens
        assert_eq!(tokens.len(), 3);
        // First line should start with a token for "fn" (syntect classifies it as storage.type)
        assert_eq!(tokens[0][0].len, 2); // "fn"
        assert!(
            tokens[0][0].kind == TokenKind::Keyword || tokens[0][0].kind == TokenKind::Type,
            "expected Keyword or Type for 'fn', got {:?}",
            tokens[0][0].kind
        );
        // Verify we get non-trivial tokenization (not all Plain)
        let has_non_plain = tokens.iter().flatten().any(|t| t.kind != TokenKind::Plain);
        assert!(has_non_plain, "expected at least some non-Plain tokens");
    }

    #[test]
    fn test_tokenize_unknown_language_returns_none() {
        let code = b"hello world\n";
        assert!(tokenize_file(code, Language::Unknown).is_none());
    }
}
