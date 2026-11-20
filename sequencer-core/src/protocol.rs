// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The scheduler's staleness rule and its off-chain acceptance predicate,
//! which must agree with the canonical scheduler (I1), plus the frame clock.

use crate::batch::Batch;
use alloy_primitives::Address;

/// Advance logical frame time after this many newly-safe L1 blocks.
///
/// Protocol-visible, deliberately not configurable: user ops validate at
/// their frame's safe block, so this value is the application-clock
/// granularity (about a minute on mainnet), a product semantics decision
/// — not a lane implementation detail (prose owner:
/// `docs/protocol/scheduler-semantics.md`, frame-clock policy). The
/// observed tip is used directly, so delayed or epoch-sized observations
/// create one frame and never synthesize missed intermediate ticks.
pub const FRAME_CLOCK_INTERVAL_SAFE_BLOCKS: u64 = 5;

/// Off-chain simulation of the scheduler's batch-acceptance predicate, under
/// the application's staleness window `max_wait_blocks`.
///
/// Returns `Some(AcceptedBatch)` iff the scheduler would accept the input
/// at the given `expected_nonce`. The caller threads `expected_nonce`
/// across a stream of inputs, advancing by one on each `Some`.
///
/// Rejection paths (wrong sender, SSZ decode failure, stale by inclusion,
/// nonce mismatch) return `None` without advancing — matching what the
/// scheduler does on-chain.
///
/// **Deliberately omitted rejects: a self-trust.** The canonical scheduler
/// also rejects a batch with a frame whose `safe_block` is not strictly
/// below the inclusion block (`SafeBlockNotBelowInclusionBlock`), with
/// safe blocks that decrease across frames (`NonMonotonicSafeBlocks`), or
/// with a fee above `MAX_EXPONENT` (`FeePriceOutOfRange`): gate d. Gate f
/// rejects one whose first frame lies below the scheduler clock
/// (`FirstFrameBelowClock`). This predicate re-checks none of them. A
/// correct submitter never trips them: it opens frames in order, only at
/// safe heads it has already seen, so its batch lands in a later block.
/// Gate f belongs to the same omission for a further reason: the clock
/// rises with every executed direct, backstop and overflow drains
/// included, so the predicate could know it only by replaying the
/// directs. An overflow drain can still make gate f reject a sealed batch;
/// the watchdog's divergence latch reports that, and a manual recovery
/// answers it. The I1 duality test pins every omission as a *documented*
/// asymmetry; mirror them here if the submitter stops being trusted.
pub fn scheduler_accepts(
    batch_submitter: Address,
    max_wait_blocks: u64,
    input: SafeInputView<'_>,
    expected_nonce: u64,
) -> Option<AcceptedBatch> {
    if input.sender != batch_submitter {
        return None;
    }
    let batch = <Batch as ssz::Decode>::from_ssz_bytes(input.payload).ok()?;
    let first_frame_safe_block = batch.frames.first().map(|f| f.safe_block).unwrap_or(0);
    // Gate e: a batch that lands `max_wait_blocks` or more after its first
    // frame's safe block is stale.
    if !batch.frames.is_empty()
        && age_exceeds(
            input.inclusion_block,
            first_frame_safe_block,
            max_wait_blocks,
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

/// The one staleness formula, `now - start >= window`. Saturating keeps the
/// consensus path total when `start` is ahead of `now`. Callers: the
/// scheduler's gate e and overdue-direct backstop, the off-chain
/// [`scheduler_accepts`], and the sequencer's void-history check.
pub fn age_exceeds(now: u64, start: u64, window: u64) -> bool {
    now.saturating_sub(start) >= window
}

/// Borrowed view of one safe-input row, in the shape scheduler_accepts needs.
/// Using a borrowed payload avoids copying during iteration.
#[derive(Debug, Clone, Copy)]
pub struct SafeInputView<'a> {
    pub safe_input_index: u64,
    pub sender: Address,
    pub payload: &'a [u8],
    pub inclusion_block: u64,
}

/// One batch submission the scheduler would accept as part of its gold frontier.
#[derive(Debug, Clone, Copy)]
pub struct AcceptedBatch {
    pub safe_input_index: u64,
    pub nonce: u64,
    pub first_frame_safe_block: u64,
    pub inclusion_block: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Batch, Frame};

    const SUBMITTER: Address = Address::repeat_byte(0xAA);
    const OTHER: Address = Address::repeat_byte(0xBB);
    const MAX_WAIT: u64 = 1200;

    fn accepts(input: SafeInputView<'_>, expected_nonce: u64) -> Option<AcceptedBatch> {
        scheduler_accepts(SUBMITTER, MAX_WAIT, input, expected_nonce)
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
    fn age_exceeds_saturates_on_underflow() {
        assert!(!age_exceeds(5, 10, 1));
        assert!(age_exceeds(1200, 0, 1200));
        assert!(!age_exceeds(1199, 0, 1200));
    }

    #[test]
    fn scheduler_accepts_fresh_batch_with_matching_nonce() {
        let payload = encode(&single_frame_batch(3, 100));
        let input = SafeInputView {
            safe_input_index: 7,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: 500,
        };
        let accepted =
            accepts(input, 3).expect("matching nonce + fresh inclusion should be accepted");
        assert_eq!(accepted.safe_input_index, 7);
        assert_eq!(accepted.nonce, 3);
        assert_eq!(accepted.first_frame_safe_block, 100);
        assert_eq!(accepted.inclusion_block, 500);
    }

    #[test]
    fn scheduler_rejects_wrong_sender() {
        let payload = encode(&single_frame_batch(0, 0));
        let input = SafeInputView {
            safe_input_index: 0,
            sender: OTHER,
            payload: payload.as_slice(),
            inclusion_block: 0,
        };
        assert!(accepts(input, 0).is_none());
    }

    #[test]
    fn scheduler_rejects_stale_by_inclusion() {
        let payload = encode(&single_frame_batch(0, 0));
        let input = SafeInputView {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: MAX_WAIT,
        };
        assert!(accepts(input, 0).is_none());
    }

    #[test]
    fn scheduler_accepts_boundary_just_below_stale() {
        let payload = encode(&single_frame_batch(0, 1));
        let input = SafeInputView {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: MAX_WAIT,
        };
        assert!(accepts(input, 0).is_some());
    }

    #[test]
    fn scheduler_accepts_a_first_frame_ahead_of_inclusion_as_fresh() {
        // Degenerate input: the first frame is ahead of the inclusion block.
        // Saturating subtraction yields age 0, never stale; gate d, which
        // the predicate omits, is what rejects it on chain.
        let payload = encode(&single_frame_batch(0, 100));
        let input = SafeInputView {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: 50,
        };
        assert!(accepts(input, 0).is_some());
    }

    #[test]
    fn scheduler_rejects_nonce_mismatch() {
        let payload = encode(&single_frame_batch(2, 100));
        let input = SafeInputView {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: 200,
        };
        assert!(accepts(input, 3).is_none());
        assert!(accepts(input, 1).is_none());
    }

    #[test]
    fn scheduler_rejects_garbage_payload() {
        let input = SafeInputView {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: &[0xFF, 0xEE, 0xDD],
            inclusion_block: 0,
        };
        assert!(accepts(input, 0).is_none());
    }

    #[test]
    fn scheduler_accepts_empty_frames_batch_regardless_of_age() {
        let payload = encode(&Batch {
            nonce: 0,
            frames: vec![],
        });
        let input = SafeInputView {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: payload.as_slice(),
            inclusion_block: MAX_WAIT.saturating_mul(10),
        };
        assert!(accepts(input, 0).is_some());
    }
}
