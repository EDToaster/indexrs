//! Tree-sitter based symbol extraction for supported languages.
//!
//! Walks the AST produced by tree-sitter to find definitions (functions, structs,
//! classes, traits, enums, etc.) and returns them as [`SymbolEntry`] values.
//! This module is gated behind the `symbols` cargo feature.

use std::cell::RefCell;
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

(source_file
  (var_declaration
    (var_spec
      name: (identifier) @name) @definition.variable))

(source_file
  (var_declaration
    (var_spec_list
      (var_spec
        name: (identifier) @name) @definition.variable)))

(source_file
  (const_declaration
    (const_spec
      name: (identifier) @name) @definition.constant))
"#;

/// Tree-sitter query for Ruby symbol definitions.
const RUBY_QUERY: &str = r#"
(method
  name: (identifier) @name) @definition.function

(singleton_method
  name: (identifier) @name) @definition.method

(class
  name: (constant) @name) @definition.class

(module
  name: (constant) @name) @definition.module

(assignment
  left: (constant) @name) @definition.constant
"#;

/// Tree-sitter query for Java symbol definitions.
const JAVA_QUERY: &str = r#"
(method_declaration
  name: (identifier) @name) @definition.method

(class_declaration
  name: (identifier) @name) @definition.class

(interface_declaration
  name: (identifier) @name) @definition.interface

(enum_declaration
  name: (identifier) @name) @definition.enum

(constructor_declaration
  name: (identifier) @name) @definition.function
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
            (Language::Ruby, RUBY_QUERY),
            (Language::Java, JAVA_QUERY),
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

thread_local! {
    static PARSER_CACHE: RefCell<HashMap<Language, tree_sitter::Parser>> =
        RefCell::new(HashMap::new());
}

/// Parse content using a thread-local cached parser for the given language.
fn parse_with_cached_parser(
    content: &[u8],
    language: Language,
    ts_lang: &tree_sitter::Language,
) -> Option<tree_sitter::Tree> {
    PARSER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let parser = cache.entry(language).or_insert_with(|| {
            let mut p = tree_sitter::Parser::new();
            let _ = p.set_language(ts_lang);
            p
        });
        parser.parse(content, None)
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

    let tree = match parse_with_cached_parser(content, language, &ts_lang) {
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

    #[test]
    fn test_go_var_standalone() {
        let src = r#"
package main

var Config = map[string]string{}
var Version string
"#;
        let syms = extract(Language::Go, src);
        let config = find_by_name(&syms, "Config").expect("should find Config");
        assert_eq!(config.kind, SymbolKind::Variable);
        let version = find_by_name(&syms, "Version").expect("should find Version");
        assert_eq!(version.kind, SymbolKind::Variable);
    }

    #[test]
    fn test_go_const_standalone() {
        let src = r#"
package main

const MaxRetries = 3
const DefaultTimeout = 30
"#;
        let syms = extract(Language::Go, src);
        let max = find_by_name(&syms, "MaxRetries").expect("should find MaxRetries");
        assert_eq!(max.kind, SymbolKind::Constant);
        let timeout = find_by_name(&syms, "DefaultTimeout").expect("should find DefaultTimeout");
        assert_eq!(timeout.kind, SymbolKind::Constant);
    }

    #[test]
    fn test_go_var_block() {
        let src = r#"
package main

var (
    PascalCaseVar   = "hello"
    SCREAMING_VAR   = "world"
    camelCaseVar    = 42
    DatabaseURL     = "postgres://localhost"
)
"#;
        let syms = extract(Language::Go, src);
        let pascal = find_by_name(&syms, "PascalCaseVar").expect("should find PascalCaseVar");
        assert_eq!(pascal.kind, SymbolKind::Variable);
        let screaming = find_by_name(&syms, "SCREAMING_VAR").expect("should find SCREAMING_VAR");
        assert_eq!(screaming.kind, SymbolKind::Variable);
        let camel = find_by_name(&syms, "camelCaseVar").expect("should find camelCaseVar");
        assert_eq!(camel.kind, SymbolKind::Variable);
        let db = find_by_name(&syms, "DatabaseURL").expect("should find DatabaseURL");
        assert_eq!(db.kind, SymbolKind::Variable);
    }

    #[test]
    fn test_go_const_block() {
        let src = r#"
package main

const (
    SAMPLE_RECORD_ALL_METHODS string = "all_methods"
    MaxRetries                       = 3
    DefaultTimeout                   = 30
    API_VERSION                      = "v2"
)
"#;
        let syms = extract(Language::Go, src);
        let sample = find_by_name(&syms, "SAMPLE_RECORD_ALL_METHODS")
            .expect("should find SAMPLE_RECORD_ALL_METHODS");
        assert_eq!(sample.kind, SymbolKind::Constant);
        let max = find_by_name(&syms, "MaxRetries").expect("should find MaxRetries");
        assert_eq!(max.kind, SymbolKind::Constant);
        let timeout = find_by_name(&syms, "DefaultTimeout").expect("should find DefaultTimeout");
        assert_eq!(timeout.kind, SymbolKind::Constant);
        let api = find_by_name(&syms, "API_VERSION").expect("should find API_VERSION");
        assert_eq!(api.kind, SymbolKind::Constant);
    }

    #[test]
    fn test_go_var_block_with_complex_values() {
        // Modeled after real-world config.go patterns
        let src = r#"
package app

var (
    FullSyncCleanupHeartbeatTimeout = 30
    OrangeItemsReplica0Url          = "http://localhost"
    EnableNewFeatureFlag            = false
)
"#;
        let syms = extract(Language::Go, src);
        let timeout = find_by_name(&syms, "FullSyncCleanupHeartbeatTimeout")
            .expect("should find FullSyncCleanupHeartbeatTimeout");
        assert_eq!(timeout.kind, SymbolKind::Variable);
        let url = find_by_name(&syms, "OrangeItemsReplica0Url")
            .expect("should find OrangeItemsReplica0Url");
        assert_eq!(url.kind, SymbolKind::Variable);
        let flag =
            find_by_name(&syms, "EnableNewFeatureFlag").expect("should find EnableNewFeatureFlag");
        assert_eq!(flag.kind, SymbolKind::Variable);
    }

    #[test]
    fn test_go_local_vars_not_indexed() {
        let src = r#"
package main

func main() {
    var localVar = "should not appear"
    const localConst = 42
}
"#;
        let syms = extract(Language::Go, src);
        assert!(
            find_by_name(&syms, "localVar").is_none(),
            "local var should not be indexed"
        );
        assert!(
            find_by_name(&syms, "localConst").is_none(),
            "local const should not be indexed"
        );
        // The function itself should still be found
        assert!(find_by_name(&syms, "main").is_some());
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
    // Ruby
    // -----------------------------------------------------------------------

    #[test]
    fn test_ruby_methods() {
        let src = r#"
def greet(name)
  puts "Hello, #{name}"
end

def add(a, b)
  a + b
end
"#;
        let syms = extract(Language::Ruby, src);
        assert!(names(&syms).contains(&"greet"));
        assert!(names(&syms).contains(&"add"));
        assert!(syms.iter().all(|s| s.kind == SymbolKind::Function));
    }

    #[test]
    fn test_ruby_classes() {
        let src = r#"
class Animal
  def speak
    nil
  end
end

class Dog < Animal
  def speak
    "woof"
  end
end
"#;
        let syms = extract(Language::Ruby, src);
        let animal = find_by_name(&syms, "Animal").expect("should find Animal");
        assert_eq!(animal.kind, SymbolKind::Class);
        let dog = find_by_name(&syms, "Dog").expect("should find Dog");
        assert_eq!(dog.kind, SymbolKind::Class);
        assert!(names(&syms).contains(&"speak"));
    }

    #[test]
    fn test_ruby_modules() {
        let src = r#"
module Serializable
  def serialize
    to_s
  end
end
"#;
        let syms = extract(Language::Ruby, src);
        let serializable = find_by_name(&syms, "Serializable").expect("should find Serializable");
        assert_eq!(serializable.kind, SymbolKind::Module);
    }

    #[test]
    fn test_ruby_singleton_methods() {
        let src = r#"
class Config
  def self.load
    new
  end
end
"#;
        let syms = extract(Language::Ruby, src);
        let load = find_by_name(&syms, "load").expect("should find load");
        assert_eq!(load.kind, SymbolKind::Method);
    }

    #[test]
    fn test_ruby_constants() {
        let src = r#"
MAX_RETRIES = 3
DEFAULT_HOST = "localhost"
"#;
        let syms = extract(Language::Ruby, src);
        let max = find_by_name(&syms, "MAX_RETRIES").expect("should find MAX_RETRIES");
        assert_eq!(max.kind, SymbolKind::Constant);
        let host = find_by_name(&syms, "DEFAULT_HOST").expect("should find DEFAULT_HOST");
        assert_eq!(host.kind, SymbolKind::Constant);
    }

    // -----------------------------------------------------------------------
    // Java
    // -----------------------------------------------------------------------

    #[test]
    fn test_java_classes() {
        let src = r#"
public class Animal {
    public void speak() {}
}
"#;
        let syms = extract(Language::Java, src);
        let animal = find_by_name(&syms, "Animal").expect("should find Animal");
        assert_eq!(animal.kind, SymbolKind::Class);
    }

    #[test]
    fn test_java_methods() {
        let src = r#"
public class Calculator {
    public int add(int a, int b) {
        return a + b;
    }

    public int subtract(int a, int b) {
        return a - b;
    }
}
"#;
        let syms = extract(Language::Java, src);
        assert!(names(&syms).contains(&"add"));
        assert!(names(&syms).contains(&"subtract"));
        let add = find_by_name(&syms, "add").expect("should find add");
        assert_eq!(add.kind, SymbolKind::Method);
    }

    #[test]
    fn test_java_interfaces() {
        let src = r#"
public interface Shape {
    double area();
}
"#;
        let syms = extract(Language::Java, src);
        let shape = find_by_name(&syms, "Shape").expect("should find Shape");
        assert_eq!(shape.kind, SymbolKind::Interface);
    }

    #[test]
    fn test_java_enums() {
        let src = r#"
public enum Direction {
    NORTH, SOUTH, EAST, WEST
}
"#;
        let syms = extract(Language::Java, src);
        let dir = find_by_name(&syms, "Direction").expect("should find Direction");
        assert_eq!(dir.kind, SymbolKind::Enum);
    }

    #[test]
    fn test_java_constructors() {
        let src = r#"
public class Point {
    private int x, y;
    public Point(int x, int y) {
        this.x = x;
        this.y = y;
    }
}
"#;
        let syms = extract(Language::Java, src);
        let point_syms: Vec<_> = syms.iter().filter(|s| s.name == "Point").collect();
        assert!(
            point_syms.len() >= 2,
            "should find both class and constructor for Point"
        );
        assert!(point_syms.iter().any(|s| s.kind == SymbolKind::Class));
        assert!(point_syms.iter().any(|s| s.kind == SymbolKind::Function));
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_unsupported_language_returns_empty() {
        let syms = extract(Language::Haskell, "main = putStrLn \"hello\"");
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
        assert!(cache.contains_key(&Language::Ruby));
        assert!(cache.contains_key(&Language::Java));
    }

    #[test]
    fn test_compiled_queries_unsupported() {
        let cache = compiled_queries();
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

    #[test]
    fn test_parser_reuse_across_languages() {
        // Extract from multiple languages in sequence to exercise parser reuse.
        let rust_src = b"fn hello() {}";
        let py_src = b"def world():\n    pass\n";
        let go_src = b"package main\nfunc foo() {}\n";

        let r = extract_symbols(FileId(0), rust_src, Language::Rust);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "hello");

        let p = extract_symbols(FileId(1), py_src, Language::Python);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].name, "world");

        let g = extract_symbols(FileId(2), go_src, Language::Go);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].name, "foo");

        // Back to Rust — parser must switch back correctly
        let r2 = extract_symbols(FileId(3), b"struct Bar;", Language::Rust);
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0].name, "Bar");
    }
}
