# Workspace & Dependencies Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Set up cargo workspace with 3 crates and all foundational dependencies.

**Architecture:** Workspace root with ferret-indexer-core (lib), ferret-indexer-cli (bin), ferret-mcp (bin). Core is the shared library, CLI and MCP are thin binaries that depend on it.

**Tech Stack:** Rust 2024 edition, tokio, serde, memmap2, blake3, clap, rmcp

---

## Task 1: Convert root Cargo.toml to workspace manifest

**File:** `Cargo.toml`

Replace the existing `[package]` manifest with a workspace manifest:

```toml
[workspace]
resolver = "3"
members = [
    "ferret-indexer-core",
    "ferret-indexer-cli",
    "ferret-mcp",
]
```

**Verify:** `cat Cargo.toml` shows workspace manifest, no `[package]` section.

---

## Task 2: Delete old src/main.rs

**Action:** Remove `src/main.rs` since the workspace root is no longer a package.

```bash
rm src/main.rs
rmdir src
```

**Verify:** `ls src/` should fail (directory removed).

---

## Task 3: Create ferret-indexer-core crate with dependencies

**File:** `ferret-indexer-core/Cargo.toml`

```toml
[package]
name = "ferret-indexer-core"
version = "0.1.0"
edition = "2024"

[dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
memmap2 = "0.9"
blake3 = "1"
zstd = "0.13"
regex = "1"
ignore = "0.4"
notify = "8"
notify-debouncer-full = "0.4"
integer-encoding = "4"
thiserror = "2"
tracing = "0.1"
```

**File:** `ferret-indexer-core/src/lib.rs`

```rust
//! ferret-indexer-core: Index engine, storage, and query library for local code search.
```

**Verify:** `cargo check -p ferret-indexer-core` compiles without errors.

---

## Task 4: Create ferret-indexer-cli crate with dependencies

**File:** `ferret-indexer-cli/Cargo.toml`

```toml
[package]
name = "ferret-indexer-cli"
version = "0.1.0"
edition = "2024"

[dependencies]
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
ferret-indexer-core = { path = "../ferret-indexer-core" }
```

**File:** `ferret-indexer-cli/src/main.rs`

```rust
#[tokio::main]
async fn main() {
    println!("ferret-indexer-cli");
}
```

**Verify:** `cargo check -p ferret-indexer-cli` compiles without errors.

---

## Task 5: Create ferret-mcp crate with dependencies

**File:** `ferret-mcp/Cargo.toml`

```toml
[package]
name = "ferret-mcp"
version = "0.1.0"
edition = "2024"

[dependencies]
rmcp = { version = "0.1", features = ["server", "transport-io"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
ferret-indexer-core = { path = "../ferret-indexer-core" }
```

**File:** `ferret-mcp/src/main.rs`

```rust
#[tokio::main]
async fn main() {
    println!("ferret-mcp");
}
```

**Verify:** `cargo check -p ferret-mcp` compiles without errors.

---

## Task 6: Full workspace build and verification

**Commands:**
```bash
cargo check --workspace
cargo build --workspace
```

**Expected:** Both pass with zero errors. `Cargo.lock` is generated. Three crates are listed.

---

## Task 7: Commit all changes

Commit with message: "Set up cargo workspace with core, cli, and mcp crates (HHC-22, HHC-23)"
