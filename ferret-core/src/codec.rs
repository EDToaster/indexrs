//! Delta-encoded varint codec for posting list serialization.
//!
//! This module provides compact binary serialization for posting lists used in
//! the trigram index. Two formats are supported:
//!
//! - **File-level posting lists**: sorted arrays of `u32` file IDs, delta-encoded
//!   and varint-compressed. Used for fast file-level trigram intersection.
//!
//! - **Positional posting lists**: `(file_id, offset)` pairs grouped by file_id,
//!   with offsets delta-encoded within each group. Used for proximity queries
//!   that need byte-offset positions.
//!
//! Delta encoding stores differences between consecutive sorted values instead of
//! absolute values. Since posting lists contain sorted IDs with small gaps,
//! deltas are typically small numbers that compress well with variable-length
//! integer (varint) encoding, achieving 2-4x compression over raw `u32` arrays.

use std::io::Cursor;

use integer_encoding::{VarIntReader, VarIntWriter};

/// Encode a sorted slice of `u32` values using delta encoding + varint.
///
/// Each value stores the difference from the previous value. The first value
/// is stored as-is (its delta from an implicit zero predecessor).
///
/// # Arguments
///
/// * `values` - A sorted (ascending) slice of `u32` values. The caller must
///   ensure the slice is sorted; unsorted input produces garbage output.
///
/// # Returns
///
/// A byte vector containing the varint-encoded deltas.
///
/// # Examples
///
/// ```
/// use ferret_indexer_core::codec::{encode_delta_varint, decode_delta_varint};
///
/// let values = vec![1, 3, 5, 7, 100];
/// let encoded = encode_delta_varint(&values);
/// let decoded = decode_delta_varint(&encoded);
/// assert_eq!(decoded, values);
/// ```
pub fn encode_delta_varint(values: &[u32]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }

    let mut buf = Vec::with_capacity(values.len()); // rough estimate
    let mut prev = 0u32;

    for &val in values {
        debug_assert!(
            val >= prev,
            "encode_delta_varint requires sorted input: {val} < {prev}"
        );
        let delta = val.wrapping_sub(prev);
        buf.write_varint(delta)
            .expect("write to Vec<u8> cannot fail");
        prev = val;
    }

    buf
}

/// Decode a delta-encoded varint sequence back to sorted `u32` values.
///
/// This is the inverse of [`encode_delta_varint`]. Reads varint-encoded deltas
/// from the byte slice and reconstructs the original sorted values by
/// accumulating a running sum.
///
/// # Arguments
///
/// * `data` - A byte slice produced by [`encode_delta_varint`].
///
/// # Returns
///
/// The reconstructed sorted vector of `u32` values.
///
/// # Examples
///
/// ```
/// use ferret_indexer_core::codec::{encode_delta_varint, decode_delta_varint};
///
/// let encoded = encode_delta_varint(&[10, 20, 30]);
/// assert_eq!(decode_delta_varint(&encoded), vec![10, 20, 30]);
/// ```
pub fn decode_delta_varint(data: &[u8]) -> Vec<u32> {
    if data.is_empty() {
        return Vec::new();
    }

    let mut cursor = Cursor::new(data);
    let mut result = Vec::new();
    let mut accumulator = 0u32;

    while (cursor.position() as usize) < data.len() {
        let delta: u32 = match cursor.read_varint() {
            Ok(d) => d,
            Err(_) => break,
        };
        // Finding 13: Use saturating_add to avoid u32 wrapping on malformed data.
        accumulator = accumulator.saturating_add(delta);
        result.push(accumulator);
    }

    result
}

/// Encode a positional posting list: `Vec<(file_id, offset)>` grouped by file_id.
///
/// The input must be sorted by `(file_id, offset)`. The encoding format groups
/// entries by file_id:
///
/// ```text
/// For each file group:
///   file_id:      varint
///   offset_count: varint
///   offsets:      delta-encoded varint sequence
/// ```
///
/// Within each file group, offsets are delta-encoded (storing differences between
/// consecutive offsets), which compresses well when offsets are clustered.
///
/// # Arguments
///
/// * `postings` - A sorted (by file_id, then offset) slice of `(file_id, offset)` pairs.
///
/// # Returns
///
/// A byte vector containing the encoded positional posting list.
///
/// # Examples
///
/// ```
/// use ferret_indexer_core::codec::{encode_positional_postings, decode_positional_postings};
///
/// let postings = vec![(0, 5), (0, 10), (1, 0), (1, 20)];
/// let encoded = encode_positional_postings(&postings);
/// let decoded = decode_positional_postings(&encoded);
/// assert_eq!(decoded, postings);
/// ```
pub fn encode_positional_postings(postings: &[(u32, u32)]) -> Vec<u8> {
    if postings.is_empty() {
        return Vec::new();
    }

    let mut buf = Vec::new();

    // Group consecutive entries by file_id
    let mut i = 0;
    while i < postings.len() {
        let file_id = postings[i].0;

        // Collect all offsets for this file_id
        let group_start = i;
        while i < postings.len() && postings[i].0 == file_id {
            i += 1;
        }
        let group_end = i;
        let count = group_end - group_start;

        // Write file_id
        buf.write_varint(file_id)
            .expect("write to Vec<u8> cannot fail");

        // Write offset count
        buf.write_varint(count as u32)
            .expect("write to Vec<u8> cannot fail");

        // Write delta-encoded offsets
        let mut prev_offset = 0u32;
        for posting in &postings[group_start..group_end] {
            let offset = posting.1;
            debug_assert!(
                offset >= prev_offset,
                "encode_positional_postings requires sorted offsets within each file group: \
                 offset {offset} < prev {prev_offset}"
            );
            let delta = offset.wrapping_sub(prev_offset);
            buf.write_varint(delta)
                .expect("write to Vec<u8> cannot fail");
            prev_offset = offset;
        }
    }

    buf
}

/// Decode a positional posting list back to `Vec<(file_id, offset)>`.
///
/// This is the inverse of [`encode_positional_postings`]. Reads the grouped
/// format and reconstructs the original sorted `(file_id, offset)` pairs.
///
/// # Arguments
///
/// * `data` - A byte slice produced by [`encode_positional_postings`].
///
/// # Returns
///
/// The reconstructed sorted vector of `(file_id, offset)` pairs.
///
/// # Examples
///
/// ```
/// use ferret_indexer_core::codec::{encode_positional_postings, decode_positional_postings};
///
/// let encoded = encode_positional_postings(&[(0, 5), (0, 10)]);
/// let decoded = decode_positional_postings(&encoded);
/// assert_eq!(decoded, vec![(0, 5), (0, 10)]);
/// ```
pub fn decode_positional_postings(data: &[u8]) -> Vec<(u32, u32)> {
    if data.is_empty() {
        return Vec::new();
    }

    let mut cursor = Cursor::new(data);
    let mut result = Vec::new();

    while (cursor.position() as usize) < data.len() {
        // Read file_id
        let file_id: u32 = match cursor.read_varint() {
            Ok(v) => v,
            Err(_) => break,
        };

        // Read offset count
        let count: u32 = match cursor.read_varint() {
            Ok(v) => v,
            Err(_) => break,
        };

        // Read delta-encoded offsets
        let mut prev_offset = 0u32;
        for _ in 0..count {
            let delta: u32 = match cursor.read_varint() {
                Ok(d) => d,
                Err(_) => return result,
            };
            // Finding 13: Use saturating_add to avoid u32 wrapping on malformed data.
            prev_offset = prev_offset.saturating_add(delta);
            result.push((file_id, prev_offset));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- encode/decode_delta_varint tests ----

    #[test]
    fn test_roundtrip_known_values() {
        let values = vec![1, 3, 5, 7, 100];
        let encoded = encode_delta_varint(&values);
        let decoded = decode_delta_varint(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_empty_input() {
        let encoded = encode_delta_varint(&[]);
        assert!(encoded.is_empty());
        let decoded = decode_delta_varint(&encoded);
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_single_value() {
        let values = vec![42];
        let encoded = encode_delta_varint(&values);
        let decoded = decode_delta_varint(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_large_deltas() {
        let values = vec![0, 1_000_000];
        let encoded = encode_delta_varint(&values);
        let decoded = decode_delta_varint(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_roundtrip_random() {
        // Generate 100 sorted random-ish u32 values using a simple LCG
        let mut rng_state: u64 = 12345;
        let mut values: Vec<u32> = Vec::new();
        for _ in 0..100 {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            values.push((rng_state >> 33) as u32);
        }
        values.sort();
        values.dedup();

        let encoded = encode_delta_varint(&values);
        let decoded = decode_delta_varint(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_known_byte_output() {
        // Input: [1, 3, 5, 7, 100]
        // Deltas: [1, 2, 2, 2, 93]
        // Varint encoding of small values:
        //   1 -> 0x01, 2 -> 0x02, 93 -> 0x5D
        // All deltas fit in a single byte (< 128).
        let values = vec![1, 3, 5, 7, 100];
        let encoded = encode_delta_varint(&values);
        assert_eq!(encoded, vec![0x01, 0x02, 0x02, 0x02, 0x5D]);

        // Verify decode of these exact bytes
        let decoded = decode_delta_varint(&[0x01, 0x02, 0x02, 0x02, 0x5D]);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_compression_benefit() {
        // 1000 sequential file_ids: 0, 1, 2, ..., 999
        let values: Vec<u32> = (0..1000).collect();
        let raw_size = values.len() * 4; // 4000 bytes as raw u32 array
        let encoded = encode_delta_varint(&values);

        assert!(
            encoded.len() < raw_size,
            "encoded size {} should be less than raw size {}",
            encoded.len(),
            raw_size,
        );

        // With deltas of 1 (except first which is 0), each varint is 1 byte.
        // So encoded should be ~1000 bytes, well under 4000.
        assert!(
            encoded.len() <= 1000,
            "encoded size {} should be ~1000 bytes for sequential IDs",
            encoded.len(),
        );

        // Verify roundtrip
        let decoded = decode_delta_varint(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_consecutive_duplicates_zero_delta() {
        // Edge case: same value repeated. Deltas are all zero.
        let values = vec![5, 5, 5, 5];
        let encoded = encode_delta_varint(&values);
        let decoded = decode_delta_varint(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_max_u32_value() {
        let values = vec![u32::MAX];
        let encoded = encode_delta_varint(&values);
        let decoded = decode_delta_varint(&encoded);
        assert_eq!(decoded, values);
    }

    // ---- encode/decode_positional_postings tests ----

    #[test]
    fn test_positional_roundtrip() {
        let postings = vec![(0, 5), (0, 10), (0, 15), (1, 0), (1, 20)];
        let encoded = encode_positional_postings(&postings);
        let decoded = decode_positional_postings(&encoded);
        assert_eq!(decoded, postings);
    }

    #[test]
    fn test_positional_empty() {
        let encoded = encode_positional_postings(&[]);
        assert!(encoded.is_empty());
        let decoded = decode_positional_postings(&encoded);
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_positional_single() {
        let postings = vec![(5, 42)];
        let encoded = encode_positional_postings(&postings);
        let decoded = decode_positional_postings(&encoded);
        assert_eq!(decoded, postings);
    }

    #[test]
    fn test_positional_multiple_files() {
        // Multiple files, each with several offsets
        let postings = vec![
            (0, 10),
            (0, 20),
            (0, 30),
            (1, 5),
            (1, 100),
            (2, 0),
            (2, 1),
            (2, 2),
            (10, 500),
        ];
        let encoded = encode_positional_postings(&postings);
        let decoded = decode_positional_postings(&encoded);
        assert_eq!(decoded, postings);
    }

    #[test]
    fn test_positional_large_offsets() {
        let postings = vec![(0, 0), (0, 1_000_000), (1, 999_999)];
        let encoded = encode_positional_postings(&postings);
        let decoded = decode_positional_postings(&encoded);
        assert_eq!(decoded, postings);
    }

    #[test]
    fn test_positional_compression() {
        // 100 files, each with 10 sequential offsets
        let mut postings = Vec::new();
        for file_id in 0..100u32 {
            for offset in 0..10u32 {
                postings.push((file_id, offset * 10));
            }
        }
        let raw_size = postings.len() * 8; // 8 bytes per (u32, u32) pair = 8000
        let encoded = encode_positional_postings(&postings);
        assert!(
            encoded.len() < raw_size,
            "encoded size {} should be less than raw size {}",
            encoded.len(),
            raw_size,
        );

        let decoded = decode_positional_postings(&encoded);
        assert_eq!(decoded, postings);
    }
}
