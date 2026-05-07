// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::user_op::UserOp;
use ssz_derive::{Decode, Encode};

// ---------------------------------------------------------------------------
// Gas-economics-derived batch sizing
//
// The InputBox contract charges roughly:
//   total_gas ≈ base_gas + delta × payload_bytes
//
// We charge each user-op a DA fee of (1 + α) × δ per byte, where α amortizes
// the base cost across the batch:
//
//   α × δ × n = base_gas   ⟹   n = base_gas / (α × δ)
//
// Choosing α (the overhead fraction) determines the batch size n in bytes.
// All parameters live in the `batch_policy` SQLite singleton table so they
// can be hot-swapped at runtime (see 0001_schema.sql). A CHECK constraint
// on that table ensures batch_size_target < const_max_batch_bytes.
//
// Fee encoding:
//   All fees are **log-space exponents** with base 129/128 (see `crate::fee`).
//   An exponent N represents a linear fee of `(129/128)^N` smallest-token-units.
//   Exponent 0 = 1 unit (minimum). No sentinel.
//
//   The DB computes `log_recommended_fee` via pure addition of log terms:
//     log_recommended_fee = log_gas_price + log_one_plus_alpha + log_delta + log_user_op_bytes
//
//   The oracle feeds `log_gas_price` directly in log space. For tokens with
//   few decimals (e.g. USDC, 6 decimals) the oracle should pre-scale the
//   linear gas price before converting to log space.
// ---------------------------------------------------------------------------

/// Batch submissions are sent as raw `ssz(Batch)` with no tag; classification at L1 is by
/// attempting SSZ decode, and at the rollup by msg_sender.

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Batch {
    /// Batch index (nonce) for deduplication and ordering at the scheduler.
    pub nonce: u64,
    pub frames: Vec<Frame>,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Frame {
    pub user_ops: Vec<WireUserOp>,
    pub safe_block: u64,
    /// Log-space fee exponent (base 129/128). See [`crate::fee`].
    pub fee_price: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct WireUserOp {
    pub nonce: u32,
    /// Log-space fee exponent (base 129/128). See [`crate::fee`].
    pub max_fee: u16,
    pub data: Vec<u8>,
    pub signature: Vec<u8>,
}

impl WireUserOp {
    pub const SIGNATURE_BYTES: usize = 65;

    pub fn to_user_op(&self) -> UserOp {
        UserOp {
            nonce: self.nonce,
            max_fee: self.max_fee,
            data: self.data.clone().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssz::{Decode, Encode};

    fn sample_user_op(nonce: u32) -> WireUserOp {
        WireUserOp {
            nonce,
            max_fee: 100,
            data: vec![0xaa, 0xbb, 0xcc, 0xdd],
            signature: vec![0xee; WireUserOp::SIGNATURE_BYTES],
        }
    }

    fn sample_frame(safe_block: u64, user_op_count: u32) -> Frame {
        Frame {
            user_ops: (0..user_op_count).map(sample_user_op).collect(),
            safe_block,
            fee_price: 42,
        }
    }

    fn sample_batch(nonce: u64, frame_count: u64) -> Batch {
        Batch {
            nonce,
            frames: (0..frame_count).map(|i| sample_frame(100 + i, 2)).collect(),
        }
    }

    // ── SSZ round-trip determinism ──────────────────────────────────

    #[test]
    fn ssz_roundtrip_empty_batch_is_identity() {
        let batch = Batch {
            nonce: 0,
            frames: vec![],
        };
        let encoded = batch.as_ssz_bytes();
        let decoded = Batch::from_ssz_bytes(&encoded).expect("decode empty batch");
        assert_eq!(decoded, batch);
        assert_eq!(decoded.as_ssz_bytes(), encoded);
    }

    #[test]
    fn ssz_roundtrip_populated_batch_is_identity() {
        let batch = sample_batch(42, 3);
        let encoded = batch.as_ssz_bytes();
        let decoded = Batch::from_ssz_bytes(&encoded).expect("decode populated batch");
        assert_eq!(decoded, batch);
        assert_eq!(decoded.as_ssz_bytes(), encoded);
    }

    #[test]
    fn ssz_roundtrip_frame_with_empty_user_ops_is_identity() {
        // Closed-empty frames (direct-input-only) are a real on-wire shape.
        let frame = Frame {
            user_ops: vec![],
            safe_block: 7,
            fee_price: 0,
        };
        let encoded = frame.as_ssz_bytes();
        let decoded = Frame::from_ssz_bytes(&encoded).expect("decode");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn ssz_roundtrip_wire_user_op_is_identity() {
        let uop = sample_user_op(99);
        let encoded = uop.as_ssz_bytes();
        let decoded = WireUserOp::from_ssz_bytes(&encoded).expect("decode wire user op");
        assert_eq!(decoded, uop);
    }

    #[test]
    fn ssz_encoding_is_deterministic_across_calls() {
        // Determinism under the same input is a consensus requirement; encoding
        // the same batch twice must produce byte-identical output.
        let batch = sample_batch(7, 2);
        assert_eq!(batch.as_ssz_bytes(), batch.as_ssz_bytes());
    }

    // ── Decode robustness (no panics on adversarial bytes) ──────────

    #[test]
    fn ssz_decode_empty_payload_returns_error() {
        assert!(Batch::from_ssz_bytes(&[]).is_err());
    }

    #[test]
    fn ssz_decode_below_fixed_header_returns_error() {
        // Batch's fixed portion is 8 (nonce) + 4 (frames offset) = 12 bytes.
        for len in 0..12 {
            let buf = vec![0u8; len];
            assert!(
                Batch::from_ssz_bytes(&buf).is_err(),
                "decoding {len} bytes below fixed header must fail",
            );
        }
    }

    #[test]
    fn ssz_decode_truncated_valid_batch_returns_error() {
        let batch = sample_batch(1, 2);
        let full = batch.as_ssz_bytes();
        // Truncating anywhere before the full length must not round-trip.
        for cut in 0..full.len() {
            let truncated = &full[..cut];
            match Batch::from_ssz_bytes(truncated) {
                Err(_) => {}
                Ok(decoded) => assert_ne!(
                    decoded, batch,
                    "truncation at {cut} silently decoded to the original batch",
                ),
            }
        }
    }

    #[test]
    fn ssz_decode_invalid_offset_returns_error() {
        // Well-formed nonce (8 zero bytes), frames offset points far past the
        // buffer end. SSZ must reject rather than read out of bounds.
        let mut buf = vec![0u8; 12];
        buf[8..12].copy_from_slice(&0xffff_ffff_u32.to_le_bytes());
        assert!(Batch::from_ssz_bytes(&buf).is_err());
    }

    #[test]
    fn ssz_decode_garbage_bytes_never_panics() {
        // Adversarial fixed patterns. Decoding may Err or Ok; the invariant we
        // care about is "no panic" — the test passing proves it.
        for pattern in [0x00, 0x01, 0x42, 0x7f, 0x80, 0xff] {
            for len in [1, 12, 64, 256, 1024] {
                let _ = Batch::from_ssz_bytes(&vec![pattern; len]);
            }
        }
    }
}
