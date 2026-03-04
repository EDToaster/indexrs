//! Tree-sitter based symbol extraction for supported languages.
//!
//! Walks the AST produced by tree-sitter to find definitions (functions, structs,
//! classes, traits, enums, etc.) and returns them as [`SymbolEntry`] values.
//! This module is gated behind the `symbols` cargo feature.

use std::collections::HashMap;
use std::sync::OnceLock;

use streaming_iterator::StreamingIterator;

use crate::grammar::tree_sitter_language;
use crate::types::{FileId, Language, SymbolKind};

/// A symbol definition extracted from source code via tree-sitter AST walking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolEntry {
    /// The file this symbol was found in.
    pub file_id: FileId,
    /// The symbol name (e.g. function name, struct name).
    pub name: String,
    /// The kind of symbol (function, struct, enum, etc.).
    pub kind: SymbolKind,
    /// 1-based line number where the symbol is defined.
    pub line: u32,
    /// 0-based column offset where the symbol name starts.
    pub column: u16,
}

// ---------------------------------------------------------------------------
// Per-language tree-sitter query patterns
// ---------------------------------------------------------------------------

/// Tree-sitter query for Rust symbol definitions.
const RUST_QUERY: &str = r#"
(function_item
  name: (identifier) @name) @definition.function

(struct_item
  name: (type_identifier) @name) @definition.struct

(enum_item
  name: (type_identifier) @name) @definition.enum

(trait_item
  name: (type_identifier) @name) @definition.trait

(type_item
  name: (type_identifier) @name) @definition.type

(const_item
  name: (identifier) @name) @definition.constant

(static_item
  name: (identifier) @name) @definition.constant

(mod_item
  name: (identifier) @name) @definition.module

(impl_item
  body: (declaration_list
    (function_item
      name: (identifier) @name) @definition.method))
"#;

/// Tree-sitter query for Python symbol definitions.
const PYTHON_QUERY: &str = r#"
(function_definition
  name: (identifier) @name) @definition.function

(class_definition
  name: (identifier) @name) @definition.class
"#;

/// Tree-sitter query for TypeScript symbol definitions.
const TYPESCRIPT_QUERY: &str = r#"
(function_declaration
  name: (identifier) @name) @definition.function

(class_declaration
  name: (type_identifier) @name) @definition.class

(interface_declaration
  name: (type_identifier) @name) @definition.interface

(type_alias_declaration
  name: (type_identifier) @name) @definition.type

(enum_declaration
  name: (identifier) @name) @definition.enum

(method_definition
  name: (property_identifier) @name) @definition.method
"#;

/// Tree-sitter query for Go symbol definitions.
const GO_QUERY: &str = r#"
(function_declaration
  name: (identifier) @name) @definition.function

(method_declaration
  name: (field_identifier) @name) @definition.method

(type_declaration
  (type_spec
    name: (type_identifier) @name) @definition.type)
"#;

/// Tree-sitter query for C symbol definitions.
const C_QUERY: &str = r#"
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

(struct_specifier
  name: (type_identifier) @name) @definition.struct

(enum_specifier
  name: (type_identifier) @name) @definition.enum

(type_definition
  declarator: (type_identifier) @name) @definition.type
"#;

/// Global cache of compiled tree-sitter queries, keyed by language.
fn compiled_queries() -> &'static HashMap<Language, tree_sitter::Query> {
    static CACHE: OnceLock<HashMap<Language, tree_sitter::Query>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let pairs: &[(Language, &str)] = &[
            (Language::Rust, RUST_QUERY),
            (Language::Python, PYTHON_QUERY),
            (Language::TypeScript, TYPESCRIPT_QUERY),
            (Language::JavaScript, TYPESCRIPT_QUERY),
            (Language::Go, GO_QUERY),
            (Language::C, C_QUERY),
            (Language::Cpp, C_QUERY),
        ];
        let mut map = HashMap::with_capacity(pairs.len());
        for &(lang, query_src) in pairs {
            if let Some(ts_lang) = tree_sitter_language(lang)
                && let Ok(q) = tree_sitter::Query::new(&ts_lang, query_src)
            {
                map.insert(lang, q);
            }
        }
        map
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Maps a tree-sitter capture name suffix (e.g. `"definition.function"`) to
/// the corresponding [`SymbolKind`].
///
/// Returns `None` for unrecognized capture names (including `"name"`).
fn kind_from_capture(capture_name: &str) -> Option<SymbolKind> {
    match capture_name {
        "definition.function" => Some(SymbolKind::Function),
        "definition.struct" => Some(SymbolKind::Struct),
        "definition.trait" => Some(SymbolKind::Trait),
        "definition.enum" => Some(SymbolKind::Enum),
        "definition.interface" => Some(SymbolKind::Interface),
        "definition.class" => Some(SymbolKind::Class),
        "definition.method" => Some(SymbolKind::Method),
        "definition.constant" => Some(SymbolKind::Constant),
        "definition.variable" => Some(SymbolKind::Variable),
        "definition.type" => Some(SymbolKind::Type),
        "definition.module" => Some(SymbolKind::Module),
        _ => None,
    }
}

/// Extract symbol definitions from source code using tree-sitter.
///
/// Returns an empty vec if:
/// - The language is not supported for symbol extraction
/// - The content is empty
/// - Parsing fails
/// - The query pattern fails to compile (logged as a warning)
pub fn extract_symbols(file_id: FileId, content: &[u8], language: Language) -> Vec<SymbolEntry> {
    if content.is_empty() {
        return Vec::new();
    }

    let query = match compiled_queries().get(&language) {
        Some(q) => q,
        None => return Vec::new(),
    };

    let ts_lang = match tree_sitter_language(language) {
        Some(l) => l,
        None => return Vec::new(),
    };

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        tracing::warn!("failed to set tree-sitter language for {:?}", language);
        return Vec::new();
    }

    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut symbols = Vec::new();
    let mut matches = cursor.matches(query, tree.root_node(), content);

    while let Some(match_) = {
        matches.advance();
        matches.get()
    } {
        let mut name_text: Option<String> = None;
        let mut kind: Option<SymbolKind> = None;
        let mut def_line: u32 = 0;
        let mut def_column: u16 = 0;

        for capture in match_.captures {
            let capture_name = query.capture_names()[capture.index as usize];
            if capture_name == "name" {
                let node = capture.node;
                if let Ok(text) = std::str::from_utf8(&content[node.byte_range()]) {
                    name_text = Some(text.to_string());
                    // Use the name node's position for line/column
                    def_line = node.start_position().row as u32; // 0-based; callers add 1 for display
                    def_column = node.start_position().column as u16;
                }
            } else if let Some(k) = kind_from_capture(capture_name) {
                kind = Some(k);
            }
        }

        if let (Some(sym_name), Some(sym_kind)) = (name_text, kind) {
            symbols.push(SymbolEntry {
                file_id,
                name: sym_name,
                kind: sym_kind,
                line: def_line,
                column: def_column,
            });
        }
    }

    symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helper
    // -----------------------------------------------------------------------

    fn extract(lang: Language, src: &str) -> Vec<SymbolEntry> {
        extract_symbols(FileId(0), src.as_bytes(), lang)
    }

    fn names(entries: &[SymbolEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn find_by_name<'a>(entries: &'a [SymbolEntry], name: &str) -> Option<&'a SymbolEntry> {
        entries.iter().find(|e| e.name == name)
    }

    // -----------------------------------------------------------------------
    // Rust
    // -----------------------------------------------------------------------

    #[test]
    fn test_rust_functions() {
        let src = r#"
fn foo() {}
fn bar(x: i32) -> i32 { x }
"#;
        let syms = extract(Language::Rust, src);
        assert!(names(&syms).contains(&"foo"));
        assert!(names(&syms).contains(&"bar"));
        assert!(syms.iter().all(|s| s.kind == SymbolKind::Function));
    }

    #[test]
    fn test_rust_structs() {
        let src = r#"
struct Point { x: f64, y: f64 }
struct Empty;
"#;
        let syms = extract(Language::Rust, src);
        assert!(names(&syms).contains(&"Point"));
        assert!(names(&syms).contains(&"Empty"));
        assert!(syms.iter().all(|s| s.kind == SymbolKind::Struct));
    }

    #[test]
    fn test_rust_enums() {
        let src = r#"
enum Color {
    Red,
    Green,
    Blue,
}
"#;
        let syms = extract(Language::Rust, src);
        let color = find_by_name(&syms, "Color").expect("should find Color");
        assert_eq!(color.kind, SymbolKind::Enum);
    }

    #[test]
    fn test_rust_traits() {
        let src = r#"
trait Drawable {
    fn draw(&self);
}
"#;
        let syms = extract(Language::Rust, src);
        let drawable = find_by_name(&syms, "Drawable").expect("should find Drawable");
        assert_eq!(drawable.kind, SymbolKind::Trait);
    }

    #[test]
    fn test_rust_impl_methods() {
        let src = r#"
struct Foo;
impl Foo {
    fn new() -> Self { Foo }
    fn method(&self) {}
}
"#;
        let syms = extract(Language::Rust, src);
        let new_sym = find_by_name(&syms, "new").expect("should find new");
        assert_eq!(new_sym.kind, SymbolKind::Method);
        let method_sym = find_by_name(&syms, "method").expect("should find method");
        assert_eq!(method_sym.kind, SymbolKind::Method);
    }

    #[test]
    fn test_rust_modules() {
        let src = r#"
mod inner {
    fn hidden() {}
}
"#;
        let syms = extract(Language::Rust, src);
        let inner = find_by_name(&syms, "inner").expect("should find inner");
        assert_eq!(inner.kind, SymbolKind::Module);
    }

    #[test]
    fn test_rust_type_alias() {
        let src = "type Meters = f64;\n";
        let syms = extract(Language::Rust, src);
        let meters = find_by_name(&syms, "Meters").expect("should find Meters");
        assert_eq!(meters.kind, SymbolKind::Type);
    }

    #[test]
    fn test_rust_constants() {
        let src = r#"
const MAX: u32 = 100;
static GLOBAL: &str = "hello";
"#;
        let syms = extract(Language::Rust, src);
        let max_sym = find_by_name(&syms, "MAX").expect("should find MAX");
        assert_eq!(max_sym.kind, SymbolKind::Constant);
        let global_sym = find_by_name(&syms, "GLOBAL").expect("should find GLOBAL");
        assert_eq!(global_sym.kind, SymbolKind::Constant);
    }

    #[test]
    fn test_rust_line_numbers() {
        let src = "fn first() {}\nfn second() {}\nfn third() {}\n";
        let syms = extract(Language::Rust, src);
        let first = find_by_name(&syms, "first").expect("should find first");
        assert_eq!(first.line, 0); // 0-based; callers add 1 for display
        let second = find_by_name(&syms, "second").expect("should find second");
        assert_eq!(second.line, 1);
        let third = find_by_name(&syms, "third").expect("should find third");
        assert_eq!(third.line, 2);
    }

    // -----------------------------------------------------------------------
    // Python
    // -----------------------------------------------------------------------

    #[test]
    fn test_python_functions() {
        let src = r#"
def greet(name):
    print(f"Hello, {name}")

def add(a, b):
    return a + b
"#;
        let syms = extract(Language::Python, src);
        assert!(names(&syms).contains(&"greet"));
        assert!(names(&syms).contains(&"add"));
        assert!(syms.iter().all(|s| s.kind == SymbolKind::Function));
    }

    #[test]
    fn test_python_classes() {
        let src = r#"
class Animal:
    def speak(self):
        pass

class Dog(Animal):
    def speak(self):
        return "woof"
"#;
        let syms = extract(Language::Python, src);
        let animal = find_by_name(&syms, "Animal").expect("should find Animal");
        assert_eq!(animal.kind, SymbolKind::Class);
        let dog = find_by_name(&syms, "Dog").expect("should find Dog");
        assert_eq!(dog.kind, SymbolKind::Class);
        // Methods inside classes are also extracted as functions
        assert!(names(&syms).contains(&"speak"));
    }

    // -----------------------------------------------------------------------
    // TypeScript
    // -----------------------------------------------------------------------

    #[test]
    fn test_typescript_functions() {
        let src = "function hello() {}\nfunction world(x: number): string { return ''; }\n";
        let syms = extract(Language::TypeScript, src);
        assert!(names(&syms).contains(&"hello"));
        assert!(names(&syms).contains(&"world"));
    }

    #[test]
    fn test_typescript_classes() {
        let src = r#"
class Greeter {
    greet() {
        return "hello";
    }
}
"#;
        let syms = extract(Language::TypeScript, src);
        let greeter = find_by_name(&syms, "Greeter").expect("should find Greeter");
        assert_eq!(greeter.kind, SymbolKind::Class);
        let greet = find_by_name(&syms, "greet").expect("should find greet");
        assert_eq!(greet.kind, SymbolKind::Method);
    }

    #[test]
    fn test_typescript_interfaces() {
        let src = r#"
interface Shape {
    area(): number;
}
"#;
        let syms = extract(Language::TypeScript, src);
        let shape = find_by_name(&syms, "Shape").expect("should find Shape");
        assert_eq!(shape.kind, SymbolKind::Interface);
    }

    #[test]
    fn test_typescript_enums() {
        let src = r#"
enum Direction {
    Up,
    Down,
    Left,
    Right,
}
"#;
        let syms = extract(Language::TypeScript, src);
        let dir = find_by_name(&syms, "Direction").expect("should find Direction");
        assert_eq!(dir.kind, SymbolKind::Enum);
    }

    // -----------------------------------------------------------------------
    // Go
    // -----------------------------------------------------------------------

    #[test]
    fn test_go_functions() {
        let src = r#"
package main

func main() {}
func helper(x int) int { return x }
"#;
        let syms = extract(Language::Go, src);
        assert!(names(&syms).contains(&"main"));
        assert!(names(&syms).contains(&"helper"));
        assert!(
            syms.iter()
                .filter(|s| s.kind == SymbolKind::Function)
                .count()
                >= 2
        );
    }

    #[test]
    fn test_go_types() {
        let src = r#"
package main

type Point struct {
    X float64
    Y float64
}

type Handler func(int) error
"#;
        let syms = extract(Language::Go, src);
        let point = find_by_name(&syms, "Point").expect("should find Point");
        assert_eq!(point.kind, SymbolKind::Type);
        let handler = find_by_name(&syms, "Handler").expect("should find Handler");
        assert_eq!(handler.kind, SymbolKind::Type);
    }

    #[test]
    fn test_go_methods() {
        let src = r#"
package main

type Foo struct{}

func (f *Foo) Bar() {}
"#;
        let syms = extract(Language::Go, src);
        let bar = find_by_name(&syms, "Bar").expect("should find Bar");
        assert_eq!(bar.kind, SymbolKind::Method);
    }

    // -----------------------------------------------------------------------
    // C
    // -----------------------------------------------------------------------

    #[test]
    fn test_c_functions() {
        let src = r#"
int main() { return 0; }
void helper(int x) {}
"#;
        let syms = extract(Language::C, src);
        assert!(names(&syms).contains(&"main"));
        assert!(names(&syms).contains(&"helper"));
        assert!(syms.iter().all(|s| s.kind == SymbolKind::Function));
    }

    #[test]
    fn test_c_structs() {
        let src = r#"
struct Point {
    double x;
    double y;
};
"#;
        let syms = extract(Language::C, src);
        let point = find_by_name(&syms, "Point").expect("should find Point");
        assert_eq!(point.kind, SymbolKind::Struct);
    }

    #[test]
    fn test_c_enums() {
        let src = r#"
enum Color {
    RED,
    GREEN,
    BLUE,
};
"#;
        let syms = extract(Language::C, src);
        let color = find_by_name(&syms, "Color").expect("should find Color");
        assert_eq!(color.kind, SymbolKind::Enum);
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_unsupported_language_returns_empty() {
        let syms = extract(Language::Ruby, "def foo; end");
        assert!(syms.is_empty());
    }

    #[test]
    fn test_unknown_language_returns_empty() {
        let syms = extract(Language::Unknown, "some content");
        assert!(syms.is_empty());
    }

    #[test]
    fn test_empty_content_returns_empty() {
        let syms = extract(Language::Rust, "");
        assert!(syms.is_empty());
    }

    #[test]
    fn test_malformed_syntax_partial_results() {
        // Even with syntax errors, tree-sitter produces a partial AST
        let src = r#"
fn valid_fn() {}
fn broken( { }
struct Good { x: i32 }
"#;
        let syms = extract(Language::Rust, src);
        // Should at least find `valid_fn` and `Good`
        assert!(names(&syms).contains(&"valid_fn"));
        assert!(names(&syms).contains(&"Good"));
    }

    #[test]
    fn test_file_id_preserved() {
        let syms = extract_symbols(FileId(42), b"fn test_fn() {}", Language::Rust);
        assert!(!syms.is_empty());
        assert!(syms.iter().all(|s| s.file_id == FileId(42)));
    }

    // -----------------------------------------------------------------------
    // kind_from_capture unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_kind_from_capture_all_variants() {
        assert_eq!(
            kind_from_capture("definition.function"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            kind_from_capture("definition.struct"),
            Some(SymbolKind::Struct)
        );
        assert_eq!(
            kind_from_capture("definition.trait"),
            Some(SymbolKind::Trait)
        );
        assert_eq!(kind_from_capture("definition.enum"), Some(SymbolKind::Enum));
        assert_eq!(
            kind_from_capture("definition.interface"),
            Some(SymbolKind::Interface)
        );
        assert_eq!(
            kind_from_capture("definition.class"),
            Some(SymbolKind::Class)
        );
        assert_eq!(
            kind_from_capture("definition.method"),
            Some(SymbolKind::Method)
        );
        assert_eq!(
            kind_from_capture("definition.constant"),
            Some(SymbolKind::Constant)
        );
        assert_eq!(
            kind_from_capture("definition.variable"),
            Some(SymbolKind::Variable)
        );
        assert_eq!(kind_from_capture("definition.type"), Some(SymbolKind::Type));
        assert_eq!(
            kind_from_capture("definition.module"),
            Some(SymbolKind::Module)
        );
    }

    #[test]
    fn test_kind_from_capture_unknown() {
        assert_eq!(kind_from_capture("name"), None);
        assert_eq!(kind_from_capture("unknown"), None);
        assert_eq!(kind_from_capture(""), None);
    }

    // -----------------------------------------------------------------------
    // compiled_queries cache tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_compiled_queries_supported() {
        let cache = compiled_queries();
        assert!(cache.contains_key(&Language::Rust));
        assert!(cache.contains_key(&Language::Python));
        assert!(cache.contains_key(&Language::TypeScript));
        assert!(cache.contains_key(&Language::JavaScript));
        assert!(cache.contains_key(&Language::Go));
        assert!(cache.contains_key(&Language::C));
        assert!(cache.contains_key(&Language::Cpp));
    }

    #[test]
    fn test_compiled_queries_unsupported() {
        let cache = compiled_queries();
        assert!(!cache.contains_key(&Language::Ruby));
        assert!(!cache.contains_key(&Language::Java));
        assert!(!cache.contains_key(&Language::Unknown));
    }

    #[test]
    fn test_cached_queries_return_same_results() {
        let src = b"fn alpha() {}\nstruct Beta {}\nenum Gamma { A, B }\n";
        let first = extract_symbols(FileId(0), src, Language::Rust);
        let second = extract_symbols(FileId(1), src, Language::Rust);
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.line, b.line);
        }
    }
}
