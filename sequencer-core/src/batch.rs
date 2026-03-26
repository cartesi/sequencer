// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::user_op::UserOp;
use ssz_derive::{Decode, Encode};

/// Tag byte for InputBox payloads that are L1 app direct inputs (e.g. deposits).
/// L1/app must post such inputs as `0x00 || body`. Only these are stored (body only) and executed.
pub const INPUT_TAG_DIRECT_INPUT: u8 = 0x00;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchForSubmission {
    pub batch_index: u64,
    pub created_at_ms: u64,
    pub batch: Batch,
}

impl BatchForSubmission {
    /// Encode the batch for the scheduler as a single SSZ payload.
    ///
    /// Payload is `ssz(Batch { nonce: batch_index, frames })`. The scheduler decodes this
    /// and uses `batch.nonce` for deduplication; classification at the rollup is by msg_sender.
    pub fn encode_for_scheduler(&self) -> Vec<u8> {
        let batch = Batch {
            nonce: self.batch_index,
            frames: self.batch.frames.clone(),
        };
        ssz::Encode::as_ssz_bytes(&batch)
    }
}
