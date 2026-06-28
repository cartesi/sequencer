// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The recovery replay/fold engine: reconstruct logical app state and
//! the resume batch nonce from a trusted checkpoint plus an L1 input range.
//!
//! `(machine@checkpoint, L1 range) → (machine@S', resume nonce N)`. The engine
//! is a thin, deterministic driver over the [`Scheduler`] library: it seeds the
//! fridge from the `(A, B]` directs (step 3), replays the `(B, C]` stream
//! (step 4), drains the leftover fridge at `C` (step 5), and returns the
//! advanced [`Application`] state plus the scheduler's resume nonce.
//!
//! Read-only and pure: it never touches L1 or storage. The caller sources the
//! inputs (`setup --recovery` from the synced `safe_inputs`; a future standalone
//! tool from raw L1) and persists the result. Determinism is inherited from the
//! scheduler (FIFO fridge, fixed-width arithmetic, no iteration-order leakage);
//! the engine's only added obligation is that the caller feed inputs in
//! ascending L1 order within `(A, C]` (seeds `(A, B]` before replay `(B, C]`).
//! That contract is enforced fail-loud with always-on assertions — a violation
//! would silently drop a direct and yield a wrong recovered state.

use alloy_primitives::Address;
use alloy_sol_types::Eip712Domain;

use super::{Scheduler, SchedulerConfig, SchedulerInput};
use crate::application::Application;

/// One reconstructed L1 input for the fold, ordered ascending by inclusion
/// block (ties broken by safe-input index at the source). Mirrors
/// [`SchedulerInput`] minus the EIP-712 `domain` — that is one
/// deployment-wide constant the engine clones onto each input rather than
/// storing per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldInput {
    pub sender: Address,
    pub inclusion_block: u64,
    pub payload: Vec<u8>,
}

/// Run recovery steps 3–5 over a trusted checkpoint and an L1 range,
/// returning `(S', N')` = (advanced app state, resume batch nonce).
///
/// - `app` / `checkpoint_nonce`: the trusted checkpoint machine `S` at block
///   `B` and its scheduler nonce `N` (metadata — the bare-metal app cannot
///   recompute it, so the engine is *told* it via `resume_at`).
/// - `seeds`: directs reconstructed from `(A, B]`, with sequencer-sourced
///   batches already dropped by the caller (their content is already in `S`,
///   their frames' safe blocks `≤ A`). Must arrive in ascending L1 order.
/// - `replay`: the full `(B, C]` stream (batches + directs) in L1 order. The
///   scheduler classifies each input (batch iff `sender == sequencer_address`),
///   force-executes overdue directs on arrival, applies accepted batches,
///   drains covered fridge directs, and advances the nonce to `N'`.
/// - `stop_block`: `C`, the stable stopping block. The terminal drain runs here.
///
/// `checkpoint_nonce` is trusted unchallenged (the operator's checkpoint is
/// trusted input; there is no cheap on-chain oracle to validate it against).
/// Notices/vouchers produced during the fold are dropped:
/// recovery reconstructs *logical* state (balances, nonces), not the L1-facing
/// output stream.
pub fn fold_replay<A, S, R>(
    app: A,
    checkpoint_nonce: u64,
    config: SchedulerConfig,
    domain: Eip712Domain,
    seeds: S,
    replay: R,
    stop_block: u64,
) -> (A, u64)
where
    A: Application,
    S: IntoIterator<Item = FoldInput>,
    R: IntoIterator<Item = FoldInput>,
{
    let sequencer_address = config.sequencer_address;
    let mut scheduler = Scheduler::resume_at(app, config, checkpoint_nonce);

    // The engine's input contract (ascending L1 order, seeds in (A, B] before
    // replay in (B, C], everything `<= C`) is enforced fail-loud with always-on
    // `assert!` rather than `debug_assert!`: a violation means the caller sourced
    // the wrong inputs, and the failure mode is silent state divergence (a
    // dropped direct → a wrong recovered `S'` that still *looks* valid). For a
    // recovery engine that is the worst outcome, and the checks are negligible
    // against a once-per-recovery fold, so we pay for them in release too.

    // Step 3 — reconstruct the fridge as of B from the (A, B] directs.
    let mut last_seed_block: Option<u64> = None;
    for seed in seeds {
        assert!(
            last_seed_block.is_none_or(|prev| seed.inclusion_block >= prev),
            "fold seeds must arrive in ascending inclusion-block order"
        );
        // SC-3: seeds are reconstructed `(A, B]` *directs* — enqueue_direct skips
        // sender classification, so a mis-sourced `sender == sequencer_address`
        // row would be silently filed as a direct instead of a batch (a wrong
        // S'). Fail loud, matching the ordering/range asserts above.
        assert!(
            seed.sender != sequencer_address,
            "fold seed must be a direct input, not a sequencer batch (sender == sequencer_address)"
        );
        last_seed_block = Some(seed.inclusion_block);
        scheduler.enqueue_direct(seed.sender, seed.inclusion_block, seed.payload);
    }

    // Step 4 — replay (B, C] through the scheduler.
    let mut last_replay_block: Option<u64> = None;
    for input in replay {
        if last_replay_block.is_none() {
            // The first replay input opens `(B, C]`, which is strictly disjoint
            // from the seeds' `(A, B]`: a seed block is `≤ B`, a replay block is
            // `> B`, so the first replay block must come *strictly after* the
            // last seeded direct. (A shared boundary block would let the FIFO
            // fridge interleave the two ranges out of L1 order, since all seeds
            // are enqueued before any replay input regardless of within-block
            // index — see the disjoint-range contract in the docstring.)
            assert!(
                last_seed_block.is_none_or(|seed| input.inclusion_block > seed),
                "fold replay must start strictly after the last seeded direct (ranges are disjoint)"
            );
        }
        assert!(
            last_replay_block.is_none_or(|prev| input.inclusion_block >= prev),
            "fold replay inputs must arrive in ascending inclusion-block order"
        );
        last_replay_block = Some(input.inclusion_block);
        let _ = scheduler.process_input(SchedulerInput {
            sender: input.sender,
            inclusion_block: input.inclusion_block,
            domain: domain.clone(),
            payload: input.payload,
        });
    }

    // Step 5 — drain the leftover fridge at C. Every still-queued direct has
    // `inclusion_block <= C`, so this executes them all; the booting run, which
    // starts past C with no fridge, would otherwise never see them.
    let _ = scheduler.drain_covered_at(stop_block);

    // Fail loud on a dropped direct: after draining at C the fridge MUST be
    // empty. A non-empty fridge means an input arrived with `inclusion_block > C`
    // (seed or replay) — outside the fold's range — which `finish` would
    // otherwise discard silently, yielding a wrong `S'`.
    assert_eq!(
        scheduler.queued_direct_len(),
        0,
        "fold left {} direct(s) undrained at stop block {stop_block}: an input \
         had inclusion_block > C (caller sourced inputs outside (A, C])",
        scheduler.queued_direct_len(),
    );

    scheduler.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{AppError, AppOutputs, InvalidReason};
    use crate::batch::{Batch, Frame};
    use crate::l2_tx::{DirectInput, ValidUserOp};
    use crate::user_op::UserOp;
    use alloy_primitives::address;

    const SEQUENCER: Address = address!("0x1111111111111111111111111111111111111111");
    const DIRECT_SENDER: Address = address!("0x2222222222222222222222222222222222222222");
    const MAX_WAIT: u64 = 100;

    fn config() -> SchedulerConfig {
        SchedulerConfig {
            sequencer_address: SEQUENCER,
            max_wait_blocks: MAX_WAIT,
        }
    }

    fn domain() -> Eip712Domain {
        super::super::input_domain(1, Address::ZERO)
    }

    /// Minimal `Application` double for the engine tests: records the markers of
    /// executed direct inputs (first payload byte) in order, and advances a
    /// safe-block clock. No user ops are exercised here — the engine drives the
    /// scheduler over directs and empty/cover-only batches, so the user-op path
    /// is trivial. Snapshot lifecycle is unused by the fold.
    #[derive(Default, Clone)]
    struct FoldApp {
        executed_directs: Vec<u8>,
        safe_block: u64,
    }

    impl Application for FoldApp {
        const MAX_METHOD_PAYLOAD_BYTES: usize = 1 + 32 + 20;

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _current_fee: u16,
        ) -> Result<(), InvalidReason> {
            Ok(())
        }

        fn execute_valid_user_op(
            &mut self,
            _user_op: &ValidUserOp,
            safe_block: u64,
        ) -> Result<AppOutputs, AppError> {
            self.safe_block = self.safe_block.max(safe_block);
            Ok(Vec::new())
        }

        fn execute_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError> {
            self.executed_directs
                .push(input.payload.first().copied().unwrap_or(0));
            self.safe_block = self.safe_block.max(input.block_number);
            Ok(Vec::new())
        }

        fn executed_input_count(&self) -> u64 {
            self.executed_directs.len() as u64
        }

        fn last_executed_safe_block(&self) -> u64 {
            self.safe_block
        }

        fn from_dump(_prefix: &std::path::Path) -> Result<Self, AppError> {
            unimplemented!("FoldApp does not participate in snapshot lifecycle")
        }
        fn create_dump(&self, _prefix: &std::path::Path) -> Result<(), AppError> {
            unimplemented!("FoldApp does not participate in snapshot lifecycle")
        }
        fn delete_dump(_prefix: &std::path::Path) -> Result<(), AppError> {
            unimplemented!("FoldApp does not participate in snapshot lifecycle")
        }
        fn state_file_in_dump(_prefix: &std::path::Path) -> std::path::PathBuf {
            unimplemented!("FoldApp does not participate in snapshot lifecycle")
        }
    }

    fn direct(sender: Address, block: u64, marker: u8) -> FoldInput {
        FoldInput {
            sender,
            inclusion_block: block,
            payload: vec![marker],
        }
    }

    /// A batch input (from the sequencer) carrying `batch`, SSZ-encoded.
    fn batch(block: u64, batch: Batch) -> FoldInput {
        FoldInput {
            sender: SEQUENCER,
            inclusion_block: block,
            payload: ssz::Encode::as_ssz_bytes(&batch),
        }
    }

    /// An empty batch (no frames) at `nonce` — a scheduler no-op that only
    /// advances the expected nonce.
    fn empty_batch(block: u64, nonce: u64) -> FoldInput {
        batch(
            block,
            Batch {
                nonce,
                frames: vec![],
            },
        )
    }

    /// A batch at `nonce` with one user-op-free frame at `safe_block` — covers
    /// (drains) fridge directs with `inclusion_block <= safe_block` without
    /// needing signed user ops.
    fn cover_batch(block: u64, nonce: u64, safe_block: u64) -> FoldInput {
        batch(
            block,
            Batch {
                nonce,
                frames: vec![Frame {
                    user_ops: vec![],
                    safe_block,
                    fee_price: 0,
                }],
            },
        )
    }

    #[test]
    fn empty_fold_returns_checkpoint_state_and_nonce() {
        let (app, n) = fold_replay(
            FoldApp::default(),
            7,
            config(),
            domain(),
            Vec::new(),
            Vec::new(),
            1_000,
        );
        assert_eq!(n, 7, "no batches ⇒ nonce stays at the checkpoint");
        assert!(app.executed_directs.is_empty());
    }

    #[test]
    fn resume_nonce_advances_from_checkpoint_through_replayed_batches() {
        // Start at N0 = 5 (proves resume_at is honored, not 0); replay three
        // empty batches at the expected nonces ⇒ N' = 8.
        let replay = vec![
            empty_batch(100, 5),
            empty_batch(110, 6),
            empty_batch(120, 7),
        ];
        let (_app, n) = fold_replay(
            FoldApp::default(),
            5,
            config(),
            domain(),
            Vec::new(),
            replay,
            1_000,
        );
        assert_eq!(n, 8, "three accepted batches advance N from 5 to 8");
    }

    #[test]
    fn seeded_fridge_directs_drain_on_a_covering_batch_frame() {
        // Seed two (A,B] directs; replay a batch whose frame safe_block covers
        // both ⇒ they execute in FIFO order, before the nonce advances.
        let seeds = vec![
            direct(DIRECT_SENDER, 10, 0xAA),
            direct(DIRECT_SENDER, 20, 0xBB),
        ];
        let replay = vec![cover_batch(30, 0, 20)];
        let (app, n) = fold_replay(
            FoldApp::default(),
            0,
            config(),
            domain(),
            seeds,
            replay,
            1_000,
        );
        assert_eq!(
            app.executed_directs,
            vec![0xAA, 0xBB],
            "directs drain in L1 order"
        );
        assert_eq!(n, 1, "the covering batch advances the nonce");
    }

    #[test]
    fn step5_drains_all_leftover_directs_even_when_not_overdue() {
        // A direct seeded at block 25, no covering batch, C = 30, MAX_WAIT = 100.
        // The direct is NOT overdue at C (25 + 100 > 30), but it IS covered by C,
        // so step 5 must still execute it — proving drain-covered-at-C, not
        // drain-overdue. (An overdue-only drain would lose it.)
        let seeds = vec![direct(DIRECT_SENDER, 25, 0xCC)];
        let (app, _n) = fold_replay(
            FoldApp::default(),
            0,
            config(),
            domain(),
            seeds,
            Vec::new(),
            30,
        );
        assert_eq!(
            app.executed_directs,
            vec![0xCC],
            "a non-overdue leftover direct ≤ C must be drained at step 5"
        );
    }

    #[test]
    fn fold_is_deterministic_over_identical_inputs() {
        let seeds = vec![
            direct(DIRECT_SENDER, 5, 0x01),
            direct(DIRECT_SENDER, 15, 0x02),
        ];
        let replay = vec![cover_batch(40, 0, 15), empty_batch(50, 1)];
        let run = || {
            fold_replay(
                FoldApp::default(),
                0,
                config(),
                domain(),
                seeds.clone(),
                replay.clone(),
                100,
            )
        };
        let (app_a, n_a) = run();
        let (app_b, n_b) = run();
        assert_eq!(app_a.executed_directs, app_b.executed_directs);
        assert_eq!(app_a.safe_block, app_b.safe_block);
        assert_eq!(n_a, n_b);
    }

    // --- fail-loud contract guards (always-on, not debug-only) ---

    #[test]
    #[should_panic(expected = "undrained at stop block")]
    fn replay_input_past_stop_block_panics_instead_of_silently_dropping() {
        // A direct at block 50 with C = 30: never covered, never overdue, so it
        // lingers in the fridge after step 5. The booting run would never see it,
        // so a silent drop would diverge `S'` — the guard must catch it.
        let replay = vec![direct(DIRECT_SENDER, 50, 0xDD)];
        let _ = fold_replay(
            FoldApp::default(),
            0,
            config(),
            domain(),
            Vec::new(),
            replay,
            30,
        );
    }

    #[test]
    #[should_panic(expected = "seeds must arrive in ascending")]
    fn out_of_order_seeds_panic() {
        let seeds = vec![
            direct(DIRECT_SENDER, 20, 0x01),
            direct(DIRECT_SENDER, 10, 0x02),
        ];
        let _ = fold_replay(
            FoldApp::default(),
            0,
            config(),
            domain(),
            seeds,
            Vec::new(),
            100,
        );
    }

    #[test]
    #[should_panic(expected = "replay inputs must arrive in ascending")]
    fn out_of_order_replay_panics() {
        let replay = vec![
            direct(DIRECT_SENDER, 100, 0x01),
            direct(DIRECT_SENDER, 50, 0x02),
        ];
        let _ = fold_replay(
            FoldApp::default(),
            0,
            config(),
            domain(),
            Vec::new(),
            replay,
            200,
        );
    }

    #[test]
    #[should_panic(expected = "replay must start strictly after")]
    fn replay_starting_before_last_seed_panics() {
        // Seed a direct at B-ish (block 50); a replay input at block 10 precedes
        // it, which would interleave the (A,B] and (B,C] ranges out of L1 order.
        let seeds = vec![direct(DIRECT_SENDER, 50, 0x01)];
        let replay = vec![direct(DIRECT_SENDER, 10, 0x02)];
        let _ = fold_replay(
            FoldApp::default(),
            0,
            config(),
            domain(),
            seeds,
            replay,
            100,
        );
    }

    #[test]
    #[should_panic(expected = "replay must start strictly after")]
    fn replay_sharing_the_last_seed_block_panics() {
        // The (A,B] seeds and (B,C] replay are strictly disjoint: a replay input
        // at the SAME block as the last seed violates the contract (and would
        // risk FIFO interleaving), so the boundary assert is strict (`>`).
        let seeds = vec![direct(DIRECT_SENDER, 30, 0x01)];
        let replay = vec![direct(DIRECT_SENDER, 30, 0x02)];
        let _ = fold_replay(
            FoldApp::default(),
            0,
            config(),
            domain(),
            seeds,
            replay,
            100,
        );
    }

    #[test]
    fn fold_replay_reconstructs_identical_state_to_a_live_run() {
        // Differential: splitting the (genesis, C] stream at an intermediate B
        // and running fold_replay over (checkpoint@B, (A,B] seeds, (B,C] replay)
        // must reconstruct EXACTLY the (state, nonce) a single uninterrupted live
        // run produces. This is the property the (C,H1] bug violated — and
        // idempotence (the existing `fold_is_deterministic` test) cannot catch a
        // systematic reconstruction error, because it only re-runs fold against
        // itself.

        // Full input stream over (genesis, C=30], deliberately shaped so block B
        // = 15 leaves a non-empty (A,B] fridge (the seed path):
        //   block 5  direct(1)         — executed pre-A by the batch@10
        //   block 10 batch nonce0 sb5  — drains direct(1); A := 5
        //   block 12 direct(2)         — UNDRAINED at B=15 ⇒ a seed in (A=5, B=15]
        //   block 20 batch nonce1 sb12 — drains the seed direct(2)
        //   block 25 direct(3)         — drained only by the terminal drain at C
        let covering_batch = |nonce: u64, safe_block: u64| {
            batch(
                if nonce == 0 { 10 } else { 20 },
                Batch {
                    nonce,
                    frames: vec![Frame {
                        user_ops: vec![],
                        safe_block,
                        fee_price: 0,
                    }],
                },
            )
        };
        let pre_b = || {
            vec![
                direct(DIRECT_SENDER, 5, 1),
                covering_batch(0, 5),
                direct(DIRECT_SENDER, 12, 2),
            ]
        };
        let post_b = || vec![covering_batch(1, 12), direct(DIRECT_SENDER, 25, 3)];
        let stop = 30;

        let feed = |scheduler: &mut Scheduler<FoldApp>, inputs: Vec<FoldInput>| {
            for input in inputs {
                scheduler.process_input(SchedulerInput {
                    sender: input.sender,
                    inclusion_block: input.inclusion_block,
                    domain: domain(),
                    payload: input.payload,
                });
            }
        };

        // LIVE: one scheduler over the whole stream, terminal-drained at C.
        let mut live = Scheduler::new(FoldApp::default(), config());
        feed(&mut live, pre_b());
        feed(&mut live, post_b());
        live.drain_covered_at(stop);
        let (live_app, live_nonce) = live.finish();

        // CHECKPOINT @ B: a scheduler over (genesis, B] then `finish` WITHOUT a
        // drain — the undrained (A,B] directs are dropped from S and re-supplied
        // as seeds. This is exactly the checkpoint contract: every batch ≤ B
        // applied, no (A,B] direct executed yet.
        let mut at_b = Scheduler::new(FoldApp::default(), config());
        feed(&mut at_b, pre_b());
        let (checkpoint_app, checkpoint_nonce) = at_b.finish();
        let a = checkpoint_app.last_executed_safe_block();
        assert!(a < 12 && 12 <= 15, "the seed direct(2) must sit in (A, B]");

        // RECOVERY: fold the checkpoint over the (A,B] seeds + (B,C] replay to C.
        let seeds = vec![direct(DIRECT_SENDER, 12, 2)];
        let (recovered_app, recovered_nonce) = fold_replay(
            checkpoint_app,
            checkpoint_nonce,
            config(),
            domain(),
            seeds,
            post_b(),
            stop,
        );

        assert_eq!(
            recovered_nonce, live_nonce,
            "resume nonce N' must equal the live run's nonce"
        );
        assert_eq!(
            recovered_app.executed_directs, live_app.executed_directs,
            "executed directs (content AND order) must equal the live run"
        );
        assert_eq!(
            recovered_app.last_executed_safe_block(),
            live_app.last_executed_safe_block(),
            "the safe-block clock must equal the live run"
        );
        // Sanity: the stream actually executed all three directs (not a trivial pass).
        assert_eq!(recovered_app.executed_directs, vec![1, 2, 3]);
    }
}
