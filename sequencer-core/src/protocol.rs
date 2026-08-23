// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Protocol timing parameters the sequencer mirrors from the scheduler, plus
//! the sequencer-side tuning knobs that govern preemptive self-protection.
//!
//! [`ProtocolTiming`] is the single source of truth for time-based protocol
//! rules:
//!
//! - **Scheduler-acceptance** timing (`is_scheduler_stale`, `scheduler_accepts`).
//!   `max_wait_blocks` matches the on-chain scheduler exactly — mis-aligning
//!   it would cause the sequencer's cached "gold frontier" to diverge from
//!   the scheduler's actual accepted set.
//! - **Preemptive-recovery** tuning (`danger_threshold`,
//!   `l1_read_stale_after_blocks`, `seconds_per_block`). These do not exist
//!   on the scheduler side; they control when the sequencer proactively
//!   stops to avoid issuing soft confirmations against stale or unknowable
//!   L1 state.
//!
//! The batch-submitter address is **not** part of `ProtocolTiming`. It's an
//! identity, not a timing parameter; the validation in
//! [`ProtocolTiming::try_new`] only relates timing fields to each other.
//! Predicates that need the address (`scheduler_accepts`) take it as a
//! parameter — see those methods.

use crate::batch::Batch;
use alloy_primitives::Address;
use thiserror::Error;

/// Error surfaced by [`ProtocolTiming::try_new`] when timing fields would
/// produce an unusable danger threshold.
///
/// Returning a typed error rather than panicking lets the runtime convert this
/// into a `Result` at config-parse time and surface it through the structured
/// `RunError` taxonomy, instead of crashing later inside
/// [`ProtocolTiming::danger_threshold`] (or worse, inside a logging macro).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolTimingError {
    /// `preemptive_margin_blocks == 0` — zero-width preemptive zone defeats
    /// the entire margin design (recovery only ever fires at the absolute max,
    /// with no operator runway). Always an operator misconfig.
    #[error("preemptive_margin_blocks must be greater than zero")]
    MarginZero,
    /// `preemptive_margin_blocks >= max_wait_blocks` — the danger threshold
    /// would be 0, making preemptive recovery indistinguishable from hard
    /// staleness. The margin is supposed to be operator runway *before*
    /// hitting `MAX_WAIT_BLOCKS`, so this is always an operator misconfig.
    #[error(
        "preemptive_margin_blocks ({margin}) must be strictly less than \
         max_wait_blocks ({max_wait})"
    )]
    MarginNotLessThanMaxWait { margin: u64, max_wait: u64 },
    /// `l1_read_stale_after_blocks == 0` would make the L1 view unusable
    /// immediately, so the sequencer could never start.
    #[error("l1_read_stale_after_blocks must be greater than zero")]
    ReadStaleAfterZero,
    /// The read-staleness threshold must fire strictly before the write danger
    /// threshold. Equality would have both refusal arms (L1ViewStale and
    /// EstimatedBatchInDanger) trip at the same point, defeating the
    /// "stale fires first" design property.
    #[error(
        "l1_read_stale_after_blocks ({read_stale_after}) must be strictly \
         less than danger_threshold ({danger_threshold})"
    )]
    ReadStaleAfterPastDanger {
        read_stale_after: u64,
        danger_threshold: u64,
    },
    /// A zero block-time estimate makes the wall-clock fallback undefined.
    #[error("seconds_per_block must be greater than zero")]
    SecondsPerBlockZero,
    /// The configured read-staleness horizon must fit in the `u64` Unix-time
    /// arithmetic used by the detector.
    #[error(
        "l1_read_stale_after_blocks ({read_stale_after}) * seconds_per_block \
         ({seconds_per_block}) exceeds u64::MAX seconds"
    )]
    ReadStaleWindowOverflow {
        read_stale_after: u64,
        seconds_per_block: u64,
    },
}

/// The local wall clock predates the persisted safe-head progress baseline.
///
/// This is an unusable timing environment, not a storage invariant violation:
/// callers must stop issuing soft confirmations until the clock catches up or
/// a new safe-head advance establishes a usable baseline.
#[derive(Debug, Error, PartialEq, Eq)]
#[error(
    "current wall clock ({now_ms} ms) predates last safe-head progress \
     ({last_safe_progress_ms} ms)"
)]
pub struct WallClockRegression {
    pub now_ms: u64,
    pub last_safe_progress_ms: u64,
}

/// Time-based protocol parameters: scheduler-mirroring `max_wait_blocks`
/// plus sequencer-side preemptive-recovery tuning. No identity (the
/// batch-submitter address is passed separately to predicates that need it).
///
/// Construct via [`ProtocolTiming::try_new`] in production code so the
/// margin/stale relationships are checked once up front. The fields stay
/// public to keep test fixtures concise — direct struct-literal construction
/// is fine where the inputs are controlled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolTiming {
    /// `MAX_WAIT_BLOCKS` — after this many L1 blocks, the scheduler skips a
    /// submitted batch as stale.
    pub max_wait_blocks: u64,
    /// How many blocks before `max_wait_blocks` the sequencer triggers
    /// preemptive recovery. Sequencer-local; must be strictly less than
    /// `max_wait_blocks` (enforced by [`ProtocolTiming::try_new`]).
    pub preemptive_margin_blocks: u64,
    /// How long the L1 safe block itself may be stale, measured in assumed L1
    /// blocks. Sequencer-local; startup refuses once the safe block timestamp
    /// is older than this threshold.
    pub l1_read_stale_after_blocks: u64,
    /// Wall-clock estimate of L1 block time, used as a fallback when the L1
    /// safe head appears frozen. Sequencer-local.
    pub seconds_per_block: u64,
}

impl ProtocolTiming {
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

    /// Validated constructor. Rejects timing configurations that would
    /// produce an unusable danger threshold or a degenerate margin.
    ///
    /// Production callers should use this; tests can still construct
    /// `ProtocolTiming` directly via struct-literal syntax with controlled
    /// inputs.
    pub fn try_new(
        max_wait_blocks: u64,
        preemptive_margin_blocks: u64,
        l1_read_stale_after_blocks: u64,
        seconds_per_block: u64,
    ) -> Result<Self, ProtocolTimingError> {
        if preemptive_margin_blocks == 0 {
            return Err(ProtocolTimingError::MarginZero);
        }
        if preemptive_margin_blocks >= max_wait_blocks {
            return Err(ProtocolTimingError::MarginNotLessThanMaxWait {
                margin: preemptive_margin_blocks,
                max_wait: max_wait_blocks,
            });
        }
        if l1_read_stale_after_blocks == 0 {
            return Err(ProtocolTimingError::ReadStaleAfterZero);
        }
        if seconds_per_block == 0 {
            return Err(ProtocolTimingError::SecondsPerBlockZero);
        }
        let danger_threshold = max_wait_blocks - preemptive_margin_blocks;
        if l1_read_stale_after_blocks >= danger_threshold {
            return Err(ProtocolTimingError::ReadStaleAfterPastDanger {
                read_stale_after: l1_read_stale_after_blocks,
                danger_threshold,
            });
        }
        l1_read_stale_after_blocks
            .checked_mul(seconds_per_block)
            .ok_or(ProtocolTimingError::ReadStaleWindowOverflow {
                read_stale_after: l1_read_stale_after_blocks,
                seconds_per_block,
            })?;
        Ok(Self {
            max_wait_blocks,
            preemptive_margin_blocks,
            l1_read_stale_after_blocks,
            seconds_per_block,
        })
    }

    /// The block-age threshold at which preemptive recovery triggers.
    ///
    /// Production code goes through [`ProtocolTiming::try_new`], which rejects
    /// an invalid margin up front. A directly-constructed invalid value is a
    /// test/programming bug and fails loud here.
    pub fn danger_threshold(&self) -> u64 {
        assert!(
            self.preemptive_margin_blocks > 0,
            "ProtocolTiming must be constructed with a nonzero preemptive margin"
        );
        assert!(
            self.preemptive_margin_blocks < self.max_wait_blocks,
            "ProtocolTiming must be constructed with margin < max_wait_blocks"
        );
        self.max_wait_blocks - self.preemptive_margin_blocks
    }

    /// Wall-clock age, in seconds, after which the L1 safe block is too old
    /// for the sequencer to trust its L1 view.
    pub fn l1_read_stale_after_secs(&self) -> u64 {
        assert!(
            self.l1_read_stale_after_blocks > 0,
            "ProtocolTiming must be constructed with a nonzero L1 read-staleness window"
        );
        assert!(
            self.seconds_per_block > 0,
            "ProtocolTiming must be constructed with nonzero seconds_per_block"
        );
        self.l1_read_stale_after_blocks
            .checked_mul(self.seconds_per_block)
            .expect("validated L1 read-staleness window must fit in u64 seconds")
    }

    /// Whether the safe block timestamp is too old to support recovery or
    /// continued soft confirmations. `None` means the view is unknown and is
    /// treated as unusable.
    pub fn l1_view_is_stale(&self, safe_block_timestamp_secs: Option<u64>, now_ms: u64) -> bool {
        assert!(
            self.seconds_per_block > 0,
            "ProtocolTiming must be constructed with nonzero seconds_per_block"
        );
        let Some(timestamp_secs) = safe_block_timestamp_secs else {
            return true;
        };
        let now_secs = now_ms / 1000;
        let Some(age_secs) = now_secs.checked_sub(timestamp_secs) else {
            // Ahead-of-clock is not staleness — the observation is the
            // freshest possible. Whether the local *clock* is usable against
            // it is a separate fault ([`Self::clock_cannot_age_l1_view`]),
            // deliberately checked after the observed danger arms.
            return false;
        };
        age_secs >= self.l1_read_stale_after_secs()
    }

    /// Whether the local clock is a full block-time or more behind the
    /// persisted L1 safe-block timestamp — i.e. it cannot age the view at
    /// all. Sub-block ahead-ness is ordinary NTP-scale skew against a
    /// block-granular timestamp (usable, age 0) and is tolerated.
    ///
    /// This is a clock fault, not a view fault: `check_danger` evaluates it
    /// only after the observed danger arms, which are pure block arithmetic
    /// and must not be suppressed by a broken local clock.
    pub fn clock_cannot_age_l1_view(
        &self,
        safe_block_timestamp_secs: Option<u64>,
        now_ms: u64,
    ) -> bool {
        assert!(
            self.seconds_per_block > 0,
            "ProtocolTiming must be constructed with nonzero seconds_per_block"
        );
        let Some(timestamp_secs) = safe_block_timestamp_secs else {
            return false;
        };
        // Compare at millisecond precision: flooring `now_ms` to seconds
        // would make the full-block refusal fire up to 999 ms early,
        // contradicting the sub-block tolerance. Saturating multiplies are
        // safe: a saturated ahead-side only widens toward refusal for
        // timestamps that are absurd anyway.
        let timestamp_ms = timestamp_secs.saturating_mul(1000);
        match timestamp_ms.checked_sub(now_ms) {
            Some(ahead_ms) => ahead_ms >= self.seconds_per_block.saturating_mul(1000),
            None => false,
        }
    }

    /// Wall-clock-adjusted danger threshold, used when the L1 safe head may be
    /// stale.
    ///
    /// Translates "wall-clock time elapsed since the last L1 safe-head
    /// observation" into "blocks the safe head is presumed to be behind", and
    /// shaves that off the strict [`Self::danger_threshold`]. Returns `None`
    /// when the wall-clock arm should be skipped:
    ///
    /// - `last_safe_progress_ms` is `None` — no baseline to extrapolate from.
    /// - Less than one block-time has elapsed — adjustment would be 0, so the
    ///   strict check covers this case directly.
    ///
    /// Returns `Ok(Some(adjusted))` where
    /// `adjusted = danger_threshold − (elapsed_secs / seconds_per_block)`,
    /// saturating at 0. A `now_ms` less than one block-time behind the
    /// baseline is treated as elapsed 0 — the estimate quantizes elapsed time
    /// into whole blocks, so a sub-block regression cannot change it and is
    /// ordinary clock-step noise. Returns [`WallClockRegression`] only
    /// for larger regressions, which genuinely invalidate the extrapolation;
    /// callers must treat that environment as unusable rather than silently
    /// reporting no elapsed time.
    pub fn wall_clock_adjusted_danger_threshold(
        &self,
        last_safe_progress_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<Option<u64>, WallClockRegression> {
        assert!(
            self.seconds_per_block > 0,
            "ProtocolTiming must be constructed with nonzero seconds_per_block"
        );
        let Some(last) = last_safe_progress_ms else {
            return Ok(None);
        };
        let elapsed_ms = match now_ms.checked_sub(last) {
            Some(elapsed) => elapsed,
            None => {
                let regression_secs = (last - now_ms) / 1000;
                if regression_secs < self.seconds_per_block {
                    return Ok(None);
                }
                return Err(WallClockRegression {
                    now_ms,
                    last_safe_progress_ms: last,
                });
            }
        };
        let elapsed_secs = elapsed_ms / 1000;
        let missed = elapsed_secs / self.seconds_per_block;
        if missed == 0 {
            return Ok(None);
        }
        Ok(Some(self.danger_threshold().saturating_sub(missed)))
    }

    /// Scheduler's staleness predicate: a batch is stale when
    /// `inclusion_block - first_frame_safe_block >= max_wait_blocks`. Used by
    /// the scheduler to skip stale submissions, and by the sequencer's
    /// frontier simulator to match that behavior.
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
    /// `batch_submitter` is the address the scheduler accepts batches from —
    /// kept as a parameter (rather than a struct field) because it's
    /// orthogonal to timing and the validation in [`Self::try_new`] doesn't
    /// touch it.
    ///
    /// Rejection paths (wrong sender, SSZ decode failure, stale by inclusion,
    /// nonce mismatch) return `None` without advancing — matching what the
    /// scheduler does on-chain.
    ///
    /// **Deliberately omitted structural rejects.** The canonical fold's
    /// `batch_reject_reason_for_block` additionally rejects a batch whose frames
    /// have a `safe_block` above the inclusion block
    /// (`SafeBlockAboveInclusionBlock`) or that decrease across frames
    /// (`NonMonotonicSafeBlocks`). This predicate does not re-check those: a
    /// well-formed batch submitter never builds such a batch, so they are
    /// reachable only under a buggy or forked submitter — already outside the
    /// duality boundary. The I1 duality test pins this as a *documented*
    /// asymmetry; mirror those two checks here if the submitter ever stops being
    /// trusted to be well-formed.
    pub fn scheduler_accepts(
        &self,
        batch_submitter: Address,
        input: SafeInputView<'_>,
        expected_nonce: u64,
    ) -> Option<AcceptedBatch> {
        if input.sender != batch_submitter {
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

/// Advance `expected` by greedily consuming any matching observed nonce.
///
/// `observed_nonces` is the stream of **batch nonces** (from the SSZ payload)
/// decoded from `InputAdded` events sent by our batch-submitter EOA, in L1
/// event order. Because L1 mines txs from a single EOA in strict wallet-nonce
/// order, this stream is naturally gap-less at the wallet-nonce level:
/// tx[k]'s event cannot appear on-chain without tx[k-1]'s event, and the
/// observed batch nonce sequence therefore mirrors our submission order.
///
/// Batch nonces themselves (unlike wallet nonces) CAN repeat across recovery
/// generations — e.g., after a cascade, a fresh batch reuses its invalidated
/// predecessor's nonce. That's why we still match on equality rather than
/// trusting a sort: in a post-recovery window, the same batch nonce can be
/// observed twice (once from the invalidated generation, once from the new
/// one), and we only want to advance once.
///
/// Under the wallet-nonce ordering above, once the next `expected` doesn't
/// appear in the stream the frontier naturally stops advancing — the gap
/// means the scheduler hasn't seen that nonce on-chain yet (or observed it at
/// a different wallet nonce from an earlier generation).
///
/// Co-located here next to [`ProtocolTiming::scheduler_accepts`] so all
/// scheduler-mirroring acceptance logic lives at the library seam (the
/// off-chain mirror of the canonical scheduler's batch-nonce fold).
pub fn advance_expected_batch_nonce(
    mut expected: u64,
    observed_nonces: impl IntoIterator<Item = u64>,
) -> u64 {
    for nonce in observed_nonces {
        if nonce == expected {
            expected = expected
                .checked_add(1)
                .expect("expected batch nonce overflow: contract-impossible");
        }
    }
    expected
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

    fn timing() -> ProtocolTiming {
        ProtocolTiming {
            max_wait_blocks: MAX_WAIT,
            preemptive_margin_blocks: 75,
            l1_read_stale_after_blocks: 900,
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
        assert_eq!(timing().danger_threshold(), MAX_WAIT - 75);
    }

    #[test]
    fn l1_read_stale_after_secs_uses_configured_block_time() {
        assert_eq!(timing().l1_read_stale_after_secs(), 900 * 12);
    }

    #[test]
    fn l1_view_is_stale_without_timestamp() {
        assert!(timing().l1_view_is_stale(None, 1_000_000));
    }

    #[test]
    fn l1_view_is_stale_at_threshold() {
        let cfg = timing();
        let timestamp_secs = 1_000;
        let before = (timestamp_secs + cfg.l1_read_stale_after_secs() - 1) * 1000;
        let at = (timestamp_secs + cfg.l1_read_stale_after_secs()) * 1000;
        assert!(!cfg.l1_view_is_stale(Some(timestamp_secs), before));
        assert!(cfg.l1_view_is_stale(Some(timestamp_secs), at));
    }

    #[test]
    fn l1_view_is_stale_never_fires_on_a_future_timestamp() {
        // Ahead-of-clock is not staleness — the observation is the freshest
        // possible. Clock usability is `clock_cannot_age_l1_view`'s job.
        assert!(!timing().l1_view_is_stale(Some(1_001), 1_000_000));
        assert!(!timing().l1_view_is_stale(Some(1_012), 1_000_000));
        assert!(
            !timing().l1_view_is_stale(Some(1_001), 1_001_000),
            "equality is a usable zero-age observation"
        );
    }

    #[test]
    fn clock_cannot_age_l1_view_tolerates_sub_block_skew() {
        let cfg = timing();
        // No view at all: nothing for the clock to age — not a clock fault.
        assert!(!cfg.clock_cannot_age_l1_view(None, 1_000_000));
        // Behind or equal: ordinary aging, never a clock fault.
        assert!(!cfg.clock_cannot_age_l1_view(Some(900), 1_000_000));
        assert!(!cfg.clock_cannot_age_l1_view(Some(1_000), 1_000_000));
        // Sub-block ahead-ness is NTP-scale skew against a block-granular
        // timestamp: usable, age 0.
        assert!(!cfg.clock_cannot_age_l1_view(Some(1_011), 1_000_000));
        // Millisecond precision at the boundary: 11.999 s and 11.001 s ahead
        // are both sub-block; exactly 12.000 s refuses.
        assert!(!cfg.clock_cannot_age_l1_view(Some(1_012), 1_000_001));
        assert!(!cfg.clock_cannot_age_l1_view(Some(1_012), 1_000_999));
        assert!(cfg.clock_cannot_age_l1_view(Some(1_012), 1_000_000));
    }

    #[test]
    #[should_panic(expected = "nonzero seconds_per_block")]
    fn l1_view_is_stale_fails_loud_on_zero_block_time_direct_construction() {
        let cfg = ProtocolTiming {
            seconds_per_block: 0,
            ..timing()
        };
        let _ = cfg.l1_view_is_stale(Some(1_001), 1_000_000);
    }

    #[test]
    #[should_panic(expected = "nonzero seconds_per_block")]
    fn clock_cannot_age_l1_view_fails_loud_on_zero_block_time_direct_construction() {
        let cfg = ProtocolTiming {
            seconds_per_block: 0,
            ..timing()
        };
        let _ = cfg.clock_cannot_age_l1_view(Some(1_001), 1_000_000);
    }

    #[test]
    fn wall_clock_adjusted_threshold_returns_none_without_baseline() {
        assert_eq!(
            timing().wall_clock_adjusted_danger_threshold(None, 1_000_000),
            Ok(None),
        );
    }

    #[test]
    fn wall_clock_adjusted_threshold_returns_none_below_one_block_time() {
        // 11 seconds elapsed, seconds_per_block = 12 → missed = 0.
        let last = 1_000_000;
        let now = last + 11_000;
        assert_eq!(
            timing().wall_clock_adjusted_danger_threshold(Some(last), now),
            Ok(None),
        );
    }

    #[test]
    fn wall_clock_adjusted_threshold_rejects_regression() {
        let last = 1_000_000;
        // Sub-block regressions are quantization noise: identical outcome to
        // elapsed = 0 for a block-granular estimate (clock steps are
        // legitimate).
        assert_eq!(
            timing().wall_clock_adjusted_danger_threshold(Some(last), last - 1),
            Ok(None),
            "a sub-block clock step must not be a fault"
        );
        assert_eq!(
            timing().wall_clock_adjusted_danger_threshold(Some(last), last - 11_999),
            Ok(None),
            "just under one block-time of regression is still noise"
        );
        // A regression of one block-time or more genuinely invalidates the
        // extrapolation.
        assert_eq!(
            timing().wall_clock_adjusted_danger_threshold(Some(last), last - 12_000),
            Err(WallClockRegression {
                now_ms: last - 12_000,
                last_safe_progress_ms: last,
            }),
        );
        assert_eq!(
            timing().wall_clock_adjusted_danger_threshold(Some(last), last),
            Ok(None),
            "equality is a usable zero-elapsed baseline"
        );
    }

    #[test]
    fn wall_clock_adjusted_threshold_shaves_one_block_per_block_time() {
        // 25 blocks of elapsed time (300s) → adjusted = danger_threshold - 25.
        let last = 1_000_000;
        let now = last + 300_000;
        let cfg = timing();
        assert_eq!(
            cfg.wall_clock_adjusted_danger_threshold(Some(last), now),
            Ok(Some(cfg.danger_threshold() - 25)),
        );
    }

    #[test]
    fn wall_clock_adjusted_threshold_saturates_at_zero() {
        // Wildly long outage — adjustment shaves more blocks than the threshold.
        let last = 0;
        let now = u64::MAX / 2;
        assert_eq!(
            timing().wall_clock_adjusted_danger_threshold(Some(last), now),
            Ok(Some(0)),
        );
    }

    #[test]
    #[should_panic(expected = "ProtocolTiming must be constructed with margin < max_wait_blocks")]
    fn danger_threshold_fails_loud_on_invalid_direct_construction() {
        // Production uses try_new. A direct invalid struct literal is a
        // programming error and must not be normalized into threshold zero.
        let cfg = ProtocolTiming {
            preemptive_margin_blocks: MAX_WAIT,
            ..timing()
        };
        let _ = cfg.danger_threshold();
    }

    #[test]
    #[should_panic(expected = "nonzero preemptive margin")]
    fn danger_threshold_fails_loud_on_zero_margin_direct_construction() {
        let cfg = ProtocolTiming {
            preemptive_margin_blocks: 0,
            ..timing()
        };
        let _ = cfg.danger_threshold();
    }

    #[test]
    #[should_panic(expected = "nonzero seconds_per_block")]
    fn wall_clock_helpers_fail_loud_on_zero_block_time_direct_construction() {
        let cfg = ProtocolTiming {
            seconds_per_block: 0,
            ..timing()
        };
        let _ = cfg.wall_clock_adjusted_danger_threshold(None, 0);
    }

    #[test]
    fn try_new_rejects_margin_equal_to_max_wait() {
        assert_eq!(
            ProtocolTiming::try_new(MAX_WAIT, MAX_WAIT, 1, 12),
            Err(ProtocolTimingError::MarginNotLessThanMaxWait {
                margin: MAX_WAIT,
                max_wait: MAX_WAIT,
            }),
        );
    }

    #[test]
    fn try_new_rejects_margin_greater_than_max_wait() {
        assert_eq!(
            ProtocolTiming::try_new(MAX_WAIT, MAX_WAIT + 1, 1, 12),
            Err(ProtocolTimingError::MarginNotLessThanMaxWait {
                margin: MAX_WAIT + 1,
                max_wait: MAX_WAIT,
            }),
        );
    }

    #[test]
    fn try_new_accepts_max_margin_admitting_valid_stale() {
        // margin = MAX_WAIT - 2 is the largest margin that still admits a
        // valid stale value (stale must satisfy 0 < stale < danger, and
        // danger = MAX_WAIT - margin, so danger >= 2 is required for stale = 1
        // to be valid).
        let cfg = ProtocolTiming::try_new(MAX_WAIT, MAX_WAIT - 2, 1, 12)
            .expect("margin admitting stale = 1 must be accepted");
        assert_eq!(cfg.danger_threshold(), 2);
    }

    #[test]
    fn try_new_rejects_zero_margin() {
        assert_eq!(
            ProtocolTiming::try_new(MAX_WAIT, 0, 1, 12),
            Err(ProtocolTimingError::MarginZero),
        );
    }

    #[test]
    fn try_new_rejects_zero_l1_read_stale_after() {
        assert_eq!(
            ProtocolTiming::try_new(MAX_WAIT, 75, 0, 12),
            Err(ProtocolTimingError::ReadStaleAfterZero),
        );
    }

    #[test]
    fn try_new_rejects_zero_seconds_per_block() {
        assert_eq!(
            ProtocolTiming::try_new(MAX_WAIT, 75, 1, 0),
            Err(ProtocolTimingError::SecondsPerBlockZero),
        );
    }

    #[test]
    fn try_new_rejects_overflowing_read_staleness_window() {
        assert_eq!(
            ProtocolTiming::try_new(u64::MAX, 1, u64::MAX - 2, 2),
            Err(ProtocolTimingError::ReadStaleWindowOverflow {
                read_stale_after: u64::MAX - 2,
                seconds_per_block: 2,
            }),
        );
    }

    #[test]
    fn try_new_rejects_l1_read_stale_after_past_danger() {
        let danger_threshold = MAX_WAIT - 75;
        assert_eq!(
            ProtocolTiming::try_new(MAX_WAIT, 75, danger_threshold + 1, 12),
            Err(ProtocolTimingError::ReadStaleAfterPastDanger {
                read_stale_after: danger_threshold + 1,
                danger_threshold,
            }),
        );
    }

    #[test]
    fn try_new_rejects_l1_read_stale_after_equal_to_danger() {
        // Strict less-than: equality means L1ViewStale and
        // EstimatedBatchInDanger fire at the same point, defeating the
        // "stale fires first" design property.
        let danger_threshold = MAX_WAIT - 75;
        assert_eq!(
            ProtocolTiming::try_new(MAX_WAIT, 75, danger_threshold, 12),
            Err(ProtocolTimingError::ReadStaleAfterPastDanger {
                read_stale_after: danger_threshold,
                danger_threshold,
            }),
        );
    }

    #[test]
    fn age_exceeds_saturates_on_underflow() {
        assert!(!age_exceeds(5, 10, 1));
        assert!(age_exceeds(1200, 0, 1200));
        assert!(!age_exceeds(1199, 0, 1200));
    }

    // ── ProtocolTiming::is_scheduler_stale direct boundary tests ──────────
    //
    // Indirectly covered by `scheduler_accepts_boundary_just_below_stale`, but
    // the staleness predicate is load-bearing on its own (the scheduler skips
    // submissions that trip it) and deserves direct tests that don't go through
    // SSZ decoding.

    #[test]
    fn is_scheduler_stale_reports_false_below_threshold() {
        // age = inclusion - first = MAX_WAIT - 1, strictly below.
        assert!(!timing().is_scheduler_stale(MAX_WAIT, 1));
        // age = 0 (safe head right at the first frame).
        assert!(!timing().is_scheduler_stale(100, 100));
    }

    #[test]
    fn is_scheduler_stale_reports_true_at_and_past_threshold() {
        // age = MAX_WAIT exactly — `>=` comparison trips.
        assert!(timing().is_scheduler_stale(MAX_WAIT, 0));
        // age = MAX_WAIT + 1, clearly past.
        assert!(timing().is_scheduler_stale(MAX_WAIT + 1, 0));
    }

    #[test]
    fn is_scheduler_stale_saturates_when_first_frame_is_ahead() {
        // Degenerate input: safe head is behind the first frame's safe_block.
        // Saturating subtraction yields 0, strictly below threshold — never stale.
        assert!(!timing().is_scheduler_stale(50, 100));
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
        let accepted = timing()
            .scheduler_accepts(SUBMITTER, input, 3)
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
        assert!(timing().scheduler_accepts(SUBMITTER, input, 0).is_none());
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
        assert!(timing().scheduler_accepts(SUBMITTER, input, 0).is_none());
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
        assert!(timing().scheduler_accepts(SUBMITTER, input, 0).is_some());
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
        assert!(timing().scheduler_accepts(SUBMITTER, input, 3).is_none());
        assert!(timing().scheduler_accepts(SUBMITTER, input, 1).is_none());
    }

    #[test]
    fn scheduler_rejects_garbage_payload() {
        let input = SafeInputView {
            safe_input_index: 0,
            sender: SUBMITTER,
            payload: &[0xFF, 0xEE, 0xDD],
            inclusion_block: 0,
        };
        assert!(timing().scheduler_accepts(SUBMITTER, input, 0).is_none());
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
        assert!(timing().scheduler_accepts(SUBMITTER, input, 0).is_some());
    }

    #[test]
    fn advance_expected_batch_nonce_matches_scheduler_nonce_rule() {
        use super::advance_expected_batch_nonce;
        assert_eq!(advance_expected_batch_nonce(0, Vec::<u64>::new()), 0);
        assert_eq!(advance_expected_batch_nonce(0, vec![0, 1, 2]), 3);
        assert_eq!(advance_expected_batch_nonce(0, vec![0, 2, 3]), 1);
        assert_eq!(advance_expected_batch_nonce(0, vec![1, 2, 3]), 0);
        assert_eq!(advance_expected_batch_nonce(0, vec![0, 1, 1, 2]), 3);
        assert_eq!(
            advance_expected_batch_nonce(0, vec![6, 4, 3, 2, 2, 0, 1]),
            2
        );
        assert_eq!(advance_expected_batch_nonce(0, vec![0, 2, 1]), 2);
        assert_eq!(advance_expected_batch_nonce(2, vec![2, 3]), 4);
    }

    #[test]
    #[should_panic(expected = "expected batch nonce overflow: contract-impossible")]
    fn advance_expected_batch_nonce_fails_loud_on_overflow() {
        let _ = advance_expected_batch_nonce(u64::MAX, [u64::MAX]);
    }
}
