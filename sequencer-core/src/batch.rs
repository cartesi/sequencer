// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::user_op::UserOp;
use ssz_derive::{Decode, Encode};

// Frame fees and batch sizing are log-space DA charges; the formula and its
// knobs live with `batch_policy` in sequencer/src/storage/migrations/0001_schema.sql.

/// Batch submissions are sent as raw `ssz(Batch)` with no tag; classification is
/// by `msg_sender` everywhere (sender == batch-submitter address ⇒ batch, any
/// other sender ⇒ direct input) — never by decode-attempt. See
/// docs/protocol/scheduler-semantics.md, "The one stream, the one classification".

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

    // ── Pinned encoding ─────────────────────────────────────────────

    /// The pinned batch has nine frames, one holding nine signed user ops, so
    /// both lists decode through offset tables longer than eight entries. Each
    /// container decode also pushes its fields through ssz's 8-slot `SmallVec`
    /// builder; no wire type has more than four fields, so that never spills.
    #[test]
    fn ssz_spill_batch_matches_the_pinned_bytes() {
        let vectors = crate::consensus_vectors::vectors();
        let pinned = &vectors.ssz.spill_batch;
        let batch = pinned.to_batch();
        assert!(batch.frames.len() > 8);
        assert!(batch.frames.iter().any(|frame| frame.user_ops.len() > 8));

        assert_eq!(batch.as_ssz_bytes(), pinned.encoded.as_ref());
        assert_eq!(
            Batch::from_ssz_bytes(&pinned.encoded).expect("decode the pinned batch"),
            batch
        );
        // canonical-test replays these same bytes through the guest.
        assert_eq!(vectors.guest.payloads["spill_batch"], pinned.encoded);
    }
}
