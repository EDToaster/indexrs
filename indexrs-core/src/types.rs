//! Core identifier types and enums for indexrs.
//!
//! This module defines the fundamental types used throughout the indexing system:
//! file identifiers, trigrams for index lookups, segment identifiers, language
//! classification, and symbol kinds.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Unique identifier for an indexed file within the index.
///
/// File IDs are assigned sequentially during indexing and used as compact
/// references in posting lists and metadata tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileId(pub u32);

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A 3-byte n-gram used for trigram index lookups.
///
/// Trigrams are the fundamental unit of the search index. Every 3-byte sequence
/// in indexed files is recorded as a trigram, enabling fast substring and regex
/// search via posting list intersection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Trigram(pub [u8; 3]);

impl Trigram {
    /// Construct a trigram from three individual bytes.
    pub fn from_bytes(a: u8, b: u8, c: u8) -> Self {
        Trigram([a, b, c])
    }

    /// Convert the trigram to a `u32` value for use as a hash key or array index.
    ///
    /// The encoding packs the three bytes into the lower 24 bits:
    /// `(byte0 << 16) | (byte1 << 8) | byte2`
    pub fn to_u32(self) -> u32 {
        (self.0[0] as u32) << 16 | (self.0[1] as u32) << 8 | self.0[2] as u32
    }
}

impl fmt::Display for Trigram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            if b.is_ascii_graphic() || b == b' ' {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        Ok(())
    }
}

/// Identifier for an index segment.
///
/// The index is composed of multiple immutable segments, each containing a
/// subset of indexed files. Segments are created on updates and periodically
/// compacted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SegmentId(pub u32);

impl fmt::Display for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Programming language classification for indexed files.
///
/// Language detection is used for `language:rust` style query filters and for
/// selecting the appropriate tree-sitter grammar for symbol extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
    C,
    Cpp,
    Java,
    Ruby,
    Shell,
    Markdown,
    Yaml,
    Toml,
    Json,
    Xml,
    Html,
    Css,
    Scss,
    Sass,
    Sql,
    Protobuf,
    Dockerfile,
    Hcl,
    Kotlin,
    Swift,
    Scala,
    Elixir,
    Erlang,
    Haskell,
    OCaml,
    Lua,
    Perl,
    R,
    Dart,
    Zig,
    Nix,
    Unknown,
}

impl Language {
    /// Convert this language to its `u16` representation for binary serialization.
    ///
    /// Each variant maps to a fixed numeric value. `Unknown` maps to `0xFFFF`.
    pub fn to_u16(self) -> u16 {
        match self {
            Language::Rust => 0,
            Language::Python => 1,
            Language::TypeScript => 2,
            Language::JavaScript => 3,
            Language::Go => 4,
            Language::C => 5,
            Language::Cpp => 6,
            Language::Java => 7,
            Language::Ruby => 8,
            Language::Shell => 9,
            Language::Markdown => 10,
            Language::Yaml => 11,
            Language::Toml => 12,
            Language::Json => 13,
            Language::Xml => 14,
            Language::Html => 15,
            Language::Css => 16,
            Language::Scss => 17,
            Language::Sass => 18,
            Language::Sql => 19,
            Language::Protobuf => 20,
            Language::Dockerfile => 21,
            Language::Hcl => 22,
            Language::Kotlin => 23,
            Language::Swift => 24,
            Language::Scala => 25,
            Language::Elixir => 26,
            Language::Erlang => 27,
            Language::Haskell => 28,
            Language::OCaml => 29,
            Language::Lua => 30,
            Language::Perl => 31,
            Language::R => 32,
            Language::Dart => 33,
            Language::Zig => 34,
            Language::Nix => 35,
            Language::Unknown => 0xFFFF,
        }
    }

    /// Reconstruct a `Language` from its `u16` representation.
    ///
    /// Unrecognized values map to `Language::Unknown`.
    pub fn from_u16(v: u16) -> Language {
        match v {
            0 => Language::Rust,
            1 => Language::Python,
            2 => Language::TypeScript,
            3 => Language::JavaScript,
            4 => Language::Go,
            5 => Language::C,
            6 => Language::Cpp,
            7 => Language::Java,
            8 => Language::Ruby,
            9 => Language::Shell,
            10 => Language::Markdown,
            11 => Language::Yaml,
            12 => Language::Toml,
            13 => Language::Json,
            14 => Language::Xml,
            15 => Language::Html,
            16 => Language::Css,
            17 => Language::Scss,
            18 => Language::Sass,
            19 => Language::Sql,
            20 => Language::Protobuf,
            21 => Language::Dockerfile,
            22 => Language::Hcl,
            23 => Language::Kotlin,
            24 => Language::Swift,
            25 => Language::Scala,
            26 => Language::Elixir,
            27 => Language::Erlang,
            28 => Language::Haskell,
            29 => Language::OCaml,
            30 => Language::Lua,
            31 => Language::Perl,
            32 => Language::R,
            33 => Language::Dart,
            34 => Language::Zig,
            35 => Language::Nix,
            _ => Language::Unknown,
        }
    }

    /// Detect language from a file extension string (without the leading dot).
    ///
    /// Returns `Language::Unknown` for unrecognized extensions.
    ///
    /// # Examples
    ///
    /// ```
    /// use indexrs_core::Language;
    ///
    /// assert_eq!(Language::from_extension("rs"), Language::Rust);
    /// assert_eq!(Language::from_extension("py"), Language::Python);
    /// assert_eq!(Language::from_extension("xyz"), Language::Unknown);
    /// ```
    /// Detect language from a file path.
    ///
    /// Extracts the file extension and delegates to [`from_extension`](Language::from_extension).
    /// Also handles special filenames like `Dockerfile` that have no extension.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::Path;
    /// use indexrs_core::Language;
    ///
    /// assert_eq!(Language::from_path(Path::new("src/main.rs")), Language::Rust);
    /// assert_eq!(Language::from_path(Path::new("Dockerfile")), Language::Dockerfile);
    /// ```
    pub fn from_path(path: &Path) -> Language {
        // Check filename-based detection first (e.g., Dockerfile)
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == "Dockerfile" || name.starts_with("Dockerfile.") {
                return Language::Dockerfile;
            }
        }

        // Fall back to extension-based detection
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => Language::from_extension(ext),
            None => Language::Unknown,
        }
    }

    pub fn from_extension(ext: &str) -> Language {
        match ext {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "ts" | "tsx" => Language::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "go" => Language::Go,
            "c" | "h" => Language::C,
            "cpp" | "cxx" | "cc" | "hpp" | "hxx" | "hh" => Language::Cpp,
            "java" => Language::Java,
            "rb" => Language::Ruby,
            "sh" | "bash" | "zsh" | "fish" => Language::Shell,
            "md" | "markdown" => Language::Markdown,
            "yml" | "yaml" => Language::Yaml,
            "toml" => Language::Toml,
            "json" | "jsonc" => Language::Json,
            "xml" | "xsl" | "xslt" => Language::Xml,
            "html" | "htm" => Language::Html,
            "css" => Language::Css,
            "scss" => Language::Scss,
            "sass" => Language::Sass,
            "sql" => Language::Sql,
            "proto" => Language::Protobuf,
            "tf" | "hcl" | "tfvars" => Language::Hcl,
            "kt" | "kts" => Language::Kotlin,
            "swift" => Language::Swift,
            "scala" | "sc" => Language::Scala,
            "ex" | "exs" => Language::Elixir,
            "erl" | "hrl" => Language::Erlang,
            "hs" | "lhs" => Language::Haskell,
            "ml" | "mli" => Language::OCaml,
            "lua" => Language::Lua,
            "pl" | "pm" => Language::Perl,
            "r" | "R" => Language::R,
            "dart" => Language::Dart,
            "zig" => Language::Zig,
            "nix" => Language::Nix,
            _ => Language::Unknown,
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Language::Rust => write!(f, "Rust"),
            Language::Python => write!(f, "Python"),
            Language::TypeScript => write!(f, "TypeScript"),
            Language::JavaScript => write!(f, "JavaScript"),
            Language::Go => write!(f, "Go"),
            Language::C => write!(f, "C"),
            Language::Cpp => write!(f, "C++"),
            Language::Java => write!(f, "Java"),
            Language::Ruby => write!(f, "Ruby"),
            Language::Shell => write!(f, "Shell"),
            Language::Markdown => write!(f, "Markdown"),
            Language::Yaml => write!(f, "YAML"),
            Language::Toml => write!(f, "TOML"),
            Language::Json => write!(f, "JSON"),
            Language::Xml => write!(f, "XML"),
            Language::Html => write!(f, "HTML"),
            Language::Css => write!(f, "CSS"),
            Language::Scss => write!(f, "SCSS"),
            Language::Sass => write!(f, "Sass"),
            Language::Sql => write!(f, "SQL"),
            Language::Protobuf => write!(f, "Protobuf"),
            Language::Dockerfile => write!(f, "Dockerfile"),
            Language::Hcl => write!(f, "HCL"),
            Language::Kotlin => write!(f, "Kotlin"),
            Language::Swift => write!(f, "Swift"),
            Language::Scala => write!(f, "Scala"),
            Language::Elixir => write!(f, "Elixir"),
            Language::Erlang => write!(f, "Erlang"),
            Language::Haskell => write!(f, "Haskell"),
            Language::OCaml => write!(f, "OCaml"),
            Language::Lua => write!(f, "Lua"),
            Language::Perl => write!(f, "Perl"),
            Language::R => write!(f, "R"),
            Language::Dart => write!(f, "Dart"),
            Language::Zig => write!(f, "Zig"),
            Language::Nix => write!(f, "Nix"),
            Language::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Kind of symbol extracted from source code via tree-sitter.
///
/// Used in the symbol index to classify symbol definitions, enabling
/// `symbol:parse_query` style searches filtered by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Struct,
    Trait,
    Enum,
    Interface,
    Class,
    Method,
    Constant,
    Variable,
    Type,
    Module,
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SymbolKind::Function => write!(f, "Function"),
            SymbolKind::Struct => write!(f, "Struct"),
            SymbolKind::Trait => write!(f, "Trait"),
            SymbolKind::Enum => write!(f, "Enum"),
            SymbolKind::Interface => write!(f, "Interface"),
            SymbolKind::Class => write!(f, "Class"),
            SymbolKind::Method => write!(f, "Method"),
            SymbolKind::Constant => write!(f, "Constant"),
            SymbolKind::Variable => write!(f, "Variable"),
            SymbolKind::Type => write!(f, "Type"),
            SymbolKind::Module => write!(f, "Module"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_id_display() {
        assert_eq!(FileId(42).to_string(), "42");
        assert_eq!(FileId(0).to_string(), "0");
        assert_eq!(FileId(u32::MAX).to_string(), "4294967295");
    }

    #[test]
    fn test_file_id_equality_and_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(FileId(1));
        set.insert(FileId(2));
        set.insert(FileId(1)); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_trigram_from_bytes() {
        let t = Trigram::from_bytes(b'a', b'b', b'c');
        assert_eq!(t, Trigram([b'a', b'b', b'c']));
    }

    #[test]
    fn test_trigram_to_u32() {
        let t = Trigram::from_bytes(b'a', b'b', b'c');
        // 'a' = 0x61, 'b' = 0x62, 'c' = 0x63
        // (0x61 << 16) | (0x62 << 8) | 0x63 = 6382179
        let expected = (0x61u32 << 16) | (0x62u32 << 8) | 0x63u32;
        assert_eq!(t.to_u32(), expected);
        assert_eq!(t.to_u32(), 6_382_179);
    }

    #[test]
    fn test_trigram_to_u32_zero() {
        let t = Trigram::from_bytes(0, 0, 0);
        assert_eq!(t.to_u32(), 0);
    }

    #[test]
    fn test_trigram_to_u32_max() {
        let t = Trigram::from_bytes(0xFF, 0xFF, 0xFF);
        assert_eq!(t.to_u32(), 0x00FF_FFFF);
    }

    #[test]
    fn test_trigram_display_printable() {
        let t = Trigram::from_bytes(b'f', b'o', b'o');
        assert_eq!(t.to_string(), "foo");
    }

    #[test]
    fn test_trigram_display_non_printable() {
        let t = Trigram::from_bytes(0x00, b'a', 0xFF);
        assert_eq!(t.to_string(), "\\x00a\\xff");
    }

    #[test]
    fn test_segment_id_display() {
        assert_eq!(SegmentId(1).to_string(), "1");
        assert_eq!(SegmentId(9999).to_string(), "9999");
    }

    #[test]
    fn test_language_from_extension() {
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("pyi"), Language::Python);
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("tsx"), Language::TypeScript);
        assert_eq!(Language::from_extension("js"), Language::JavaScript);
        assert_eq!(Language::from_extension("jsx"), Language::JavaScript);
        assert_eq!(Language::from_extension("mjs"), Language::JavaScript);
        assert_eq!(Language::from_extension("cjs"), Language::JavaScript);
        assert_eq!(Language::from_extension("go"), Language::Go);
        assert_eq!(Language::from_extension("c"), Language::C);
        assert_eq!(Language::from_extension("h"), Language::C);
        assert_eq!(Language::from_extension("cpp"), Language::Cpp);
        assert_eq!(Language::from_extension("cxx"), Language::Cpp);
        assert_eq!(Language::from_extension("cc"), Language::Cpp);
        assert_eq!(Language::from_extension("hpp"), Language::Cpp);
        assert_eq!(Language::from_extension("java"), Language::Java);
        assert_eq!(Language::from_extension("rb"), Language::Ruby);
        assert_eq!(Language::from_extension("sh"), Language::Shell);
        assert_eq!(Language::from_extension("bash"), Language::Shell);
        assert_eq!(Language::from_extension("zsh"), Language::Shell);
        assert_eq!(Language::from_extension("fish"), Language::Shell);
        assert_eq!(Language::from_extension("md"), Language::Markdown);
        assert_eq!(Language::from_extension("markdown"), Language::Markdown);
        assert_eq!(Language::from_extension("unknown"), Language::Unknown);
        assert_eq!(Language::from_extension(""), Language::Unknown);
        assert_eq!(Language::from_extension("xyz"), Language::Unknown);
    }

    #[test]
    fn test_language_u16_roundtrip() {
        let languages = [
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::JavaScript,
            Language::Go,
            Language::C,
            Language::Cpp,
            Language::Java,
            Language::Ruby,
            Language::Shell,
            Language::Markdown,
            Language::Yaml,
            Language::Toml,
            Language::Json,
            Language::Xml,
            Language::Html,
            Language::Css,
            Language::Scss,
            Language::Sass,
            Language::Sql,
            Language::Protobuf,
            Language::Dockerfile,
            Language::Hcl,
            Language::Kotlin,
            Language::Swift,
            Language::Scala,
            Language::Elixir,
            Language::Erlang,
            Language::Haskell,
            Language::OCaml,
            Language::Lua,
            Language::Perl,
            Language::R,
            Language::Dart,
            Language::Zig,
            Language::Nix,
            Language::Unknown,
        ];
        for lang in languages {
            assert_eq!(Language::from_u16(lang.to_u16()), lang);
        }
    }

    #[test]
    fn test_language_u16_known_values() {
        assert_eq!(Language::Rust.to_u16(), 0);
        assert_eq!(Language::Python.to_u16(), 1);
        assert_eq!(Language::Unknown.to_u16(), 0xFFFF);
    }

    #[test]
    fn test_language_from_u16_unknown() {
        assert_eq!(Language::from_u16(999), Language::Unknown);
        assert_eq!(Language::from_u16(0xFFFE), Language::Unknown);
    }

    #[test]
    fn test_language_display() {
        assert_eq!(Language::Rust.to_string(), "Rust");
        assert_eq!(Language::Python.to_string(), "Python");
        assert_eq!(Language::TypeScript.to_string(), "TypeScript");
        assert_eq!(Language::JavaScript.to_string(), "JavaScript");
        assert_eq!(Language::Go.to_string(), "Go");
        assert_eq!(Language::C.to_string(), "C");
        assert_eq!(Language::Cpp.to_string(), "C++");
        assert_eq!(Language::Java.to_string(), "Java");
        assert_eq!(Language::Ruby.to_string(), "Ruby");
        assert_eq!(Language::Shell.to_string(), "Shell");
        assert_eq!(Language::Markdown.to_string(), "Markdown");
        assert_eq!(Language::Unknown.to_string(), "Unknown");
    }

    #[test]
    fn test_new_language_u16_roundtrip() {
        let new_languages = [
            (Language::Yaml, 11u16),
            (Language::Toml, 12),
            (Language::Json, 13),
            (Language::Xml, 14),
            (Language::Html, 15),
            (Language::Css, 16),
            (Language::Scss, 17),
            (Language::Sass, 18),
            (Language::Sql, 19),
            (Language::Protobuf, 20),
            (Language::Dockerfile, 21),
            (Language::Hcl, 22),
            (Language::Kotlin, 23),
            (Language::Swift, 24),
            (Language::Scala, 25),
            (Language::Elixir, 26),
            (Language::Erlang, 27),
            (Language::Haskell, 28),
            (Language::OCaml, 29),
            (Language::Lua, 30),
            (Language::Perl, 31),
            (Language::R, 32),
            (Language::Dart, 33),
            (Language::Zig, 34),
            (Language::Nix, 35),
        ];
        for (lang, expected_u16) in new_languages {
            assert_eq!(lang.to_u16(), expected_u16, "{lang} to_u16");
            assert_eq!(Language::from_u16(expected_u16), lang, "from_u16({expected_u16})");
        }
    }

    #[test]
    fn test_backward_compat_u16_values() {
        // Existing values MUST NOT change
        assert_eq!(Language::Rust.to_u16(), 0);
        assert_eq!(Language::Python.to_u16(), 1);
        assert_eq!(Language::TypeScript.to_u16(), 2);
        assert_eq!(Language::JavaScript.to_u16(), 3);
        assert_eq!(Language::Go.to_u16(), 4);
        assert_eq!(Language::C.to_u16(), 5);
        assert_eq!(Language::Cpp.to_u16(), 6);
        assert_eq!(Language::Java.to_u16(), 7);
        assert_eq!(Language::Ruby.to_u16(), 8);
        assert_eq!(Language::Shell.to_u16(), 9);
        assert_eq!(Language::Markdown.to_u16(), 10);
        assert_eq!(Language::Unknown.to_u16(), 0xFFFF);
    }

    #[test]
    fn test_new_language_from_extension() {
        // YAML
        assert_eq!(Language::from_extension("yml"), Language::Yaml);
        assert_eq!(Language::from_extension("yaml"), Language::Yaml);
        // TOML
        assert_eq!(Language::from_extension("toml"), Language::Toml);
        // JSON
        assert_eq!(Language::from_extension("json"), Language::Json);
        assert_eq!(Language::from_extension("jsonc"), Language::Json);
        // XML
        assert_eq!(Language::from_extension("xml"), Language::Xml);
        assert_eq!(Language::from_extension("xsl"), Language::Xml);
        assert_eq!(Language::from_extension("xslt"), Language::Xml);
        // HTML
        assert_eq!(Language::from_extension("html"), Language::Html);
        assert_eq!(Language::from_extension("htm"), Language::Html);
        // CSS
        assert_eq!(Language::from_extension("css"), Language::Css);
        // SCSS
        assert_eq!(Language::from_extension("scss"), Language::Scss);
        // Sass
        assert_eq!(Language::from_extension("sass"), Language::Sass);
        // SQL
        assert_eq!(Language::from_extension("sql"), Language::Sql);
        // Protobuf
        assert_eq!(Language::from_extension("proto"), Language::Protobuf);
        // HCL / Terraform
        assert_eq!(Language::from_extension("tf"), Language::Hcl);
        assert_eq!(Language::from_extension("hcl"), Language::Hcl);
        assert_eq!(Language::from_extension("tfvars"), Language::Hcl);
        // Kotlin
        assert_eq!(Language::from_extension("kt"), Language::Kotlin);
        assert_eq!(Language::from_extension("kts"), Language::Kotlin);
        // Swift
        assert_eq!(Language::from_extension("swift"), Language::Swift);
        // Scala
        assert_eq!(Language::from_extension("scala"), Language::Scala);
        assert_eq!(Language::from_extension("sc"), Language::Scala);
        // Elixir
        assert_eq!(Language::from_extension("ex"), Language::Elixir);
        assert_eq!(Language::from_extension("exs"), Language::Elixir);
        // Erlang
        assert_eq!(Language::from_extension("erl"), Language::Erlang);
        assert_eq!(Language::from_extension("hrl"), Language::Erlang);
        // Haskell
        assert_eq!(Language::from_extension("hs"), Language::Haskell);
        assert_eq!(Language::from_extension("lhs"), Language::Haskell);
        // OCaml
        assert_eq!(Language::from_extension("ml"), Language::OCaml);
        assert_eq!(Language::from_extension("mli"), Language::OCaml);
        // Lua
        assert_eq!(Language::from_extension("lua"), Language::Lua);
        // Perl
        assert_eq!(Language::from_extension("pl"), Language::Perl);
        assert_eq!(Language::from_extension("pm"), Language::Perl);
        // R
        assert_eq!(Language::from_extension("r"), Language::R);
        assert_eq!(Language::from_extension("R"), Language::R);
        // Dart
        assert_eq!(Language::from_extension("dart"), Language::Dart);
        // Zig
        assert_eq!(Language::from_extension("zig"), Language::Zig);
        // Nix
        assert_eq!(Language::from_extension("nix"), Language::Nix);
    }

    #[test]
    fn test_new_language_display() {
        assert_eq!(Language::Yaml.to_string(), "YAML");
        assert_eq!(Language::Toml.to_string(), "TOML");
        assert_eq!(Language::Json.to_string(), "JSON");
        assert_eq!(Language::Xml.to_string(), "XML");
        assert_eq!(Language::Html.to_string(), "HTML");
        assert_eq!(Language::Css.to_string(), "CSS");
        assert_eq!(Language::Scss.to_string(), "SCSS");
        assert_eq!(Language::Sass.to_string(), "Sass");
        assert_eq!(Language::Sql.to_string(), "SQL");
        assert_eq!(Language::Protobuf.to_string(), "Protobuf");
        assert_eq!(Language::Dockerfile.to_string(), "Dockerfile");
        assert_eq!(Language::Hcl.to_string(), "HCL");
        assert_eq!(Language::Kotlin.to_string(), "Kotlin");
        assert_eq!(Language::Swift.to_string(), "Swift");
        assert_eq!(Language::Scala.to_string(), "Scala");
        assert_eq!(Language::Elixir.to_string(), "Elixir");
        assert_eq!(Language::Erlang.to_string(), "Erlang");
        assert_eq!(Language::Haskell.to_string(), "Haskell");
        assert_eq!(Language::OCaml.to_string(), "OCaml");
        assert_eq!(Language::Lua.to_string(), "Lua");
        assert_eq!(Language::Perl.to_string(), "Perl");
        assert_eq!(Language::R.to_string(), "R");
        assert_eq!(Language::Dart.to_string(), "Dart");
        assert_eq!(Language::Zig.to_string(), "Zig");
        assert_eq!(Language::Nix.to_string(), "Nix");
    }

    #[test]
    fn test_from_path() {
        use std::path::Path;

        assert_eq!(Language::from_path(Path::new("src/main.rs")), Language::Rust);
        assert_eq!(Language::from_path(Path::new("lib.py")), Language::Python);
        assert_eq!(Language::from_path(Path::new("deep/nested/app.tsx")), Language::TypeScript);
        assert_eq!(Language::from_path(Path::new("config.yml")), Language::Yaml);
        assert_eq!(Language::from_path(Path::new("Cargo.toml")), Language::Toml);
        assert_eq!(Language::from_path(Path::new("data.json")), Language::Json);
        assert_eq!(Language::from_path(Path::new("schema.proto")), Language::Protobuf);
        assert_eq!(Language::from_path(Path::new("main.tf")), Language::Hcl);
        assert_eq!(Language::from_path(Path::new("App.kt")), Language::Kotlin);
        assert_eq!(Language::from_path(Path::new("main.swift")), Language::Swift);
        assert_eq!(Language::from_path(Path::new("Main.scala")), Language::Scala);
        assert_eq!(Language::from_path(Path::new("app.ex")), Language::Elixir);
        assert_eq!(Language::from_path(Path::new("server.erl")), Language::Erlang);
        assert_eq!(Language::from_path(Path::new("Main.hs")), Language::Haskell);
        assert_eq!(Language::from_path(Path::new("parser.ml")), Language::OCaml);
        assert_eq!(Language::from_path(Path::new("script.lua")), Language::Lua);
        assert_eq!(Language::from_path(Path::new("util.pl")), Language::Perl);
        assert_eq!(Language::from_path(Path::new("analysis.r")), Language::R);
        assert_eq!(Language::from_path(Path::new("main.dart")), Language::Dart);
        assert_eq!(Language::from_path(Path::new("build.zig")), Language::Zig);
        assert_eq!(Language::from_path(Path::new("default.nix")), Language::Nix);
    }

    #[test]
    fn test_from_path_no_extension() {
        use std::path::Path;

        // Files with no extension default to Unknown
        assert_eq!(Language::from_path(Path::new("Makefile")), Language::Unknown);
        assert_eq!(Language::from_path(Path::new("LICENSE")), Language::Unknown);
    }

    #[test]
    fn test_from_path_dockerfile() {
        use std::path::Path;

        // Dockerfile detected by filename, not extension
        assert_eq!(Language::from_path(Path::new("Dockerfile")), Language::Dockerfile);
        assert_eq!(Language::from_path(Path::new("path/to/Dockerfile")), Language::Dockerfile);
        assert_eq!(Language::from_path(Path::new("Dockerfile.prod")), Language::Dockerfile);
    }

    #[test]
    fn test_symbol_kind_display() {
        assert_eq!(SymbolKind::Function.to_string(), "Function");
        assert_eq!(SymbolKind::Struct.to_string(), "Struct");
        assert_eq!(SymbolKind::Trait.to_string(), "Trait");
        assert_eq!(SymbolKind::Enum.to_string(), "Enum");
        assert_eq!(SymbolKind::Interface.to_string(), "Interface");
        assert_eq!(SymbolKind::Class.to_string(), "Class");
        assert_eq!(SymbolKind::Method.to_string(), "Method");
        assert_eq!(SymbolKind::Constant.to_string(), "Constant");
        assert_eq!(SymbolKind::Variable.to_string(), "Variable");
        assert_eq!(SymbolKind::Type.to_string(), "Type");
        assert_eq!(SymbolKind::Module.to_string(), "Module");
    }
}
