# Delta-Encoded Varint Posting List Codec

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement serialization/deserialization for posting lists using delta encoding + varint compression. This is a foundational building block for the trigram index storage layer described in the design doc (section 3.2).

**Architecture:** A single new module `codec.rs` in `ferret-indexer-core`, re-exported from `lib.rs`. Four public functions: `encode_delta_varint`, `decode_delta_varint`, `encode_positional_postings`, `decode_positional_postings`.

**Tech Stack:** Rust 2024, `integer-encoding` crate (VarIntWriter / VarIntReader traits)

---

## Task 1: Add `pub mod codec` to lib.rs and create codec.rs skeleton

**File:** `ferret-indexer-core/src/lib.rs` — add `pub mod codec;`

**File:** `ferret-indexer-core/src/codec.rs` — create with module doc comment and function signatures that return empty/default values (stubs).

```rust
pub fn encode_delta_varint(values: &[u32]) -> Vec<u8> { vec![] }
pub fn decode_delta_varint(data: &[u8]) -> Vec<u32> { vec![] }
pub fn encode_positional_postings(postings: &[(u32, u32)]) -> Vec<u8> { vec![] }
pub fn decode_positional_postings(data: &[u8]) -> Vec<(u32, u32)> { vec![] }
```

**Test:** `cargo check -p ferret-indexer-core` — compiles with stubs.

---

## Task 2: Write failing tests for encode/decode_delta_varint

**File:** `ferret-indexer-core/src/codec.rs` — add `#[cfg(test)] mod tests` with:

- `test_roundtrip_known_values` — encode [1, 3, 5, 7, 100], decode, assert identical
- `test_empty_input` — encode [] -> decode -> []
- `test_single_value` — encode [42] -> decode -> [42]
- `test_large_deltas` — encode [0, 1_000_000] -> decode -> verify
- `test_roundtrip_random` — generate 100 sorted random u32s, roundtrip
- `test_compression_benefit` — encode 1000 sequential file_ids (0..1000), verify size < 4000 bytes

**Test:** `cargo test -p ferret-indexer-core -- codec` — tests fail (stubs return empty).

---

## Task 3: Implement encode_delta_varint and decode_delta_varint

**File:** `ferret-indexer-core/src/codec.rs`

`encode_delta_varint`:
1. If empty, return empty vec.
2. Create a `Vec<u8>` cursor.
3. For each value, compute delta = value - previous (first value: delta = value).
4. Write delta as varint using `VarIntWriter::write_varint`.
5. Return the buffer.

`decode_delta_varint`:
1. If empty data, return empty vec.
2. Create a cursor over the data.
3. Read varints with `VarIntReader::read_varint`, accumulating running sum.
4. Return accumulated values.

**Test:** `cargo test -p ferret-indexer-core -- codec::tests::test_roundtrip` and all delta_varint tests pass.

---

## Task 4: Write failing tests for positional postings

**File:** `ferret-indexer-core/src/codec.rs` — add to tests:

- `test_positional_roundtrip` — [(0,5),(0,10),(0,15),(1,0),(1,20)] -> encode -> decode -> verify
- `test_positional_empty` — [] -> encode -> decode -> []
- `test_positional_single` — [(5, 42)] -> encode -> decode -> [(5, 42)]

**Test:** `cargo test -p ferret-indexer-core -- codec::tests::test_positional` — tests fail.

---

## Task 5: Implement encode_positional_postings and decode_positional_postings

**File:** `ferret-indexer-core/src/codec.rs`

`encode_positional_postings`:
1. If empty, return empty vec.
2. Group postings by file_id (input is sorted by file_id, then offset).
3. For each group: write file_id (varint), offset_count (varint), then delta-encoded offsets.

`decode_positional_postings`:
1. If empty, return empty vec.
2. Read file_id (varint), offset_count (varint), then decode that many delta-encoded offsets.
3. Emit (file_id, offset) pairs. Repeat until data is exhausted.

**Test:** `cargo test -p ferret-indexer-core -- codec` — all tests pass.

---

## Task 6: Add known-bytes test for encode_delta_varint

**File:** `ferret-indexer-core/src/codec.rs` — add test:

- `test_known_byte_output` — encode [1, 3, 5, 7, 100] and verify the exact byte sequence (deltas: 1, 2, 2, 2, 93; varint encoding of each).

**Test:** `cargo test -p ferret-indexer-core -- codec::tests::test_known_byte_output` — passes.

---

## Task 7: Final verification and commit

- `cargo test -p ferret-indexer-core` — all tests pass
- `cargo check --workspace` — no errors
- Commit all changes with descriptive message
