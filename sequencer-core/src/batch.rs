// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use crate::user_op::UserOp;
use ssz_derive::{Decode, Encode};

/// Tag byte for InputBox payloads that are L1 app direct inputs (e.g. deposits).
/// L1/app must post such inputs as `0x00 || body`. Only these are stored (body only) and executed.
pub const INPUT_TAG_DIRECT_INPUT: u8 = 0x00;

/// Tag byte for InputBox payloads that are batch submissions (for the off-chain scheduler).
/// Batch submitter posts `0x01 || ssz(Batch)`. These are not stored in `direct_inputs`.
pub const INPUT_TAG_BATCH_SUBMISSION: u8 = 0x01;

/// Any other first byte is invalid; the input reader discards such payloads (garbage).

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Batch {
    pub frames: Vec<Frame>,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Frame {
    pub user_ops: Vec<WireUserOp>,
    pub safe_block: u64,
    pub fee_price: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct WireUserOp {
    pub nonce: u32,
    pub max_fee: u32,
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
    /// Encode the batch payload for the on-chain scheduler.
    ///
    /// This uses the same SSZ encoding that the canonical scheduler expects when
    /// decoding inputs, and prefixes the payload with:
    /// - a single tag byte so that batch submissions can be distinguished from other InputBox inputs; and
    /// - the batch index as an 8-byte big-endian nonce, so the scheduler can deduplicate.
    pub fn encode_for_scheduler(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // First byte identifies this InputBox payload as a batch submission.
        out.push(INPUT_TAG_BATCH_SUBMISSION);
        // Next 8 bytes carry the batch index as a big-endian nonce for the scheduler.
        out.extend_from_slice(&self.batch_index.to_be_bytes());
        out.extend(ssz::Encode::as_ssz_bytes(&self.batch));
        out
    }
}
