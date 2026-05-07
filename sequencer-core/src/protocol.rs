// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Protocol rules the sequencer mirrors from the scheduler, plus the
//! sequencer-side tuning knobs that govern preemptive self-protection.
//!
//! [`ProtocolConfig`] is the single source of truth for:
//!
//! - **Scheduler-acceptance** predicates (`scheduler_accepts`, `is_scheduler_stale`).
//!   These match the on-chain scheduler's behavior exactly — mis-aligning them
//!   would cause the sequencer's cached "gold frontier" to diverge from the
//!   scheduler's actual accepted set.
//! - **Preemptive-recovery** tuning (`danger_threshold`, `seconds_per_block`).
//!   These do not exist on the scheduler side; they control when the sequencer
//!   proactively stops to avoid letting a batch age into the scheduler's skip
//!   window.
//!
//! Keep the scheduler-mirroring fields (`batch_submitter`, `max_wait_blocks`)
//! aligned with the scheduler's config at deployment time. The two tuning
//! fields (`preemptive_margin_blocks`, `seconds_per_block`) are sequencer-local.

use crate::batch::Batch;
use alloy_primitives::Address;
use thiserror::Error;

/// Error surfaced by [`ProtocolConfig::try_new`] when the configuration would
/// produce an unusable danger threshold.
///
/// Returning a typed error rather than panicking lets the runtime convert this
/// into a `Result` at config-parse time and surface it through the structured
/// `RunError` taxonomy, instead of crashing later inside
/// [`ProtocolConfig::danger_threshold`] (or worse, inside a logging macro).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolConfigError {
    /// `preemptive_margin_blocks >= max_wait_blocks` — the danger threshold
    /// would be 0, making preemptive recovery indistinguishable from hard
    /// staleness. The margin is supposed to be operator runway *before*
    /// hitting `MAX_WAIT_BLOCKS`, so this is always an operator misconfig.
    #[error(
        "preemptive_margin_blocks ({margin}) must be strictly less than \
         max_wait_blocks ({max_wait})"
    )]
    MarginNotLessThanMaxWait { margin: u64, max_wait: u64 },
}

/// Bundled protocol config: scheduler-acceptance parameters plus
/// sequencer-side preemptive-recovery tuning.
///
/// Construct via [`ProtocolConfig::try_new`] in production code so the
/// `margin < max_wait` invariant is checked once up front. The fields stay
/// public to keep test fixtures concise — direct struct-literal construction
/// is fine where the inputs are controlled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolConfig {
    /// L1 address that submits batches. The scheduler only accepts batches
    /// whose `msg_sender` matches this.
    pub batch_submitter: Address,
    /// `MAX_WAIT_BLOCKS` — after this many L1 blocks, the scheduler skips a
    /// submitted batch as stale.
    pub max_wait_blocks: u64,
    /// How many blocks before `max_wait_blocks` the sequencer triggers
    /// preemptive recovery. Sequencer-local; must be strictly less than
    /// `max_wait_blocks` (enforced by [`ProtocolConfig::try_new`]).
    pub preemptive_margin_blocks: u64,
    /// Wall-clock estimate of L1 block time, used as a fallback when the L1
    /// safe head appears frozen. Sequencer-local.
    pub seconds_per_block: u64,
}

impl ProtocolConfig {
    /// Validated constructor. Returns
    /// [`ProtocolConfigError::MarginNotLessThanMaxWait`] when
    /// `preemptive_margin_blocks >= max_wait_blocks`.
    ///
    /// Production callers should use this; tests can still construct
    /// `ProtocolConfig` directly via struct-literal syntax with controlled
    /// inputs.
    pub fn try_new(
        batch_submitter: Address,
        max_wait_blocks: u64,
        preemptive_margin_blocks: u64,
        seconds_per_block: u64,
    ) -> Result<Self, ProtocolConfigError> {
        if preemptive_margin_blocks >= max_wait_blocks {
            return Err(ProtocolConfigError::MarginNotLessThanMaxWait {
                margin: preemptive_margin_blocks,
                max_wait: max_wait_blocks,
            });
        }
        Ok(Self {
            batch_submitter,
            max_wait_blocks,
            preemptive_margin_blocks,
            seconds_per_block,
        })
    }

    /// The block-age threshold at which preemptive recovery triggers.
    ///
    /// `saturating_sub` keeps this infallible even on a directly-constructed
    /// `ProtocolConfig` with an invalid margin (returns 0 in that case).
    /// Production code goes through [`ProtocolConfig::try_new`], which
    /// rejects that configuration up front.
    pub fn danger_threshold(&self) -> u64 {
        self.max_wait_blocks
            .saturating_sub(self.preemptive_margin_blocks)
    }

    /// Scheduler's staleness predicate: a batch is stale when
    /// `inclusion_block - first_frame_safe_block >= max_wait_blocks`. Used by
    /// the scheduler to skip stale submissions, and by the sequencer's frontier
    /// simulator to match that behavior.
    pub fn is_scheduler_stale(&self, inclusion_block: u64, first_frame_safe_block: u64) -> bool {
        age_exceeds(
            inclusion_block,
            first_frame_safe_block,
            self.max_wait_blocks,
        )
    }

    /// Off-chain simulation of the scheduler's batch-acceptance predicate.
    ///
    /// Returns `Some(AcceptedBatch)` iff the scheduler would accept the input
    /// at the given `expected_nonce`. The caller threads `expected_nonce`
    /// across a stream of inputs, advancing by one on each `Some`.
    ///
    /// Rejection paths (wrong sender, SSZ decode failure, stale by inclusion,
    /// nonce mismatch) return `None` without advancing — matching what the
    /// scheduler does on-chain.
    pub fn scheduler_accepts(
        &self,
        input: SafeInputView<'_>,
        expected_nonce: u64,
    ) -> Option<AcceptedBatch> {
        if input.sender != self.batch_submitter {
            return None;
        }
        let batch = <Batch as ssz::Decode>::from_ssz_bytes(input.payload).ok()?;
        let first_frame_safe_block = batch.frames.first().map(|f| f.safe_block).unwrap_or(0);
        if !batch.frames.is_empty()
            && self.is_scheduler_stale(input.inclusion_block, first_frame_safe_block)
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

/// Generic "age exceeds threshold" predicate shared between scheduler-staleness
/// and the preemptive danger-zone check. Saturating subtraction keeps the
/// arithmetic total over pathological inputs (safe head below a batch's first
/// frame).
pub fn age_exceeds(reference_block: u64, first_frame_safe_block: u64, threshold: u64) -> bool {
    reference_block.saturating_sub(first_frame_safe_block) >= threshold
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

    fn config() -> ProtocolConfig {
        ProtocolConfig {
            batch_submitter: SUBMITTER,
            max_wait_blocks: MAX_WAIT,
            preemptive_margin_blocks: 75,
            seconds_per_block: 12,
        }
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
    fn danger_threshold_is_max_wait_minus_margin() {
        assert_eq!(config().danger_threshold(), MAX_WAIT - 75);
    }

    #[test]
    fn danger_threshold_saturates_to_zero_on_invalid_margin() {
        // try_new rejects this configuration; if a test ever constructs it
        // directly via struct-literal syntax, danger_threshold returns 0
        // rather than panicking. (Cleaner than a hard panic during a logging
        // macro on production startup.)
        let cfg = ProtocolConfig {
            preemptive_margin_blocks: MAX_WAIT,
            ..config()
        };
        assert_eq!(cfg.danger_threshold(), 0);

        let cfg = ProtocolConfig {
            preemptive_margin_blocks: MAX_WAIT + 1,
            ..config()
        };
        assert_eq!(cfg.danger_threshold(), 0);
    }

    #[test]
    fn try_new_rejects_margin_equal_to_max_wait() {
        assert_eq!(
            ProtocolConfig::try_new(SUBMITTER, MAX_WAIT, MAX_WAIT, 12),
            Err(ProtocolConfigError::MarginNotLessThanMaxWait {
                margin: MAX_WAIT,
                max_wait: MAX_WAIT,
            }),
        );
    }

    #[test]
    fn try_new_rejects_margin_greater_than_max_wait() {
        assert_eq!(
            ProtocolConfig::try_new(SUBMITTER, MAX_WAIT, MAX_WAIT + 1, 12),
            Err(ProtocolConfigError::MarginNotLessThanMaxWait {
                margin: MAX_WAIT + 1,
                max_wait: MAX_WAIT,
            }),
        );
    }

    #[test]
    fn try_new_accepts_margin_one_below_max_wait() {
        let cfg = ProtocolConfig::try_new(SUBMITTER, MAX_WAIT, MAX_WAIT - 1, 12)
            .expect("strictly-less margin must be accepted");
        assert_eq!(cfg.danger_threshold(), 1);
    }

    #[test]
    fn try_new_accepts_zero_margin() {
        let cfg = ProtocolConfig::try_new(SUBMITTER, MAX_WAIT, 0, 12)
            .expect("zero margin is valid (degenerate but valid)");
        assert_eq!(cfg.danger_threshold(), MAX_WAIT);
    }

    #[test]
    fn age_exceeds_saturates_on_underflow() {
        assert!(!age_exceeds(5, 10, 1));
        assert!(age_exceeds(1200, 0, 1200));
        assert!(!age_exceeds(1199, 0, 1200));
    }

    // ── ProtocolConfig::is_scheduler_stale direct boundary tests ──────────
    //
    // Indirectly covered by `scheduler_accepts_boundary_just_below_stale`, but
    // the staleness predicate is load-bearing on its own (the scheduler skips
    // submissions that trip it) and deserves direct tests that don't go through
    // SSZ decoding.

    #[test]
    fn is_scheduler_stale_reports_false_below_threshold() {
        // age = inclusion - first = MAX_WAIT - 1, strictly below.
        assert!(!config().is_scheduler_stale(MAX_WAIT, 1));
        // age = 0 (safe head right at the first frame).
        assert!(!config().is_scheduler_stale(100, 100));
    }

    #[test]
    fn is_scheduler_stale_reports_true_at_and_past_threshold() {
        // age = MAX_WAIT exactly — `>=` comparison trips.
        assert!(config().is_scheduler_stale(MAX_WAIT, 0));
        // age = MAX_WAIT + 1, clearly past.
        assert!(config().is_scheduler_stale(MAX_WAIT + 1, 0));
    }

    #[test]
    fn is_scheduler_stale_saturates_when_first_frame_is_ahead() {
        // Degenerate input: safe head is behind the first frame's safe_block.
        // Saturating subtraction yields 0, strictly below threshold — never stale.
        assert!(!config().is_scheduler_stale(50, 100));
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
        let accepted = config()
            .scheduler_accepts(input, 3)
            .expect("matching nonce + fresh inclusion should be accepted");
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
        assert!(config().scheduler_accepts(input, 0).is_none());
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
        assert!(config().scheduler_accepts(input, 0).is_none());
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
        assert!(config().scheduler_accepts(input, 0).is_some());
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
        assert!(config().scheduler_accepts(input, 3).is_none());
        assert!(config().scheduler_accepts(input, 1).is_none());
    }

    #[test]
    fn scheduler_rejects_garbage_payload() {
        let input = SafeInputView {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: &[0xFF, 0xEE, 0xDD],
            inclusion_block: 0,
        };
        assert!(config().scheduler_accepts(input, 0).is_none());
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
        assert!(config().scheduler_accepts(input, 0).is_some());
    }
}
