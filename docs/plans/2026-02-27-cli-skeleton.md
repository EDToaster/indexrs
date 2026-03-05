# CLI Skeleton Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create the CLI argument parsing skeleton with clap for all ferret subcommands.

**Architecture:** Two-file structure — args.rs defines all clap types, main.rs dispatches. Subcommands are stubs that print TODO messages.

**Tech Stack:** Rust 2024, clap derive, tokio

---

## Task 1: Set up workspace and ferret-indexer-cli crate structure

Create the cargo workspace with `ferret-indexer-cli` as a member. Create the directory structure:

```
ferret-indexer-cli/
├── Cargo.toml
└── src/
    ├── main.rs
    └── args.rs
```

### 1a. Update workspace root `Cargo.toml`

**File:** `Cargo.toml`

Replace the single-package Cargo.toml with a workspace root:

```toml
[workspace]
resolver = "2"
members = [
    "ferret-indexer-cli",
]
```

### 1b. Create `ferret-indexer-cli/Cargo.toml`

**File:** `ferret-indexer-cli/Cargo.toml`

```toml
[package]
name = "ferret-indexer-cli"
version = "0.1.0"
edition = "2024"
description = "CLI binary for ferret local code search"

[dependencies]
clap = { version = "4", features = ["derive", "color"] }
tokio = { version = "1", features = ["full"] }
```

### 1c. Create placeholder files

- `ferret-indexer-cli/src/main.rs` — minimal `fn main() {}`
- `ferret-indexer-cli/src/args.rs` — empty module

### 1d. Remove old `src/main.rs`

The old single-crate src/ is no longer needed with the workspace layout.

### Verify

```
cargo check -p ferret-indexer-cli
```

Expected: compiles successfully.

---

## Task 2: Define CLI args in `args.rs`

**File:** `ferret-indexer-cli/src/args.rs`

Define all clap derive structs and enums:

1. `Cli` — root struct with global flags (`--color`, `--repo`, `--verbose`) and `Command` subcommand enum
2. `ColorMode` — enum: Auto, Always, Never (with `clap::ValueEnum`)
3. `OutputFormat` — enum: Grep, Json, Pretty (with `clap::ValueEnum`)
4. `Command` — subcommand enum with variants:
   - `Search { query, language, path, limit, format }`
   - `Files { query, language, limit }`
   - `Symbols { query, kind, language, limit }`
   - `Preview { file, line, context }`
   - `Status`
   - `Reindex { full }`

All fields should have doc comments (these become help text). Use appropriate clap attributes for short flags, defaults, and value names.

### Verify

```
cargo check -p ferret-indexer-cli
```

---

## Task 3: Wire up `main.rs` with dispatch

**File:** `ferret-indexer-cli/src/main.rs`

1. `mod args;` to import the args module
2. Parse CLI with `Cli::parse()`
3. Match on `cli.command` and print `"TODO: implement {subcommand}"` for each variant
4. Use `#[tokio::main]` for async main (will be needed later)

### Verify

```
cargo check -p ferret-indexer-cli
cargo run -p ferret-indexer-cli -- --help
cargo run -p ferret-indexer-cli -- search --help
cargo run -p ferret-indexer-cli -- search "test"
cargo run -p ferret-indexer-cli -- files --help
cargo run -p ferret-indexer-cli -- symbols --help
cargo run -p ferret-indexer-cli -- preview --help
cargo run -p ferret-indexer-cli -- preview somefile.rs
cargo run -p ferret-indexer-cli -- status
cargo run -p ferret-indexer-cli -- reindex
cargo run -p ferret-indexer-cli -- reindex --full
```

Expected:
- `--help` shows all subcommands and global options
- `search --help` shows search-specific options
- `search "test"` prints "TODO: implement search"
- Each subcommand prints its own TODO message

---

## Task 4: Commit all changes

Stage and commit:
- `Cargo.toml` (workspace root)
- `ferret-indexer-cli/Cargo.toml`
- `ferret-indexer-cli/src/args.rs`
- `ferret-indexer-cli/src/main.rs`
- `docs/plans/2026-02-27-cli-skeleton.md`

Remove old files:
- `src/main.rs` (no longer needed)

Commit message: "Add CLI skeleton with clap for ferret-indexer-cli"
