# Language Detection Enhancement Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Expand the `Language` enum to support 25+ additional languages with extension-based detection, a `from_path` convenience method, and full backward compatibility.

**Architecture:** The existing `Language` enum in `ferret-indexer-core/src/types.rs` gets new variants appended. The `to_u16`/`from_u16` mapping preserves existing values 0-10 and 0xFFFF, assigning new values starting at 11. A new `from_path()` method extracts the extension from a `Path` and delegates to the existing `from_extension()`. Filename-based detection (e.g., `Dockerfile`) is handled in `from_path()` since these files have no extension.

**Tech Stack:** Rust, `std::path::Path`

---

### Task 1: Add new Language variants and update u16 encoding

**Files:**
- Modify: `ferret-indexer-core/src/types.rs:78-133` (enum definition, `to_u16`, `from_u16`)

**Step 1: Write the failing test for new variants u16 roundtrip**

Add to the existing `mod tests` block in `types.rs`, before the closing `}`:

```rust
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core test_new_language_u16_roundtrip -- --nocapture`
Expected: FAIL — compiler error, variants do not exist yet

**Step 3: Add new enum variants and update to_u16/from_u16**

In the `Language` enum definition (after `Markdown`, before `Unknown`), add:

```rust
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
```

In `to_u16()`, add these arms before the `Unknown` arm:

```rust
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
```

In `from_u16()`, add matching arms before the `_` wildcard:

```rust
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
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p ferret-indexer-core test_new_language_u16_roundtrip test_backward_compat -- --nocapture`
Expected: FAIL — Display impl is not exhaustive yet (compiler error). We fix that in Task 2.

---

### Task 2: Update Display impl and from_extension for new variants

**Files:**
- Modify: `ferret-indexer-core/src/types.rs:148-183` (from_extension, Display)

**Step 1: Write the failing tests for new extensions and display**

Add to `mod tests`:

```rust
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core test_new_language -- --nocapture`
Expected: FAIL — compiler error, non-exhaustive match in Display

**Step 3: Update Display impl and from_extension**

In `from_extension()`, add these arms before the `_ => Language::Unknown` fallback:

```rust
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
```

In `Display` impl, add these arms before `Language::Unknown`:

```rust
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
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferret-indexer-core -- --nocapture`
Expected: ALL PASS (including old tests)

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/types.rs
git commit -m "feat: expand Language enum with 25 new language variants

Add YAML, TOML, JSON, XML, HTML, CSS, SCSS, Sass, SQL, Protobuf,
Dockerfile, HCL, Kotlin, Swift, Scala, Elixir, Erlang, Haskell,
OCaml, Lua, Perl, R, Dart, Zig, and Nix. Existing u16 values 0-10
and 0xFFFF are unchanged for backward compatibility."
```

---

### Task 3: Add from_path convenience method

**Files:**
- Modify: `ferret-indexer-core/src/types.rs` (add `use std::path::Path`, add `from_path` method)

**Step 1: Write the failing test**

Add to `mod tests`:

```rust
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p ferret-indexer-core test_from_path -- --nocapture`
Expected: FAIL — `from_path` method does not exist

**Step 3: Implement from_path**

Add `use std::path::Path;` at the top of `types.rs` (alongside existing `use std::fmt;`).

Add this method to the `impl Language` block:

```rust
/// Detect language from a file path.
///
/// Extracts the file extension and delegates to [`from_extension`](Language::from_extension).
/// Also handles special filenames like `Dockerfile` that have no extension.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use ferret_indexer_core::Language;
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
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p ferret-indexer-core -- --nocapture`
Expected: ALL PASS

**Step 5: Commit**

```bash
git add ferret-indexer-core/src/types.rs
git commit -m "feat: add Language::from_path() convenience method

Extracts the file extension from a Path and delegates to
from_extension(). Also handles filename-based detection for
Dockerfile."
```

---

### Task 4: Update existing roundtrip tests and metadata tests for completeness

**Files:**
- Modify: `ferret-indexer-core/src/types.rs` (update `test_language_u16_roundtrip`)
- Modify: `ferret-indexer-core/src/metadata.rs` (update `test_roundtrip_all_languages`)

**Step 1: Update existing roundtrip test to include all variants**

In `types.rs`, update `test_language_u16_roundtrip` to include all new variants in the array.

In `metadata.rs`, update `test_roundtrip_all_languages` to include all new variants in the array.

**Step 2: Run all tests**

Run: `cargo test -p ferret-indexer-core -- --nocapture`
Expected: ALL PASS

**Step 3: Run clippy**

Run: `cargo clippy -p ferret-indexer-core -- -D warnings`
Expected: No warnings

**Step 4: Commit**

```bash
git add ferret-indexer-core/src/types.rs ferret-indexer-core/src/metadata.rs
git commit -m "test: update roundtrip tests to cover all 37 Language variants"
```

---

### Summary of u16 assignments

| Value | Language     | Status       |
|-------|-------------|--------------|
| 0     | Rust        | Existing     |
| 1     | Python      | Existing     |
| 2     | TypeScript  | Existing     |
| 3     | JavaScript  | Existing     |
| 4     | Go          | Existing     |
| 5     | C           | Existing     |
| 6     | Cpp         | Existing     |
| 7     | Java        | Existing     |
| 8     | Ruby        | Existing     |
| 9     | Shell       | Existing     |
| 10    | Markdown    | Existing     |
| 11    | Yaml        | **New**      |
| 12    | Toml        | **New**      |
| 13    | Json        | **New**      |
| 14    | Xml         | **New**      |
| 15    | Html        | **New**      |
| 16    | Css         | **New**      |
| 17    | Scss        | **New**      |
| 18    | Sass        | **New**      |
| 19    | Sql         | **New**      |
| 20    | Protobuf    | **New**      |
| 21    | Dockerfile  | **New**      |
| 22    | Hcl         | **New**      |
| 23    | Kotlin      | **New**      |
| 24    | Swift       | **New**      |
| 25    | Scala       | **New**      |
| 26    | Elixir      | **New**      |
| 27    | Erlang      | **New**      |
| 28    | Haskell     | **New**      |
| 29    | OCaml       | **New**      |
| 30    | Lua         | **New**      |
| 31    | Perl        | **New**      |
| 32    | R           | **New**      |
| 33    | Dart        | **New**      |
| 34    | Zig         | **New**      |
| 35    | Nix         | **New**      |
| 0xFFFF| Unknown     | Existing     |
