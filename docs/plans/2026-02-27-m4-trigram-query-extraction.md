# Trigram Extraction from Query Patterns Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a `query_trigrams` module in `ferret-indexer-core` that extracts trigram sets from parsed `Query` ASTs to drive efficient index lookups. The module handles literal queries (direct trigram extraction), regex queries (literal fragment extraction via `regex-syntax` HIR analysis), phrase queries, boolean combinators (AND/OR/NOT), and graceful fallback when no trigrams can be extracted.

**Architecture:** A new module `query_trigrams.rs` in `ferret-indexer-core` containing:
- `TrigramQuery` enum representing the trigram lookup strategy for a query (AND-set, OR-of-AND-sets, or full-scan fallback)
- `extract_query_trigrams(query: &Query) -> TrigramQuery` as the main entry point, performing recursive case analysis over the `Query` AST
- `extract_regex_literals(pattern: &str) -> Vec<Vec<u8>>` for extracting required literal byte sequences from regex patterns using `regex-syntax::hir::literal::Extractor`
- Integration with existing `extract_unique_trigrams_folded()` from `trigram.rs` for converting literal bytes to `HashSet<Trigram>` (always lowercase, matching the case-folded index)

The module bridges the query parser (HHC-46) and query planner (HHC-48): the parser produces a `Query` AST, this module extracts trigrams from it, and the planner uses those trigrams to look up posting lists and build an execution plan.

**Tech Stack:** Rust 2024, `regex-syntax` 0.8.x (already a transitive dependency of `regex` 1.x -- will be added as a direct dependency), existing `ferret-indexer-core` modules (`trigram`, `types`), `tempfile` (dev)

**Dependencies:** This plan depends on the `Query` AST type from HHC-46 (query parser). The confirmed AST shape from the parser-planner agent:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// Bare text literal substring match (case-insensitive by default)
    Literal(LiteralQuery),
    /// /pattern/ regex match
    Regex(RegexQuery),
    /// "exact phrase" - exact phrase match (case-sensitive)
    Phrase(PhraseQuery),
    /// path:prefix - path prefix filter
    PathFilter(String),
    /// language:rust / lang:rs - language filter
    LanguageFilter(Language),
    /// case:yes - modifies the query to be case-sensitive
    CaseSensitive(Box<Query>),
    /// NOT term - exclusion
    Not(Box<Query>),
    /// term1 OR term2 - union
    Or(Box<Query>, Box<Query>),
    /// Implicit AND between space-separated terms
    And(Vec<Query>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiteralQuery {
    pub text: String,
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegexQuery {
    pub pattern: String,
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhraseQuery {
    pub text: String,
    pub case_sensitive: bool,
}
```

**Prerequisite:** ASCII case-fold trigrams plan (`2026-02-27-ascii-casefold-trigrams.md`) must be implemented first. The index stores lowercase-folded trigrams (A-Z → a-z at index time).

**Case-sensitivity handling for trigram extraction (SIMPLIFIED by case-folded index):**
- **All trigram extraction produces lowercase trigrams** regardless of the `case_sensitive` flag on query types. This is because the index itself stores only lowercase trigrams.
- The `case_sensitive` flag affects **verification only** (HHC-49), not trigram lookup.
- `extract_literal_trigrams` no longer needs a `case_sensitive` parameter — it always folds to lowercase via `extract_unique_trigrams_folded()`.
- `CaseSensitive(inner)` has no effect on trigram extraction — it only affects verification. The extraction function ignores it and recurses into the inner query normally.
- This means case-sensitive searches find the same candidates as case-insensitive ones (via folded trigrams), then filter more strictly during verification. The extra false positives are minimal and always correct.

**Note on `Or` shape:** The AST uses binary `Or(Box<Query>, Box<Query>)` instead of n-ary `Or(Vec<Query>)`. The extraction logic handles this by recursively extracting from both branches and collecting into a `Vec` of trigram sets for `TrigramQuery::Any`.

---

## Task 1: Add `regex-syntax` as a direct dependency

**Files:**
- Modify: `ferret-indexer-core/Cargo.toml`

### Step 1: Add `regex-syntax` to `[dependencies]`

`regex-syntax` 0.8.x is already a transitive dependency (via `regex` 1.x -> `regex-syntax` 0.8.10). Add it as a direct dependency to use its HIR literal extraction API.

Add to `ferret-indexer-core/Cargo.toml` under `[dependencies]`:

```toml
regex-syntax = "0.8"
```

### Step 2: Verify it compiles

Run: `cargo check -p ferret-indexer-core`

### Acceptance Criteria
- `regex-syntax` is listed in `Cargo.toml` as a direct dependency
- `cargo check -p ferret-indexer-core` passes
- No version conflicts (the version should resolve to the same 0.8.x already in the lock file)

---

## Task 2: Define the `TrigramQuery` type and stub module

**Files:**
- Create: `ferret-indexer-core/src/query_trigrams.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Create the module with type definitions

Create `ferret-indexer-core/src/query_trigrams.rs`:

```rust
//! Trigram extraction from parsed query ASTs.
//!
//! This module converts a [`Query`] AST into a [`TrigramQuery`] that describes
//! which trigrams to look up in the index. The query planner uses the extracted
//! trigrams to fetch posting lists and build an execution plan.
//!
//! # Extraction Strategy
//!
//! - **Literal/Phrase queries**: Extract all trigrams directly from the search string.
//!   All trigrams must match (AND semantics).
//! - **Regex queries**: Parse the regex with `regex-syntax`, extract required literal
//!   fragments from the HIR, then extract trigrams from those fragments.
//! - **AND queries**: Merge (union) trigram sets from all children -- a file must
//!   contain trigrams from every child.
//! - **OR queries**: Keep trigram sets separate -- a file matching any branch suffices.
//! - **NOT queries**: Cannot use trigrams for pruning (negation inverts the set).
//! - **Filter queries** (Symbol, Path, Language): These don't produce content trigrams;
//!   they're handled by other index types.
//!
//! When no trigrams can be extracted (short queries, wildcard-only regex, NOT-only
//! queries), the result is [`TrigramQuery::None`], signaling the planner to fall
//! back to a full file scan.

use std::collections::HashSet;

use crate::types::Trigram;

/// Describes the trigram lookup strategy for a parsed query.
///
/// The query planner uses this to decide how to query the trigram index:
/// - `All`: intersect posting lists for all trigrams (AND semantics)
/// - `Any`: union the results of multiple `All` sets (OR semantics)
/// - `None`: no trigrams available, must scan all files
#[derive(Debug, Clone, PartialEq)]
pub enum TrigramQuery {
    /// All trigrams must be present in a file (AND intersection).
    /// Used for literal, phrase, and AND-combined queries.
    All(HashSet<Trigram>),

    /// At least one branch's trigram set must match (OR union).
    /// Each inner `HashSet<Trigram>` is an AND-set; the outer Vec is OR'd.
    /// A file is a candidate if it matches ALL trigrams in ANY one branch.
    Any(Vec<HashSet<Trigram>>),

    /// No trigrams could be extracted. The planner must fall back to
    /// scanning all files. This occurs for:
    /// - Queries shorter than 3 characters
    /// - Regex patterns with no required literal substrings (e.g., `.*`)
    /// - NOT-only queries
    /// - Filter-only queries (PathFilter, LanguageFilter)
    None,
}

impl TrigramQuery {
    /// Returns `true` if this is `TrigramQuery::None`.
    pub fn is_none(&self) -> bool {
        matches!(self, TrigramQuery::None)
    }

    /// Returns the total number of unique trigrams across all branches.
    /// Useful for cost estimation in the query planner.
    pub fn trigram_count(&self) -> usize {
        match self {
            TrigramQuery::All(set) => set.len(),
            TrigramQuery::Any(branches) => {
                branches.iter().map(|s| s.len()).sum()
            }
            TrigramQuery::None => 0,
        }
    }
}
```

### Step 2: Register the module in `lib.rs`

Add to `ferret-indexer-core/src/lib.rs`:

```rust
pub mod query_trigrams;
```

And add to the `pub use` section:

```rust
pub use query_trigrams::TrigramQuery;
```

### Step 3: Write unit tests for `TrigramQuery`

Add to `query_trigrams.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Trigram;

    #[test]
    fn test_trigram_query_none_is_none() {
        assert!(TrigramQuery::None.is_none());
    }

    #[test]
    fn test_trigram_query_all_is_not_none() {
        let mut set = HashSet::new();
        set.insert(Trigram::from_bytes(b'a', b'b', b'c'));
        assert!(!TrigramQuery::All(set).is_none());
    }

    #[test]
    fn test_trigram_query_count_all() {
        let mut set = HashSet::new();
        set.insert(Trigram::from_bytes(b'a', b'b', b'c'));
        set.insert(Trigram::from_bytes(b'b', b'c', b'd'));
        assert_eq!(TrigramQuery::All(set).trigram_count(), 2);
    }

    #[test]
    fn test_trigram_query_count_any() {
        let mut s1 = HashSet::new();
        s1.insert(Trigram::from_bytes(b'a', b'b', b'c'));
        let mut s2 = HashSet::new();
        s2.insert(Trigram::from_bytes(b'x', b'y', b'z'));
        s2.insert(Trigram::from_bytes(b'y', b'z', b'w'));
        assert_eq!(TrigramQuery::Any(vec![s1, s2]).trigram_count(), 3);
    }

    #[test]
    fn test_trigram_query_count_none() {
        assert_eq!(TrigramQuery::None.trigram_count(), 0);
    }
}
```

### Step 4: Verify

Run: `cargo test -p ferret-indexer-core -- query_trigrams`

### Acceptance Criteria
- `TrigramQuery` enum is defined with `All`, `Any`, `None` variants
- Module is registered in `lib.rs` and exported
- All unit tests pass
- `cargo clippy -p ferret-indexer-core -- -D warnings` passes

---

## Task 3: Implement trigram extraction for literal and phrase queries

**Files:**
- Modify: `ferret-indexer-core/src/query_trigrams.rs`

This task implements the simplest case: extracting trigrams from plain literal strings and exact phrases. These are the most common query types. Thanks to the case-folded index, the function does NOT need a `case_sensitive` parameter — all trigram extraction always produces lowercase trigrams to match the index.

### Step 1: Write failing tests

Add tests to `query_trigrams.rs`:

```rust
#[test]
fn test_extract_literal_trigrams_lowercase() {
    // "httprequest" -> 9 trigrams, all lowercase
    let result = extract_literal_trigrams("httprequest");
    match result {
        TrigramQuery::All(set) => {
            assert_eq!(set.len(), 9);
            assert!(set.contains(&Trigram::from_bytes(b'h', b't', b't')));
            assert!(set.contains(&Trigram::from_bytes(b'e', b's', b't')));
        }
        _ => panic!("expected TrigramQuery::All"),
    }
}

#[test]
fn test_extract_literal_trigrams_mixed_case_folded() {
    // "HttpRequest" -> folded to lowercase trigrams: htt, ttp, tpr, pre, req, equ, que, ues, est
    // Same trigrams as "httprequest" since index is case-folded
    let result = extract_literal_trigrams("HttpRequest");
    match result {
        TrigramQuery::All(set) => {
            assert_eq!(set.len(), 9);
            // All trigrams are lowercase (folded)
            assert!(set.contains(&Trigram::from_bytes(b'h', b't', b't')));
            assert!(set.contains(&Trigram::from_bytes(b'e', b's', b't')));
            // No uppercase trigrams
            assert!(!set.contains(&Trigram::from_bytes(b'H', b't', b't')));
        }
        _ => panic!("expected TrigramQuery::All"),
    }
}

#[test]
fn test_extract_literal_trigrams_short() {
    // "fn" is only 2 chars -> no trigrams -> None
    assert_eq!(extract_literal_trigrams("fn"), TrigramQuery::None);
}

#[test]
fn test_extract_literal_trigrams_exact_three() {
    // "abc" -> exactly 1 trigram
    let result = extract_literal_trigrams("abc");
    match result {
        TrigramQuery::All(set) => {
            assert_eq!(set.len(), 1);
            assert!(set.contains(&Trigram::from_bytes(b'a', b'b', b'c')));
        }
        _ => panic!("expected TrigramQuery::All"),
    }
}

#[test]
fn test_extract_literal_trigrams_empty() {
    assert_eq!(extract_literal_trigrams(""), TrigramQuery::None);
}

#[test]
fn test_extract_literal_trigrams_deduplicates() {
    // "aaaa" has trigrams: "aaa", "aaa" -> deduplicated to 1
    let result = extract_literal_trigrams("aaaa");
    match result {
        TrigramQuery::All(set) => assert_eq!(set.len(), 1),
        _ => panic!("expected TrigramQuery::All"),
    }
}
```

### Step 2: Implement `extract_literal_trigrams`

Add to `query_trigrams.rs`:

```rust
use crate::trigram::extract_unique_trigrams_folded;

/// Extract trigrams from a literal search string.
///
/// Always produces lowercase (ASCII-folded) trigrams to match the case-folded
/// index. The `case_sensitive` flag on query types affects verification only,
/// not trigram extraction.
///
/// Returns `TrigramQuery::All` with the unique trigram set if the string
/// is at least 3 bytes long, or `TrigramQuery::None` if too short.
pub fn extract_literal_trigrams(text: &str) -> TrigramQuery {
    let bytes = text.as_bytes();
    if bytes.len() < 3 {
        return TrigramQuery::None;
    }
    let trigrams = extract_unique_trigrams_folded(bytes);
    if trigrams.is_empty() {
        TrigramQuery::None
    } else {
        TrigramQuery::All(trigrams)
    }
}
```

**Note on case-folded index:** The index stores ASCII-folded trigrams (A-Z → a-z). `extract_literal_trigrams` uses `extract_unique_trigrams_folded()` to produce matching lowercase trigrams. This means both "HttpRequest" and "httprequest" produce the same trigrams, and both will find the same candidate files. The `case_sensitive` flag on query types only affects the verification step (HHC-49), not trigram lookup.

1. **Index-side**: During indexing, store both original and lowercased trigrams. This doubles the index size.
2. **Query-side**: For case-insensitive queries, fall back to scanning more broadly. Extract lowercased trigrams and let verification handle case folding.

### Step 3: Verify

Run: `cargo test -p ferret-indexer-core -- query_trigrams`

### Acceptance Criteria
- `extract_literal_trigrams` correctly extracts trigrams from strings >= 3 bytes
- Always produces lowercase (ASCII-folded) trigrams regardless of input case
- "HttpRequest" and "httprequest" produce identical trigram sets
- Returns `TrigramQuery::None` for strings < 3 bytes
- Trigrams are deduplicated (uses `extract_unique_trigrams_folded`)
- All tests pass

---

## Task 4: Implement regex literal extraction via `regex-syntax`

**Files:**
- Modify: `ferret-indexer-core/src/query_trigrams.rs`

This is the most complex task. We use `regex-syntax` to parse a regex pattern into HIR, then use the `hir::literal::Extractor` to find required literal byte sequences, and finally extract trigrams from those literals.

### Step 1: Write failing tests

```rust
#[test]
fn test_extract_regex_trigrams_simple_literal() {
    // /HttpRequest/ -> same as literal "HttpRequest"
    let result = extract_regex_trigrams("HttpRequest");
    match result {
        TrigramQuery::All(set) => {
            assert_eq!(set.len(), 9);
            assert!(set.contains(&Trigram::from_bytes(b'H', b't', b't')));
        }
        _ => panic!("expected TrigramQuery::All"),
    }
}

#[test]
fn test_extract_regex_trigrams_with_wildcard() {
    // /Err(.*Error)/ -> literals "Err" and "Error"
    // "Err" trigrams: Err (1 trigram)
    // "Error" trigrams: Err, rro, ror (3 trigrams)
    // Combined: Err, rro, ror (3 unique)
    let result = extract_regex_trigrams(r"Err\(.*Error\)");
    match result {
        TrigramQuery::All(set) => {
            assert!(set.len() >= 1); // At least "Err" trigram
            assert!(set.contains(&Trigram::from_bytes(b'E', b'r', b'r')));
        }
        _ => panic!("expected TrigramQuery::All, got {:?}", result),
    }
}

#[test]
fn test_extract_regex_trigrams_alternation() {
    // /foo|bar/ -> OR of trigrams from "foo" and "bar"
    let result = extract_regex_trigrams("foo|bar");
    match result {
        TrigramQuery::Any(branches) => {
            assert_eq!(branches.len(), 2);
        }
        // Also acceptable: if the extractor sees both as required prefixes,
        // it might return None since neither is individually required.
        // The exact behavior depends on regex-syntax's extractor.
        TrigramQuery::All(_) | TrigramQuery::None => {}
    }
}

#[test]
fn test_extract_regex_trigrams_pure_wildcard() {
    // /.*/ -> no literals -> None
    let result = extract_regex_trigrams(".*");
    assert_eq!(result, TrigramQuery::None);
}

#[test]
fn test_extract_regex_trigrams_short_literal() {
    // /ab/ -> only 2-char literal -> None
    let result = extract_regex_trigrams("ab");
    assert_eq!(result, TrigramQuery::None);
}

#[test]
fn test_extract_regex_trigrams_invalid_regex() {
    // Invalid regex -> None (graceful fallback)
    let result = extract_regex_trigrams("(unclosed");
    assert_eq!(result, TrigramQuery::None);
}
```

### Step 2: Implement `extract_regex_trigrams`

```rust
use regex_syntax::hir::literal::{Extractor, ExtractKind};
use regex_syntax::Parser;

/// Extract trigrams from a regex pattern by analyzing its literal fragments.
///
/// Uses `regex-syntax` to parse the pattern into HIR, then uses the
/// `Extractor` to find required literal byte sequences. Trigrams are
/// extracted from each literal fragment and merged into a single AND-set.
///
/// For alternations (`foo|bar`), the extractor may return separate literal
/// sequences per branch. If no required literals can be extracted (e.g.,
/// pure wildcards `.*`), returns `TrigramQuery::None`.
///
/// Returns `TrigramQuery::None` on parse errors (graceful fallback to full scan).
fn extract_regex_trigrams(pattern: &str) -> TrigramQuery {
    // Parse the regex pattern into HIR
    let hir = match Parser::new().parse(pattern) {
        Ok(hir) => hir,
        Err(_) => return TrigramQuery::None,
    };

    // Extract required literal prefixes from the HIR.
    // We try prefixes first, then interior literals as fallback.
    let mut extractor = Extractor::new();
    extractor.kind(ExtractKind::Prefix);
    let prefix_seq = extractor.extract(&hir);

    // Also try suffix extraction for additional coverage
    let mut extractor_suffix = Extractor::new();
    extractor_suffix.kind(ExtractKind::Suffix);
    let suffix_seq = extractor_suffix.extract(&hir);

    // Collect all literal byte sequences from both prefix and suffix extraction
    let mut all_trigrams: HashSet<Trigram> = HashSet::new();

    // Helper: extract trigrams from a Seq of literals
    let process_seq = |seq: &regex_syntax::hir::literal::Seq, trigrams: &mut HashSet<Trigram>| {
        if let Some(literals) = seq.literals() {
            for lit in literals {
                let bytes = lit.as_bytes();
                if bytes.len() >= 3 {
                    trigrams.extend(extract_unique_trigrams(bytes));
                }
            }
        }
    };

    process_seq(&prefix_seq, &mut all_trigrams);
    process_seq(&suffix_seq, &mut all_trigrams);

    if all_trigrams.is_empty() {
        TrigramQuery::None
    } else {
        TrigramQuery::All(all_trigrams)
    }
}
```

### Step 3: Verify

Run: `cargo test -p ferret-indexer-core -- query_trigrams`

### Step 4: Handle alternation patterns better

If the prefix/suffix extraction doesn't handle alternations well (e.g., `foo|bar`), add special handling:

Check if the HIR top-level is an alternation. If so, extract trigrams from each branch separately and return `TrigramQuery::Any`. This requires inspecting the `Hir` kind:

```rust
use regex_syntax::hir::HirKind;

// After parsing, check for top-level alternation
if let HirKind::Alternation(branches) = hir.kind() {
    let mut branch_trigrams: Vec<HashSet<Trigram>> = Vec::new();
    for branch in branches {
        let mut extractor = Extractor::new();
        extractor.kind(ExtractKind::Prefix);
        let seq = extractor.extract(branch);
        let mut trigrams = HashSet::new();
        // ... extract trigrams from seq ...
        if !trigrams.is_empty() {
            branch_trigrams.push(trigrams);
        }
    }
    if branch_trigrams.is_empty() {
        return TrigramQuery::None;
    }
    if branch_trigrams.len() == 1 {
        return TrigramQuery::All(branch_trigrams.into_iter().next().unwrap());
    }
    return TrigramQuery::Any(branch_trigrams);
}
```

### Acceptance Criteria
- `extract_regex_trigrams` correctly extracts trigrams from regex literal fragments
- Alternation patterns (`foo|bar`) produce `TrigramQuery::Any` with separate branches
- Pure wildcard patterns (`.*`, `.+`, `\d+`) produce `TrigramQuery::None`
- Invalid regex patterns produce `TrigramQuery::None` (no panics)
- Short literal-only patterns (< 3 chars) produce `TrigramQuery::None`
- `cargo clippy -p ferret-indexer-core -- -D warnings` passes

---

## Task 5: Implement the main `extract_query_trigrams` function over the Query AST

**Files:**
- Modify: `ferret-indexer-core/src/query_trigrams.rs`

This task implements the recursive extraction over the full `Query` AST. It depends on the `Query` type from HHC-46. If that type is not yet available, define a local placeholder in `#[cfg(test)]` and use the real type once it lands.

### Step 1: Write failing tests

```rust
// NOTE: These tests use the Query type from the parser module (HHC-46).
// Helper constructors to create Query variants matching the confirmed AST:

fn lit(text: &str) -> Query {
    Query::Literal(LiteralQuery {
        text: text.to_string(),
        case_sensitive: true,
    })
}

fn lit_ci(text: &str) -> Query {
    Query::Literal(LiteralQuery {
        text: text.to_string(),
        case_sensitive: false,
    })
}

fn regex_q(pattern: &str) -> Query {
    Query::Regex(RegexQuery {
        pattern: pattern.to_string(),
        case_sensitive: true,
    })
}

fn phrase(text: &str) -> Query {
    Query::Phrase(PhraseQuery {
        text: text.to_string(),
        case_sensitive: true,
    })
}

#[test]
fn test_extract_query_literal() {
    let query = lit("HttpRequest");
    let result = extract_query_trigrams(&query);
    match result {
        TrigramQuery::All(set) => {
            assert_eq!(set.len(), 9);
            // All trigrams are lowercase (case-folded index)
            assert!(set.contains(&Trigram::from_bytes(b'h', b't', b't')));
            assert!(!set.contains(&Trigram::from_bytes(b'H', b't', b't')));
        }
        _ => panic!("expected All"),
    }
}

#[test]
fn test_extract_query_literal_case_insensitive_same_trigrams() {
    // case_sensitive=true and case_sensitive=false produce SAME trigrams
    // because all extraction uses folded trigrams (index is case-folded)
    let query_cs = lit("HttpRequest");
    let query_ci = lit_ci("HttpRequest");
    let result_cs = extract_query_trigrams(&query_cs);
    let result_ci = extract_query_trigrams(&query_ci);
    match (result_cs, result_ci) {
        (TrigramQuery::All(set_cs), TrigramQuery::All(set_ci)) => {
            assert_eq!(set_cs, set_ci, "case flag should not affect trigram extraction");
        }
        _ => panic!("expected All for both"),
    }
}

#[test]
fn test_extract_query_regex() {
    let query = regex_q(r"Err\(.*Error\)");
    let result = extract_query_trigrams(&query);
    assert!(!result.is_none()); // Should extract some trigrams
}

#[test]
fn test_extract_query_phrase() {
    // Phrase is treated same as Literal for trigram extraction
    let query = phrase("fn main()");
    let result = extract_query_trigrams(&query);
    match result {
        TrigramQuery::All(set) => {
            assert!(set.len() >= 7); // "fn main()" has 7 unique trigrams
        }
        _ => panic!("expected All"),
    }
}

#[test]
fn test_extract_query_and() {
    // AND: merge trigram sets from both children
    let query = Query::And(vec![lit("foo"), lit("bar")]);
    let result = extract_query_trigrams(&query);
    match result {
        TrigramQuery::All(set) => {
            // "foo" -> 1 trigram, "bar" -> 1 trigram = 2 total
            assert_eq!(set.len(), 2);
            assert!(set.contains(&Trigram::from_bytes(b'f', b'o', b'o')));
            assert!(set.contains(&Trigram::from_bytes(b'b', b'a', b'r')));
        }
        _ => panic!("expected All"),
    }
}

#[test]
fn test_extract_query_or() {
    // OR: separate trigram sets for each branch (binary Or)
    let query = Query::Or(Box::new(lit("foo")), Box::new(lit("bar")));
    let result = extract_query_trigrams(&query);
    match result {
        TrigramQuery::Any(branches) => {
            assert_eq!(branches.len(), 2);
        }
        _ => panic!("expected Any"),
    }
}

#[test]
fn test_extract_query_not() {
    // NOT: cannot use trigrams for pruning -> None
    let query = Query::Not(Box::new(lit("deprecated")));
    let result = extract_query_trigrams(&query);
    assert!(result.is_none());
}

#[test]
fn test_extract_query_language_filter() {
    // Language filter: no content trigrams -> None
    let query = Query::LanguageFilter(Language::Rust);
    let result = extract_query_trigrams(&query);
    assert!(result.is_none());
}

#[test]
fn test_extract_query_path_filter() {
    // Path filter: no content trigrams (handled by metadata) -> None
    let query = Query::PathFilter("src/lib".to_string());
    let result = extract_query_trigrams(&query);
    assert!(result.is_none());
}

#[test]
fn test_extract_query_case_sensitive_wrapper_no_effect_on_trigrams() {
    // CaseSensitive wraps another query -> NO effect on trigram extraction
    // (case_sensitive only affects verification, not trigram lookup)
    let query_wrapped = Query::CaseSensitive(Box::new(lit_ci("HttpRequest")));
    let query_plain = lit_ci("HttpRequest");
    let result_wrapped = extract_query_trigrams(&query_wrapped);
    let result_plain = extract_query_trigrams(&query_plain);
    match (result_wrapped, result_plain) {
        (TrigramQuery::All(set_w), TrigramQuery::All(set_p)) => {
            assert_eq!(set_w, set_p, "CaseSensitive wrapper should not change trigrams");
            // Both produce lowercase trigrams
            assert!(set_w.contains(&Trigram::from_bytes(b'h', b't', b't')));
            assert!(!set_w.contains(&Trigram::from_bytes(b'H', b't', b't')));
        }
        _ => panic!("expected All for both"),
    }
}

#[test]
fn test_extract_query_and_with_filter() {
    // AND(Literal("foo"), LanguageFilter(Rust)) -> only "foo" trigrams
    let query = Query::And(vec![
        lit("foo"),
        Query::LanguageFilter(Language::Rust),
    ]);
    let result = extract_query_trigrams(&query);
    match result {
        TrigramQuery::All(set) => {
            assert_eq!(set.len(), 1);
            assert!(set.contains(&Trigram::from_bytes(b'f', b'o', b'o')));
        }
        _ => panic!("expected All"),
    }
}

#[test]
fn test_extract_query_and_with_short_literal() {
    // AND(Literal("fn"), Literal("main")) -> only "main" trigrams (fn too short)
    let query = Query::And(vec![lit("fn"), lit("main")]);
    let result = extract_query_trigrams(&query);
    match result {
        TrigramQuery::All(set) => {
            assert_eq!(set.len(), 2); // "mai" and "ain"
        }
        _ => panic!("expected All"),
    }
}

#[test]
fn test_extract_query_or_with_none_branch() {
    // OR(Literal("foo"), Literal("ab")) -> "ab" has no trigrams
    // Since "ab" branch can't be filtered, whole OR is None
    let query = Query::Or(Box::new(lit("foo")), Box::new(lit("ab")));
    let result = extract_query_trigrams(&query);
    assert!(result.is_none());
}

#[test]
fn test_extract_query_nested_and_or() {
    // AND(OR(Literal("foo"), Literal("bar")), Literal("baz"))
    let query = Query::And(vec![
        Query::Or(Box::new(lit("foo")), Box::new(lit("bar"))),
        lit("baz"),
    ]);
    let result = extract_query_trigrams(&query);
    match result {
        TrigramQuery::Any(branches) => {
            assert_eq!(branches.len(), 2);
            for branch in &branches {
                assert!(branch.contains(&Trigram::from_bytes(b'b', b'a', b'z')));
            }
        }
        _ => panic!("expected Any, got {:?}", result),
    }
}
```

### Step 2: Implement `extract_query_trigrams`

```rust
/// Extract trigrams from a parsed query AST for index lookup.
///
/// Recursively walks the query AST and builds a [`TrigramQuery`] describing
/// which trigrams to look up. The query planner uses this to fetch posting
/// lists from the trigram index.
///
/// # Query Type Handling
///
/// All trigram extraction produces lowercase (ASCII-folded) trigrams to match
/// the case-folded index. The `case_sensitive` flag on query types does NOT
/// affect trigram extraction — it only affects verification (HHC-49).
///
/// | Query Type | Trigram Strategy |
/// |---|---|
/// | `Literal(LiteralQuery)` | Extract folded trigrams from text (AND) |
/// | `Phrase(PhraseQuery)` | Extract folded trigrams from text (AND) |
/// | `Regex(RegexQuery)` | Parse regex, extract literal fragments, extract folded trigrams |
/// | `And(children)` | Merge (union) trigram sets from all children |
/// | `Or(left, right)` | Keep branches separate (any branch match suffices) |
/// | `Not(inner)` | Returns `None` (negation can't prune via trigrams) |
/// | `CaseSensitive(inner)` | Recurse into inner (no effect on trigrams) |
/// | `PathFilter/LanguageFilter` | Returns `None` (handled by other indexes) |
pub fn extract_query_trigrams(query: &Query) -> TrigramQuery {
    match query {
        Query::Literal(lq) => extract_literal_trigrams(&lq.text),

        Query::Phrase(pq) => extract_literal_trigrams(&pq.text),

        Query::Regex(rq) => extract_regex_trigrams(&rq.pattern),

        // Filter-only queries don't produce content trigrams
        Query::PathFilter(_) | Query::LanguageFilter(_) => TrigramQuery::None,

        // NOT: we cannot use trigrams to prune (negation inverts the candidate set)
        Query::Not(_) => TrigramQuery::None,

        // CaseSensitive: just recurse into inner (no effect on trigram extraction)
        Query::CaseSensitive(inner) => extract_query_trigrams(inner),

        Query::And(children) => {
            let mut merged = TrigramQuery::None;
            for child in children {
                let child_tq = extract_query_trigrams(child);
                merged = merge_and(merged, child_tq);
            }
            merged
        }

        // Binary Or: extract from both branches
        Query::Or(left, right) => {
            let left_tq = extract_query_trigrams(left);
            let right_tq = extract_query_trigrams(right);

            let mut branches: Vec<HashSet<Trigram>> = Vec::new();

            for child_tq in [left_tq, right_tq] {
                match child_tq {
                    TrigramQuery::None => {
                        // If any OR branch has no trigrams, we can't prune:
                        // that branch could match any file.
                        return TrigramQuery::None;
                    }
                    TrigramQuery::All(set) => {
                        branches.push(set);
                    }
                    TrigramQuery::Any(inner_branches) => {
                        branches.extend(inner_branches);
                    }
                }
            }

            match branches.len() {
                0 => TrigramQuery::None,
                1 => TrigramQuery::All(branches.into_iter().next().unwrap()),
                _ => TrigramQuery::Any(branches),
            }
        }
    }
}

/// Merge two `TrigramQuery` values with AND semantics.
///
/// AND means a file must satisfy both constraints, so we can union
/// the required trigram sets (more trigrams = more selective).
fn merge_and(a: TrigramQuery, b: TrigramQuery) -> TrigramQuery {
    match (a, b) {
        // If either side has no trigrams, use the other
        (TrigramQuery::None, other) | (other, TrigramQuery::None) => other,

        // Both are ALL: union the trigram sets
        (TrigramQuery::All(mut s1), TrigramQuery::All(s2)) => {
            s1.extend(s2);
            TrigramQuery::All(s1)
        }

        // ALL + Any: distribute the ALL set into every branch
        (TrigramQuery::All(all), TrigramQuery::Any(branches))
        | (TrigramQuery::Any(branches), TrigramQuery::All(all)) => {
            let merged_branches: Vec<HashSet<Trigram>> = branches
                .into_iter()
                .map(|mut branch| {
                    branch.extend(all.iter().copied());
                    branch
                })
                .collect();
            TrigramQuery::Any(merged_branches)
        }

        // Both Any: cross-product (each branch from A merged with each from B)
        // To avoid exponential blowup, limit to a reasonable number of branches.
        (TrigramQuery::Any(a_branches), TrigramQuery::Any(b_branches)) => {
            let mut result_branches = Vec::new();
            for a_branch in &a_branches {
                for b_branch in &b_branches {
                    let mut merged = a_branch.clone();
                    merged.extend(b_branch.iter().copied());
                    result_branches.push(merged);
                    // Cap at 64 branches to prevent blowup
                    if result_branches.len() >= 64 {
                        return TrigramQuery::Any(result_branches);
                    }
                }
            }
            match result_branches.len() {
                0 => TrigramQuery::None,
                1 => TrigramQuery::All(result_branches.into_iter().next().unwrap()),
                _ => TrigramQuery::Any(result_branches),
            }
        }
    }
}
```

### Step 3: Define test-only Query types (temporary, until HHC-46 lands)

If the `Query` type from HHC-46 is not yet available, add temporary definitions inside `#[cfg(test)]` that match the confirmed AST shape from the parser-planner:

```rust
/// Temporary Query types for testing, mirrors the confirmed HHC-46 AST shape.
/// Will be replaced by the real types from the parser module (HHC-46).
#[cfg(test)]
mod test_query_types {
    use crate::types::Language;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Query {
        Literal(LiteralQuery),
        Regex(RegexQuery),
        Phrase(PhraseQuery),
        PathFilter(String),
        LanguageFilter(Language),
        CaseSensitive(Box<Query>),
        Not(Box<Query>),
        Or(Box<Query>, Box<Query>),
        And(Vec<Query>),
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct LiteralQuery {
        pub text: String,
        pub case_sensitive: bool,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct RegexQuery {
        pub pattern: String,
        pub case_sensitive: bool,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct PhraseQuery {
        pub text: String,
        pub case_sensitive: bool,
    }
}

#[cfg(test)]
use test_query_types::*;
```

And for the non-test code, use a cfg-gated import:

```rust
// When the parser module (HHC-46) lands, change this to:
// use crate::query_parser::{Query, LiteralQuery, RegexQuery, PhraseQuery};
#[cfg(not(test))]
use crate::query_parser::{Query, LiteralQuery, RegexQuery, PhraseQuery};
```

**Important:** The `extract_query_trigrams` and `extract_query_trigrams_inner` functions are written against the confirmed `Query` enum shape. When the real parser module lands, the test-only types are removed and the import is updated. The function bodies do not change.

### Step 4: Verify

Run: `cargo test -p ferret-indexer-core -- query_trigrams`

### Acceptance Criteria
- `extract_query_trigrams` handles all `Query` variants correctly
- `Literal(LiteralQuery)` and `Phrase(PhraseQuery)` respect `case_sensitive` flag
- `Regex(RegexQuery)` extracts from pattern literals
- `CaseSensitive(inner)` overrides case-sensitivity on inner queries
- AND queries merge trigram sets (union, more selective)
- Binary `Or(left, right)` keeps branches separate; returns `None` if any branch has no trigrams
- NOT queries return `None`
- Filter queries (`PathFilter`, `LanguageFilter`) return `None`
- Nested AND(OR(...), ...) correctly distributes trigrams
- `merge_and` handles all combinations of `TrigramQuery` variants
- Cross-product of `Any`-`Any` is capped at 64 branches
- Test-only `Query` types match the confirmed HHC-46 AST shape
- All tests pass
- `cargo clippy -p ferret-indexer-core -- -D warnings` passes

---

## Task 6: Integration tests with real index reader

**Files:**
- Modify: `ferret-indexer-core/src/query_trigrams.rs`

Write integration tests that combine trigram extraction with actual index lookups to verify the end-to-end pipeline works.

### Step 1: Write integration tests

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::index_writer::TrigramIndexWriter;
    use crate::intersection::intersect_file_ids;
    use crate::posting::PostingListBuilder;
    use crate::index_reader::TrigramIndexReader;
    use crate::types::FileId;

    /// Build an index with sample files and verify trigram extraction works end-to-end.
    fn build_test_index() -> (tempfile::TempDir, TrigramIndexReader) {
        let mut builder = PostingListBuilder::file_only();
        builder.add_file(FileId(0), b"fn main() { HttpRequest::new() }");
        builder.add_file(FileId(1), b"fn parse() { HttpResponse::new() }");
        builder.add_file(FileId(2), b"fn test() { assert!(true) }");
        builder.finalize();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trigrams.bin");
        TrigramIndexWriter::write(&builder, &path).unwrap();
        let reader = TrigramIndexReader::open(&path).unwrap();
        (dir, reader)
    }

    /// Look up all posting lists for a TrigramQuery and intersect them.
    fn lookup_candidates(
        reader: &TrigramIndexReader,
        tq: &TrigramQuery,
    ) -> Vec<FileId> {
        match tq {
            TrigramQuery::All(trigrams) => {
                let lists: Vec<Vec<FileId>> = trigrams
                    .iter()
                    .map(|t| reader.lookup_file_ids(*t).unwrap())
                    .collect();
                intersect_file_ids(&lists)
            }
            TrigramQuery::Any(branches) => {
                let mut all_candidates: Vec<FileId> = Vec::new();
                for branch in branches {
                    let lists: Vec<Vec<FileId>> = branch
                        .iter()
                        .map(|t| reader.lookup_file_ids(*t).unwrap())
                        .collect();
                    let candidates = intersect_file_ids(&lists);
                    all_candidates.extend(candidates);
                }
                all_candidates.sort();
                all_candidates.dedup();
                all_candidates
            }
            TrigramQuery::None => {
                // Full scan: return all file IDs
                vec![FileId(0), FileId(1), FileId(2)]
            }
        }
    }

    #[test]
    fn test_integration_literal_query() {
        let (_dir, reader) = build_test_index();
        let tq = extract_literal_trigrams("HttpRequest", true);
        let candidates = lookup_candidates(&reader, &tq);
        // Only file 0 contains "HttpRequest"
        assert_eq!(candidates, vec![FileId(0)]);
    }

    #[test]
    fn test_integration_literal_shared_prefix() {
        let (_dir, reader) = build_test_index();
        let tq = extract_literal_trigrams("Http", true);
        let candidates = lookup_candidates(&reader, &tq);
        // Files 0 and 1 both contain "Http"
        assert!(candidates.contains(&FileId(0)));
        assert!(candidates.contains(&FileId(1)));
        assert!(!candidates.contains(&FileId(2)));
    }

    #[test]
    fn test_integration_no_match() {
        let (_dir, reader) = build_test_index();
        let tq = extract_literal_trigrams("Nonexistent", true);
        let candidates = lookup_candidates(&reader, &tq);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_integration_short_query_full_scan() {
        let (_dir, reader) = build_test_index();
        let tq = extract_literal_trigrams("fn", true);
        // Short query -> None -> full scan
        assert!(tq.is_none());
        let candidates = lookup_candidates(&reader, &tq);
        assert_eq!(candidates.len(), 3); // All files
    }
}
```

### Step 2: Verify

Run: `cargo test -p ferret-indexer-core -- query_trigrams::integration_tests`

### Acceptance Criteria
- Integration tests demonstrate the full pipeline: query -> trigram extraction -> index lookup -> correct candidates
- Literal queries narrow candidates to files containing the literal
- Shared-prefix queries correctly return multiple matching files
- Non-matching queries return empty candidates
- Short queries fall back to full scan

---

## Task 7: Wire up module exports and documentation

**Files:**
- Modify: `ferret-indexer-core/src/query_trigrams.rs`
- Modify: `ferret-indexer-core/src/lib.rs`

### Step 1: Add public exports

Ensure the following are publicly exported from `query_trigrams.rs`:
- `TrigramQuery` (enum)
- `extract_query_trigrams` (main entry point)
- `extract_literal_trigrams` (useful for callers that have raw strings)
- `extract_regex_trigrams` (useful for callers that have raw patterns)

Update `lib.rs` exports:
```rust
pub use query_trigrams::{TrigramQuery, extract_query_trigrams, extract_literal_trigrams, extract_regex_trigrams};
```

### Step 2: Add module-level documentation

Ensure the module doc comment at the top of `query_trigrams.rs` includes:
- Overview of the extraction strategy
- Table of query type -> trigram strategy mappings
- Usage example showing the main entry point
- Notes on the `TrigramQuery::None` fallback behavior

### Step 3: Final verification

Run:
```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

### Acceptance Criteria
- All public APIs are exported
- Module documentation is complete
- Full workspace builds, tests pass, clippy clean, fmt check passes
- No regressions in existing tests

---

## Summary

| Task | Description | Estimated LOC |
|------|-------------|---------------|
| 1 | Add `regex-syntax` dependency | 1 |
| 2 | Define `TrigramQuery` type and stub module | ~80 |
| 3 | Literal/phrase trigram extraction | ~30 |
| 4 | Regex literal extraction via `regex-syntax` | ~80 |
| 5 | Main `extract_query_trigrams` over Query AST | ~120 |
| 6 | Integration tests with real index reader | ~80 |
| 7 | Wire up exports and documentation | ~20 |

**Total:** ~450 lines of production code + ~350 lines of tests

**Key Design Decisions:**
1. **`TrigramQuery::Any` branch cap at 64**: Prevents combinatorial explosion from deeply nested AND(OR, OR) queries. 64 branches is generous enough for realistic queries.
2. **OR with None branch returns None**: If any OR branch has no extractable trigrams, we can't prune via the trigram index at all since that branch could match any file.
3. **NOT returns None**: Trigrams from a negated query can't be used for candidate pruning. The verification step handles NOT logic.
4. **Filter queries return None**: `PathFilter` and `LanguageFilter` are handled by metadata filters. This module only handles content trigram extraction.
5. **Regex fallback to None**: If `regex-syntax` can't extract any literals from a pattern, gracefully fall back to full scan rather than erroring.
6. **Test-only Query types**: Until HHC-46 lands the real Query types, tests use local placeholder structs matching the confirmed AST shape (`Query`, `LiteralQuery`, `RegexQuery`, `PhraseQuery`). The extraction logic is written against these types, so switching to the real types requires only changing the import.
7. **All trigram extraction is case-folded**: Since the index stores ASCII-folded trigrams (A-Z → a-z), all extraction uses `extract_unique_trigrams_folded()`. The `case_sensitive` flag on query types has NO effect on trigram extraction — it only affects verification (HHC-49). This eliminates the previous complexity around case-insensitive matching.
8. **`CaseSensitive(inner)` wrapper**: Simply recurses into the inner query. Has no effect on trigram extraction (case folding happens regardless). The wrapper only affects downstream verification.
9. **Binary `Or(Box<Query>, Box<Query>)`**: The AST uses binary OR instead of n-ary. The extraction handles this naturally by extracting from both branches and collecting into `TrigramQuery::Any`.
