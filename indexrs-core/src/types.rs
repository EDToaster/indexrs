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
    PlainText,
    Starlark,
    Jsonnet,
    Haml,
    Csv,
    GraphQL,
    Erb,
    Template,
    ReStructuredText,
    Ejs,
    Groovy,
    Batch,
    CSharp,
    Vue,
    Svelte,
    PowerShell,
    Less,
    CoffeeScript,
    Solidity,
    Clojure,
    Julia,
    Assembly,
    Nim,
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
            Language::PlainText => 36,
            Language::Starlark => 37,
            Language::Jsonnet => 38,
            Language::Haml => 39,
            Language::Csv => 40,
            Language::GraphQL => 41,
            Language::Erb => 42,
            Language::Template => 43,
            Language::ReStructuredText => 44,
            Language::Ejs => 45,
            Language::Groovy => 46,
            Language::Batch => 47,
            Language::CSharp => 48,
            Language::Vue => 49,
            Language::Svelte => 50,
            Language::PowerShell => 51,
            Language::Less => 52,
            Language::CoffeeScript => 53,
            Language::Solidity => 54,
            Language::Clojure => 55,
            Language::Julia => 56,
            Language::Assembly => 57,
            Language::Nim => 58,
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
            36 => Language::PlainText,
            37 => Language::Starlark,
            38 => Language::Jsonnet,
            39 => Language::Haml,
            40 => Language::Csv,
            41 => Language::GraphQL,
            42 => Language::Erb,
            43 => Language::Template,
            44 => Language::ReStructuredText,
            45 => Language::Ejs,
            46 => Language::Groovy,
            47 => Language::Batch,
            48 => Language::CSharp,
            49 => Language::Vue,
            50 => Language::Svelte,
            51 => Language::PowerShell,
            52 => Language::Less,
            53 => Language::CoffeeScript,
            54 => Language::Solidity,
            55 => Language::Clojure,
            56 => Language::Julia,
            57 => Language::Assembly,
            58 => Language::Nim,
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
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && (name == "Dockerfile" || name.starts_with("Dockerfile."))
        {
            return Language::Dockerfile;
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
            "py" | "pyi" | "pyw" | "pyx" | "pxd" => Language::Python,
            "ts" | "tsx" | "mts" | "cts" => Language::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "go" => Language::Go,
            "c" | "h" => Language::C,
            "cpp" | "cxx" | "cc" | "hpp" | "hxx" | "hh" => Language::Cpp,
            "java" => Language::Java,
            "rb" | "rbi" | "rake" | "gemspec" | "jbuilder" | "ru" | "podspec" => Language::Ruby,
            "sh" | "bash" | "zsh" | "fish" | "csh" | "tcsh" => Language::Shell,
            "md" | "markdown" | "mdx" | "Rmd" | "rmd" => Language::Markdown,
            "yml" | "yaml" => Language::Yaml,
            "toml" => Language::Toml,
            "json" | "jsonc" | "json5" | "jsonl" | "geojson" => Language::Json,
            "xml" | "xsl" | "xslt" | "svg" | "xsd" | "wsdl" | "dtd" => Language::Xml,
            "html" | "htm" => Language::Html,
            "css" => Language::Css,
            "scss" => Language::Scss,
            "sass" => Language::Sass,
            "sql" | "ddl" | "hql" => Language::Sql,
            "proto" => Language::Protobuf,
            "tf" | "hcl" | "tfvars" => Language::Hcl,
            "kt" | "kts" => Language::Kotlin,
            "swift" => Language::Swift,
            "scala" | "sc" | "sbt" => Language::Scala,
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
            // New language variants
            "txt" | "text" | "log" => Language::PlainText,
            "bazel" | "bzl" | "star" | "starlark" => Language::Starlark,
            "jsonnet" | "libsonnet" => Language::Jsonnet,
            "haml" => Language::Haml,
            "csv" | "tsv" => Language::Csv,
            "graphql" | "gql" => Language::GraphQL,
            "erb" => Language::Erb,
            "j2" | "jinja" | "jinja2" | "mako" => Language::Template,
            "rst" => Language::ReStructuredText,
            "ejs" => Language::Ejs,
            "groovy" | "gradle" | "gvy" | "gy" | "gsh" => Language::Groovy,
            "bat" | "cmd" => Language::Batch,
            "cs" | "csx" => Language::CSharp,
            "vue" => Language::Vue,
            "svelte" => Language::Svelte,
            "ps1" => Language::PowerShell,
            "less" => Language::Less,
            "coffee" | "cjsx" => Language::CoffeeScript,
            "sol" => Language::Solidity,
            "clj" | "cljs" | "cljc" | "edn" => Language::Clojure,
            "jl" => Language::Julia,
            "asm" | "s" | "S" | "nasm" => Language::Assembly,
            "nim" => Language::Nim,
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
            Language::PlainText => write!(f, "Plain Text"),
            Language::Starlark => write!(f, "Starlark"),
            Language::Jsonnet => write!(f, "Jsonnet"),
            Language::Haml => write!(f, "Haml"),
            Language::Csv => write!(f, "CSV"),
            Language::GraphQL => write!(f, "GraphQL"),
            Language::Erb => write!(f, "ERB"),
            Language::Template => write!(f, "Template"),
            Language::ReStructuredText => write!(f, "reStructuredText"),
            Language::Ejs => write!(f, "EJS"),
            Language::Groovy => write!(f, "Groovy"),
            Language::Batch => write!(f, "Batch"),
            Language::CSharp => write!(f, "C#"),
            Language::Vue => write!(f, "Vue"),
            Language::Svelte => write!(f, "Svelte"),
            Language::PowerShell => write!(f, "PowerShell"),
            Language::Less => write!(f, "Less"),
            Language::CoffeeScript => write!(f, "CoffeeScript"),
            Language::Solidity => write!(f, "Solidity"),
            Language::Clojure => write!(f, "Clojure"),
            Language::Julia => write!(f, "Julia"),
            Language::Assembly => write!(f, "Assembly"),
            Language::Nim => write!(f, "Nim"),
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

impl SymbolKind {
    /// Convert this symbol kind to its `u8` representation for binary serialization.
    ///
    /// Each variant maps to a fixed numeric value (0..=10).
    pub fn to_u8(self) -> u8 {
        match self {
            SymbolKind::Function => 0,
            SymbolKind::Struct => 1,
            SymbolKind::Trait => 2,
            SymbolKind::Enum => 3,
            SymbolKind::Interface => 4,
            SymbolKind::Class => 5,
            SymbolKind::Method => 6,
            SymbolKind::Constant => 7,
            SymbolKind::Variable => 8,
            SymbolKind::Type => 9,
            SymbolKind::Module => 10,
        }
    }

    /// Reconstruct a `SymbolKind` from its `u8` representation.
    ///
    /// Returns `None` for unrecognized values.
    pub fn from_u8(v: u8) -> Option<SymbolKind> {
        match v {
            0 => Some(SymbolKind::Function),
            1 => Some(SymbolKind::Struct),
            2 => Some(SymbolKind::Trait),
            3 => Some(SymbolKind::Enum),
            4 => Some(SymbolKind::Interface),
            5 => Some(SymbolKind::Class),
            6 => Some(SymbolKind::Method),
            7 => Some(SymbolKind::Constant),
            8 => Some(SymbolKind::Variable),
            9 => Some(SymbolKind::Type),
            10 => Some(SymbolKind::Module),
            _ => None,
        }
    }

    /// Parse a `SymbolKind` from user input, supporting common aliases.
    ///
    /// The match is case-insensitive. Supported aliases include:
    /// - Function: "function", "fn", "func", "def"
    /// - Struct: "struct"
    /// - Trait: "trait"
    /// - Enum: "enum"
    /// - Interface: "interface", "iface"
    /// - Class: "class"
    /// - Method: "method"
    /// - Constant: "constant", "const"
    /// - Variable: "variable", "var", "let"
    /// - Type: "type", "typedef", "alias"
    /// - Module: "module", "mod", "namespace", "package", "ns"
    pub fn from_str_loose(s: &str) -> Option<SymbolKind> {
        match s.to_ascii_lowercase().as_str() {
            "function" | "fn" | "func" | "def" => Some(SymbolKind::Function),
            "struct" => Some(SymbolKind::Struct),
            "trait" => Some(SymbolKind::Trait),
            "enum" => Some(SymbolKind::Enum),
            "interface" | "iface" => Some(SymbolKind::Interface),
            "class" => Some(SymbolKind::Class),
            "method" => Some(SymbolKind::Method),
            "constant" | "const" => Some(SymbolKind::Constant),
            "variable" | "var" | "let" => Some(SymbolKind::Variable),
            "type" | "typedef" | "alias" => Some(SymbolKind::Type),
            "module" | "mod" | "namespace" | "package" | "ns" => Some(SymbolKind::Module),
            _ => None,
        }
    }

    /// Returns a short lowercase label suitable for CLI output.
    ///
    /// These labels are compact and familiar to developers:
    /// `"fn"`, `"struct"`, `"trait"`, `"enum"`, `"interface"`, `"class"`,
    /// `"method"`, `"const"`, `"var"`, `"type"`, `"mod"`.
    pub fn short_label(self) -> &'static str {
        match self {
            SymbolKind::Function => "fn",
            SymbolKind::Struct => "struct",
            SymbolKind::Trait => "trait",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::Class => "class",
            SymbolKind::Method => "method",
            SymbolKind::Constant => "const",
            SymbolKind::Variable => "var",
            SymbolKind::Type => "type",
            SymbolKind::Module => "mod",
        }
    }
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
            Language::PlainText,
            Language::Starlark,
            Language::Jsonnet,
            Language::Haml,
            Language::Csv,
            Language::GraphQL,
            Language::Erb,
            Language::Template,
            Language::ReStructuredText,
            Language::Ejs,
            Language::Groovy,
            Language::Batch,
            Language::CSharp,
            Language::Vue,
            Language::Svelte,
            Language::PowerShell,
            Language::Less,
            Language::CoffeeScript,
            Language::Solidity,
            Language::Clojure,
            Language::Julia,
            Language::Assembly,
            Language::Nim,
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
            (Language::PlainText, 36),
            (Language::Starlark, 37),
            (Language::Jsonnet, 38),
            (Language::Haml, 39),
            (Language::Csv, 40),
            (Language::GraphQL, 41),
            (Language::Erb, 42),
            (Language::Template, 43),
            (Language::ReStructuredText, 44),
            (Language::Ejs, 45),
            (Language::Groovy, 46),
            (Language::Batch, 47),
            (Language::CSharp, 48),
            (Language::Vue, 49),
            (Language::Svelte, 50),
            (Language::PowerShell, 51),
            (Language::Less, 52),
            (Language::CoffeeScript, 53),
            (Language::Solidity, 54),
            (Language::Clojure, 55),
            (Language::Julia, 56),
            (Language::Assembly, 57),
            (Language::Nim, 58),
        ];
        for (lang, expected_u16) in new_languages {
            assert_eq!(lang.to_u16(), expected_u16, "{lang} to_u16");
            assert_eq!(
                Language::from_u16(expected_u16),
                lang,
                "from_u16({expected_u16})"
            );
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

        assert_eq!(
            Language::from_path(Path::new("src/main.rs")),
            Language::Rust
        );
        assert_eq!(Language::from_path(Path::new("lib.py")), Language::Python);
        assert_eq!(
            Language::from_path(Path::new("deep/nested/app.tsx")),
            Language::TypeScript
        );
        assert_eq!(Language::from_path(Path::new("config.yml")), Language::Yaml);
        assert_eq!(Language::from_path(Path::new("Cargo.toml")), Language::Toml);
        assert_eq!(Language::from_path(Path::new("data.json")), Language::Json);
        assert_eq!(
            Language::from_path(Path::new("schema.proto")),
            Language::Protobuf
        );
        assert_eq!(Language::from_path(Path::new("main.tf")), Language::Hcl);
        assert_eq!(Language::from_path(Path::new("App.kt")), Language::Kotlin);
        assert_eq!(
            Language::from_path(Path::new("main.swift")),
            Language::Swift
        );
        assert_eq!(
            Language::from_path(Path::new("Main.scala")),
            Language::Scala
        );
        assert_eq!(Language::from_path(Path::new("app.ex")), Language::Elixir);
        assert_eq!(
            Language::from_path(Path::new("server.erl")),
            Language::Erlang
        );
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
        assert_eq!(
            Language::from_path(Path::new("Makefile")),
            Language::Unknown
        );
        assert_eq!(Language::from_path(Path::new("LICENSE")), Language::Unknown);
    }

    #[test]
    fn test_from_path_dockerfile() {
        use std::path::Path;

        // Dockerfile detected by filename, not extension
        assert_eq!(
            Language::from_path(Path::new("Dockerfile")),
            Language::Dockerfile
        );
        assert_eq!(
            Language::from_path(Path::new("path/to/Dockerfile")),
            Language::Dockerfile
        );
        assert_eq!(
            Language::from_path(Path::new("Dockerfile.prod")),
            Language::Dockerfile
        );
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

    #[test]
    fn test_symbol_kind_u8_roundtrip() {
        let all_kinds = [
            SymbolKind::Function,
            SymbolKind::Struct,
            SymbolKind::Trait,
            SymbolKind::Enum,
            SymbolKind::Interface,
            SymbolKind::Class,
            SymbolKind::Method,
            SymbolKind::Constant,
            SymbolKind::Variable,
            SymbolKind::Type,
            SymbolKind::Module,
        ];
        for kind in all_kinds {
            let v = kind.to_u8();
            let roundtripped = SymbolKind::from_u8(v)
                .unwrap_or_else(|| panic!("from_u8({v}) should return Some for {kind}"));
            assert_eq!(roundtripped, kind, "roundtrip failed for {kind}");
        }
    }

    #[test]
    fn test_symbol_kind_u8_known_values() {
        assert_eq!(SymbolKind::Function.to_u8(), 0);
        assert_eq!(SymbolKind::Struct.to_u8(), 1);
        assert_eq!(SymbolKind::Trait.to_u8(), 2);
        assert_eq!(SymbolKind::Enum.to_u8(), 3);
        assert_eq!(SymbolKind::Interface.to_u8(), 4);
        assert_eq!(SymbolKind::Class.to_u8(), 5);
        assert_eq!(SymbolKind::Method.to_u8(), 6);
        assert_eq!(SymbolKind::Constant.to_u8(), 7);
        assert_eq!(SymbolKind::Variable.to_u8(), 8);
        assert_eq!(SymbolKind::Type.to_u8(), 9);
        assert_eq!(SymbolKind::Module.to_u8(), 10);
    }

    #[test]
    fn test_symbol_kind_from_u8_invalid() {
        assert!(SymbolKind::from_u8(11).is_none());
        assert!(SymbolKind::from_u8(100).is_none());
        assert!(SymbolKind::from_u8(255).is_none());
    }

    #[test]
    fn test_symbol_kind_from_str_loose_canonical() {
        assert_eq!(
            SymbolKind::from_str_loose("function"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            SymbolKind::from_str_loose("struct"),
            Some(SymbolKind::Struct)
        );
        assert_eq!(SymbolKind::from_str_loose("trait"), Some(SymbolKind::Trait));
        assert_eq!(SymbolKind::from_str_loose("enum"), Some(SymbolKind::Enum));
        assert_eq!(
            SymbolKind::from_str_loose("interface"),
            Some(SymbolKind::Interface)
        );
        assert_eq!(SymbolKind::from_str_loose("class"), Some(SymbolKind::Class));
        assert_eq!(
            SymbolKind::from_str_loose("method"),
            Some(SymbolKind::Method)
        );
        assert_eq!(
            SymbolKind::from_str_loose("constant"),
            Some(SymbolKind::Constant)
        );
        assert_eq!(
            SymbolKind::from_str_loose("variable"),
            Some(SymbolKind::Variable)
        );
        assert_eq!(SymbolKind::from_str_loose("type"), Some(SymbolKind::Type));
        assert_eq!(
            SymbolKind::from_str_loose("module"),
            Some(SymbolKind::Module)
        );
    }

    #[test]
    fn test_symbol_kind_from_str_loose_aliases() {
        // Function aliases
        assert_eq!(SymbolKind::from_str_loose("fn"), Some(SymbolKind::Function));
        assert_eq!(
            SymbolKind::from_str_loose("func"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            SymbolKind::from_str_loose("def"),
            Some(SymbolKind::Function)
        );

        // Interface alias
        assert_eq!(
            SymbolKind::from_str_loose("iface"),
            Some(SymbolKind::Interface)
        );

        // Constant alias
        assert_eq!(
            SymbolKind::from_str_loose("const"),
            Some(SymbolKind::Constant)
        );

        // Variable aliases
        assert_eq!(
            SymbolKind::from_str_loose("var"),
            Some(SymbolKind::Variable)
        );
        assert_eq!(
            SymbolKind::from_str_loose("let"),
            Some(SymbolKind::Variable)
        );

        // Type aliases
        assert_eq!(
            SymbolKind::from_str_loose("typedef"),
            Some(SymbolKind::Type)
        );
        assert_eq!(SymbolKind::from_str_loose("alias"), Some(SymbolKind::Type));

        // Module aliases
        assert_eq!(SymbolKind::from_str_loose("mod"), Some(SymbolKind::Module));
        assert_eq!(
            SymbolKind::from_str_loose("namespace"),
            Some(SymbolKind::Module)
        );
        assert_eq!(
            SymbolKind::from_str_loose("package"),
            Some(SymbolKind::Module)
        );
        assert_eq!(SymbolKind::from_str_loose("ns"), Some(SymbolKind::Module));
    }

    #[test]
    fn test_symbol_kind_from_str_loose_case_insensitive() {
        assert_eq!(
            SymbolKind::from_str_loose("Function"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            SymbolKind::from_str_loose("STRUCT"),
            Some(SymbolKind::Struct)
        );
        assert_eq!(SymbolKind::from_str_loose("FN"), Some(SymbolKind::Function));
        assert_eq!(
            SymbolKind::from_str_loose("Const"),
            Some(SymbolKind::Constant)
        );
        assert_eq!(SymbolKind::from_str_loose("MOD"), Some(SymbolKind::Module));
    }

    #[test]
    fn test_symbol_kind_from_str_loose_invalid() {
        assert!(SymbolKind::from_str_loose("").is_none());
        assert!(SymbolKind::from_str_loose("unknown").is_none());
        assert!(SymbolKind::from_str_loose("foo").is_none());
        assert!(SymbolKind::from_str_loose("123").is_none());
    }

    #[test]
    fn test_symbol_kind_short_label() {
        assert_eq!(SymbolKind::Function.short_label(), "fn");
        assert_eq!(SymbolKind::Struct.short_label(), "struct");
        assert_eq!(SymbolKind::Trait.short_label(), "trait");
        assert_eq!(SymbolKind::Enum.short_label(), "enum");
        assert_eq!(SymbolKind::Interface.short_label(), "interface");
        assert_eq!(SymbolKind::Class.short_label(), "class");
        assert_eq!(SymbolKind::Method.short_label(), "method");
        assert_eq!(SymbolKind::Constant.short_label(), "const");
        assert_eq!(SymbolKind::Variable.short_label(), "var");
        assert_eq!(SymbolKind::Type.short_label(), "type");
        assert_eq!(SymbolKind::Module.short_label(), "mod");
    }

    #[test]
    fn test_symbol_kind_short_label_roundtrips_via_from_str_loose() {
        let all_kinds = [
            SymbolKind::Function,
            SymbolKind::Struct,
            SymbolKind::Trait,
            SymbolKind::Enum,
            SymbolKind::Interface,
            SymbolKind::Class,
            SymbolKind::Method,
            SymbolKind::Constant,
            SymbolKind::Variable,
            SymbolKind::Type,
            SymbolKind::Module,
        ];
        for kind in all_kinds {
            let label = kind.short_label();
            let parsed = SymbolKind::from_str_loose(label)
                .unwrap_or_else(|| panic!("from_str_loose({label:?}) should return Some"));
            assert_eq!(
                parsed, kind,
                "short_label -> from_str_loose roundtrip failed for {kind}"
            );
        }
    }
}
