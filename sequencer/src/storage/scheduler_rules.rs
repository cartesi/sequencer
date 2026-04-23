// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Off-chain simulator of the scheduler's batch-acceptance rules.
//!
//! The scheduler (in `canonical-app`) decides on-chain which InputBox events
//! it accepts as the next batch in the chain. This module implements the same
//! rules off-chain as a pure function, used to materialize
//! `safe_accepted_batches` — the sequencer's cached view of the scheduler's
//! gold frontier.
//!
//! Both sides follow the same predicate:
//!
//! - Sender must equal the configured batch-submitter address.
//! - Payload must SSZ-decode as a `Batch`.
//! - The batch's first-frame `safe_block` must not be older than
//!   `max_wait_blocks` relative to the event's inclusion block (otherwise the
//!   scheduler skips it as stale — a no-op in nonce space).
//! - The batch's `nonce` must equal the scheduler's currently expected next
//!   nonce (otherwise the scheduler skips it without advancing).
//!
//! [`SchedulerRules`] is a thin parameter object holding the two inputs the
//! predicate depends on; `evaluate` applies the predicate to a single safe
//! input and returns an [`AcceptedBatch`] when the scheduler would accept.
//! Callers own the `expected_nonce` state and advance it across inputs.

use alloy_primitives::Address;
use sequencer_core::batch::Batch;

use super::internals::batch_age_is_stale;

/// Protocol rules that decide which on-chain batch submissions the scheduler
/// would accept. Stateless across inputs — the caller threads `expected_nonce`.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerRules {
    batch_submitter_address: Address,
    max_wait_blocks: u64,
}

/// Borrowed view of one `safe_inputs` row in the shape `evaluate` needs.
/// Using a reference avoids copying the payload during iteration.
#[derive(Debug, Clone, Copy)]
pub(super) struct SafeInputRef<'a> {
    pub safe_input_index: i64,
    pub sender: Address,
    pub payload: &'a [u8],
    pub inclusion_block: u64,
}

/// One row the scheduler would append to its gold frontier.
#[derive(Debug, Clone, Copy)]
pub(super) struct AcceptedBatch {
    pub safe_input_index: i64,
    pub nonce: u64,
    pub first_frame_safe_block: u64,
    pub inclusion_block: u64,
}

impl SchedulerRules {
    pub fn new(batch_submitter_address: Address, max_wait_blocks: u64) -> Self {
        Self {
            batch_submitter_address,
            max_wait_blocks,
        }
    }

    pub fn batch_submitter_address(&self) -> Address {
        self.batch_submitter_address
    }

    pub fn max_wait_blocks(&self) -> u64 {
        self.max_wait_blocks
    }

    /// Evaluate a single safe input under the scheduler's acceptance rules,
    /// given the currently-expected next batch nonce.
    ///
    /// Returns `Some(AcceptedBatch)` iff the scheduler would accept this
    /// input at this nonce. Returns `None` on any rejection path (wrong
    /// sender, SSZ decode failure, stale by inclusion, nonce mismatch) —
    /// the caller leaves `expected_nonce` unchanged and continues.
    ///
    /// Stateless: the caller owns `expected_nonce` and advances it by 1 for
    /// each `Some` result. This lets a fold over a stream of inputs
    /// reproduce what the on-chain scheduler does without holding mutable
    /// state here.
    pub(super) fn evaluate(
        &self,
        input: SafeInputRef<'_>,
        expected_nonce: u64,
    ) -> Option<AcceptedBatch> {
        if input.sender != self.batch_submitter_address {
            return None;
        }
        let batch = <Batch as ssz::Decode>::from_ssz_bytes(input.payload).ok()?;
        let first_frame_safe_block = batch.frames.first().map(|f| f.safe_block).unwrap_or(0);
        if !batch.frames.is_empty()
            && batch_age_is_stale(
                input.inclusion_block,
                first_frame_safe_block,
                self.max_wait_blocks,
            )
        {
            return None;
        }
        if batch.nonce != expected_nonce {
            return None;
        }
        Some(AcceptedBatch {
            safe_input_index: input.safe_input_index,
            nonce: batch.nonce,
            first_frame_safe_block,
            inclusion_block: input.inclusion_block,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sequencer_core::batch::{Batch, Frame};

    const SUBMITTER: Address = Address::repeat_byte(0xAA);
    const OTHER: Address = Address::repeat_byte(0xBB);
    const MAX_WAIT: u64 = 1200;

    fn rules() -> SchedulerRules {
        SchedulerRules::new(SUBMITTER, MAX_WAIT)
    }

    fn encode(batch: &Batch) -> Vec<u8> {
        ssz::Encode::as_ssz_bytes(batch)
    }

    fn single_frame_batch(nonce: u64, safe_block: u64) -> Batch {
        Batch {
            nonce,
            frames: vec![Frame {
                user_ops: vec![],
                safe_block,
                fee_price: 0,
            }],
        }
    }

    #[test]
    fn accepts_fresh_batch_with_matching_nonce() {
        let payload = encode(&single_frame_batch(3, 100));
        let input = SafeInputRef {
            safe_input_index: 7,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: 500,
        };
        let accepted = rules()
            .evaluate(input, 3)
            .expect("matching nonce + fresh inclusion should be accepted");
        assert_eq!(accepted.safe_input_index, 7);
        assert_eq!(accepted.nonce, 3);
        assert_eq!(accepted.first_frame_safe_block, 100);
        assert_eq!(accepted.inclusion_block, 500);
    }

    #[test]
    fn rejects_wrong_sender() {
        let payload = encode(&single_frame_batch(0, 0));
        let input = SafeInputRef {
            safe_input_index: 0,
            sender: OTHER,
            payload: payload.as_slice(),
            inclusion_block: 0,
        };
        assert!(rules().evaluate(input, 0).is_none());
    }

    #[test]
    fn rejects_stale_by_inclusion() {
        let payload = encode(&single_frame_batch(0, 0));
        let input = SafeInputRef {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: MAX_WAIT,
        };
        assert!(rules().evaluate(input, 0).is_none());
    }

    #[test]
    fn accepts_boundary_just_below_stale() {
        let payload = encode(&single_frame_batch(0, 1));
        let input = SafeInputRef {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: MAX_WAIT,
        };
        // inclusion - first_frame = MAX_WAIT - 1, strictly below threshold.
        assert!(rules().evaluate(input, 0).is_some());
    }

    #[test]
    fn rejects_nonce_mismatch() {
        let payload = encode(&single_frame_batch(2, 100));
        let input = SafeInputRef {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: 200,
        };
        assert!(rules().evaluate(input, 3).is_none());
        assert!(rules().evaluate(input, 1).is_none());
    }

    #[test]
    fn rejects_garbage_payload() {
        let input = SafeInputRef {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: &[0xFF, 0xEE, 0xDD],
            inclusion_block: 0,
        };
        assert!(rules().evaluate(input, 0).is_none());
    }

    #[test]
    fn accepts_empty_frames_batch_regardless_of_inclusion_age() {
        // An empty-frames batch has no first_frame_safe_block to check; the
        // staleness predicate gates on `!frames.is_empty()` and skips the
        // check. Matches what the scheduler does — empty batches are noop
        // nonces that still advance the expected nonce.
        let payload = encode(&Batch {
            nonce: 0,
            frames: vec![],
        });
        let input = SafeInputRef {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: MAX_WAIT.saturating_mul(10),
        };
        assert!(rules().evaluate(input, 0).is_some());
    }
}
