// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use super::super::test_helpers::{
    RecoveryFixture, SENDER_A, all_ordered_l2_txs, default_protocol_timing, local_batch_payload,
    make_stale_batch_payload, open_temp_storage, seed_closed_batches,
    seed_safe_inputs_with_batch_nonces,
};
use super::{find_closed_frontier_batch_in_danger, find_first_batch_in_danger};
use crate::storage::{DirectInputExecution, ExecutedInputCount, SafeInputRange, Storage};
use alloy_primitives::Address;
use sequencer_core::l2_tx::SequencedL2Tx;
use sequencer_core::protocol::ProtocolTiming;

fn generation_cuts(storage: &Storage) -> Vec<(i64, i64)> {
    storage
        .conn
        .prepare("SELECT recovery_generation, preserved_input_count FROM history_generation_cuts ORDER BY recovery_generation")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

mod guarded_phases {
    use super::*;
    use crate::storage::RecoveryMutationError;

    fn invalidated_count(storage: &Storage) -> u64 {
        storage
            .conn
            .query_row(
                "SELECT COUNT(*) FROM batches WHERE invalidated_at_ms IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count invalidated batches") as u64
    }

    fn open_tip_count(storage: &Storage) -> u64 {
        storage
            .conn
            .query_row("SELECT COUNT(*) FROM valid_open_batch", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count open Tips") as u64
    }

    fn ensure_tip_fixture(
        name: &str,
        with_recovery_checkpoint: bool,
    ) -> (crate::test_support::TestDb, Storage, ProtocolTiming) {
        let (db, mut storage) = open_temp_storage(name);
        let protocol = default_protocol_timing();
        let now_ms = crate::clock::unix_now_ms();
        storage
            .append_safe_inputs_with_timestamp(
                0,
                now_ms / 1_000,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("seed fresh safe head");
        if with_recovery_checkpoint {
            let prefix = db._dir.path().join("finalized");
            storage
                .insert_baseline_snapshot(&prefix, ExecutedInputCount::ZERO)
                .expect("seed finalized snapshot");
        }
        (db, storage, protocol)
    }

    #[test]
    fn ensure_tip_guard_opens_one_tip_only_from_clean_facts() {
        let (_db, mut storage, protocol) = ensure_tip_fixture("guarded-ensure-tip", true);
        assert_eq!(open_tip_count(&storage), 0);

        storage
            .ensure_open_tip_for_recovery(&protocol, crate::clock::unix_now_ms())
            .expect("clean no-Tip facts authorize one Tip");
        assert_eq!(open_tip_count(&storage), 1);
    }

    #[test]
    fn ensure_tip_guard_refuses_missing_snapshot_without_mutation() {
        let (_db, mut storage, protocol) =
            ensure_tip_fixture("guarded-ensure-tip-no-snapshot", false);
        storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("a stale phase opened the Tip");

        let error = storage
            .ensure_open_tip_for_recovery(&protocol, crate::clock::unix_now_ms())
            .expect_err("missing finalized state must outrank the stale Tip decision");
        assert!(matches!(
            error,
            RecoveryMutationError::MissingRecoveryCheckpoint
        ));
        assert_eq!(open_tip_count(&storage), 1, "the existing Tip is untouched");
    }

    #[test]
    fn ensure_tip_guard_refuses_divergence_without_mutation() {
        let (_db, mut storage, protocol) =
            ensure_tip_fixture("guarded-ensure-tip-divergence", true);
        crate::storage::test_helpers::record_canonical_divergence(&mut storage, 7, 0);

        let error = storage
            .ensure_open_tip_for_recovery(&protocol, crate::clock::unix_now_ms())
            .expect_err("divergence must outrank Tip creation");
        assert!(matches!(
            error,
            RecoveryMutationError::CanonicalDivergence { nonce: 7 }
        ));
        assert_eq!(open_tip_count(&storage), 0);
    }

    #[test]
    fn ensure_tip_guard_rejects_an_already_open_tip_without_duplication() {
        let (_db, mut storage, protocol) = ensure_tip_fixture("guarded-ensure-tip-existing", true);
        storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("a competing phase opened the Tip");

        let error = storage
            .ensure_open_tip_for_recovery(&protocol, crate::clock::unix_now_ms())
            .expect_err("an unexpected open Tip must be rejected");
        assert!(matches!(error, RecoveryMutationError::TipAlreadyOpen));
        assert_eq!(open_tip_count(&storage), 1);
    }

    #[test]
    fn ensure_tip_refuses_and_rolls_back_if_its_transaction_leaves_no_tip() {
        // The fixture seeds no safe inputs on purpose: with a non-empty drain
        // range the invalidated Tip would trip the sequenced-rows trigger
        // first and the opener would fail with a storage error instead of
        // returning `Ok` — which is the exact shape this test needs.
        let (db, storage, protocol) = ensure_tip_fixture("guarded-ensure-tip-postcondition", true);
        // Fault injection: invalidate the new Tip the moment its first frame
        // lands, so `open_fresh_tip_in_tx` returns `Ok` having left no valid
        // open Tip. (Invalidating on the batch insert itself is caught one
        // step earlier by the schema — "frames can only be inserted into the
        // current Tip" — so the injection must fire after the frame.
        // `open.rs` documents failure triggers as the sanctioned way past the
        // typed API in tests.)
        drop(storage);
        Storage::open_connection(db.path.as_str())
            .expect("raw connection")
            .execute_batch(
                "CREATE TRIGGER test_invalidate_new_tip AFTER INSERT ON frames \
                 BEGIN \
                   UPDATE batches SET invalidated_at_ms = 1 \
                   WHERE batch_index = NEW.batch_index; \
                 END",
            )
            .expect("install failure trigger");
        let mut storage = Storage::open(db.path.as_str()).expect("reopen");

        let error = storage
            .ensure_open_tip_for_recovery(&protocol, crate::clock::unix_now_ms())
            .expect_err("a phase that leaves no Tip must refuse, not commit");
        assert!(
            matches!(error, RecoveryMutationError::TipMissingAfterOpen),
            "expected the postcondition refuse, got {error:?}"
        );
        assert_eq!(open_tip_count(&storage), 0);
        // Refused inside the transaction: nothing of the attempt persisted.
        let batches: i64 = storage
            .read(|tx| tx.query_row("SELECT COUNT(*) FROM batches", [], |row| row.get(0)))
            .expect("count batches");
        assert_eq!(batches, 0, "the refused phase must roll back its insert");
    }

    fn cascade_fixture(name: &str) -> (crate::test_support::TestDb, Storage) {
        let (db, mut storage) = open_temp_storage(name);
        let protocol = default_protocol_timing();
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize Tip");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close cascade candidate");
        storage
            .append_safe_inputs_with_timestamp(
                1_200,
                1_000,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("advance safe head");
        let prefix = db._dir.path().join("finalized");
        storage
            .insert_baseline_snapshot(&prefix, ExecutedInputCount::ZERO)
            .expect("seed finalized snapshot");
        (db, storage)
    }

    #[test]
    fn cascade_guard_ranks_divergence_before_f2_and_mutates_nothing() {
        let (_db, mut storage) = cascade_fixture("guarded-cascade-divergence");
        let protocol = default_protocol_timing();
        crate::storage::test_helpers::record_canonical_divergence(&mut storage, 0, 0);

        let error = storage
            .recover_post_flush_for_recovery(1_201, &protocol, crate::clock::unix_now_ms())
            .expect_err("divergence must refuse before lag");
        assert!(matches!(
            error,
            RecoveryMutationError::CanonicalDivergence { nonce: 0 }
        ));
        assert_eq!(invalidated_count(&storage), 0);
    }

    #[test]
    fn cascade_guard_rejects_lagging_sync_without_tree_mutation() {
        let (_db, mut storage) = cascade_fixture("guarded-cascade-f2");
        let protocol = default_protocol_timing();

        let error = storage
            .recover_post_flush_for_recovery(1_201, &protocol, crate::clock::unix_now_ms())
            .expect_err("lagging post-flush view must retry");
        assert!(matches!(
            error,
            RecoveryMutationError::ResyncBehindFlushView {
                resynced_safe_block: 1_200,
                flush_observed_safe_block: 1_201,
            }
        ));
        assert_eq!(invalidated_count(&storage), 0);
    }

    #[test]
    fn cascade_guard_ranks_missing_snapshot_before_f2_without_mutation() {
        let (_db, mut storage) = cascade_fixture("guarded-cascade-no-snapshot");
        let protocol = default_protocol_timing();
        storage
            .conn
            .execute("DELETE FROM snapshots WHERE batch_index IS NULL", [])
            .expect("remove finalized snapshot fact");

        let error = storage
            .recover_post_flush_for_recovery(1_201, &protocol, crate::clock::unix_now_ms())
            .expect_err("missing finalized state must outrank a lagging view");
        assert!(matches!(
            error,
            RecoveryMutationError::MissingRecoveryCheckpoint
        ));
        assert_eq!(invalidated_count(&storage), 0);
    }

    #[test]
    fn tip_guard_reasserts_the_exact_reducer_decision() {
        let (db, mut storage) = open_temp_storage("guarded-tip-decision");
        let protocol = default_protocol_timing();
        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize Tip");
        storage
            .append_safe_inputs_with_timestamp(
                1_200,
                1_000,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("advance safe head");
        let prefix = db._dir.path().join("finalized");
        storage
            .insert_baseline_snapshot(&prefix, ExecutedInputCount::ZERO)
            .expect("seed finalized snapshot");

        let error = storage
            .recover_aging_tip_for_recovery(1, &protocol, 1_000_000)
            .expect_err("a stale expected Tip must not mutate");
        assert!(matches!(
            error,
            RecoveryMutationError::StaleDecision {
                expected: crate::storage::DangerStatus::TipInDanger(1),
                actual: crate::storage::DangerStatus::TipInDanger(0),
            }
        ));
        assert_eq!(invalidated_count(&storage), 0);
    }

    #[test]
    fn tip_guard_refuses_missing_snapshot_without_tree_mutation() {
        let (_db, mut storage) = open_temp_storage("guarded-tip-no-snapshot");
        let protocol = default_protocol_timing();
        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize Tip");
        storage
            .append_safe_inputs_with_timestamp(
                1_200,
                1_000,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("advance safe head");

        let error = storage
            .recover_aging_tip_for_recovery(0, &protocol, 1_000_000)
            .expect_err("missing finalized state must refuse Tip recovery");
        assert!(matches!(
            error,
            RecoveryMutationError::MissingRecoveryCheckpoint
        ));
        assert_eq!(invalidated_count(&storage), 0);
    }
}

mod invalid_batches {
    use super::*;

    // ── invalid_batches filtering ──────────────────────────────────────

    #[test]
    fn invalid_batches_excluded_from_latest_batch_index() {
        let (_db, mut storage) = open_temp_storage("invalid-latest-batch");
        seed_closed_batches(&mut storage, 3);
        assert_eq!(
            storage.latest_batch_index().expect("latest").unwrap(),
            3,
            "open batch should be 3"
        );

        storage.insert_invalid_batch(3).expect("mark invalid");
        assert_eq!(storage.latest_batch_index().expect("latest").unwrap(), 2,);

        storage.insert_invalid_batch(2).expect("mark invalid");
        assert_eq!(storage.latest_batch_index().expect("latest").unwrap(), 1,);
    }

    #[test]
    fn invalid_batches_excluded_from_ordered_l2_txs() {
        let (_db, mut storage) = open_temp_storage("invalid-ordered-txs");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.land(10, &[(Address::ZERO, vec![0xaa])]);
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
            .expect("close frame");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        storage.land(20, &[(Address::ZERO, vec![0xbb])]);
        storage
            .close_frame_only(&mut head, 20, SafeInputRange::new(1, 2))
            .expect("close frame");

        let all = all_ordered_l2_txs(&mut storage);
        assert_eq!(all.len(), 2);

        storage
            .insert_invalid_batch(1)
            .expect("invalidate the suffix");

        let filtered = all_ordered_l2_txs(&mut storage);
        assert_eq!(filtered.len(), 1);
        match &filtered[0] {
            SequencedL2Tx::Direct(d) => assert_eq!(d.payload.as_slice(), &[0xaa]),
            _ => panic!("expected direct input"),
        }
    }

    #[test]
    fn invalid_batches_excluded_from_ordered_l2_txs_for_batch() {
        let (_db, mut storage) = open_temp_storage("invalid-ordered-for-batch");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.land(10, &[(Address::ZERO, vec![0xaa])]);
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
            .expect("close frame");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        let txs = storage.ordered_l2_txs_for_batch(0).expect("load batch 0");
        assert_eq!(txs.len(), 1);

        storage.insert_invalid_batch(0).expect("mark invalid");
        let txs = storage
            .ordered_l2_txs_for_batch(0)
            .expect("load batch 0 after invalidation");
        assert!(txs.is_empty(), "invalid batch should return no txs");
    }

    #[test]
    fn invalid_batches_excluded_from_drained_direct_count() {
        let (_db, mut storage) = open_temp_storage("invalid-drained-count");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.land(
            10,
            &[(Address::ZERO, vec![0xaa]), (Address::ZERO, vec![0xbb])],
        );
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, 2))
            .expect("close frame");
        assert_eq!(
            storage.next_undrained_safe_input_index().expect("cursor"),
            2
        );

        storage.insert_invalid_batch(0).expect("mark invalid");
        assert_eq!(
            storage
                .next_undrained_safe_input_index()
                .expect("cursor after invalidation"),
            0
        );
    }
}

mod recover_post_flush {
    use super::*;

    // ── recover_post_flush: cascade from first non-gold ────────────────
    //
    // These tests simulate the post-flush state directly: append safe-inputs
    // to drive `populate_safe_accepted_batches`, then call
    // `recover_post_flush()`. The cascade is unconditional (no threshold) —
    // any closed batch past the gold frontier is doomed and gets cascaded
    // along with everything after it.

    #[test]
    fn cascades_from_stale() {
        let (_db, mut storage) = open_temp_storage("detect-stale");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        for _ in 0..3 {
            storage.close_batch_at(&mut head, 10).expect("close batch");
        }

        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        let invalidated = storage
            .recover_post_flush(1200)
            .expect("detect and recover");
        assert_eq!(invalidated, vec![0, 1, 2, 3]);

        let head = storage.open_state().expect("load open state");
        assert!(head.is_some(), "recovery should have opened a fresh batch");
        assert_eq!(head.unwrap().batch_index, 4);
    }

    #[test]
    fn is_idempotent() {
        let (_db, mut storage) = open_temp_storage("detect-idempotent");
        let initial_history = storage.history_state().expect("initial history");
        assert_eq!(initial_history.version.recovery_generation.get(), 0);

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.close_batch_at(&mut head, 10).expect("close batch");

        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        let first = storage.recover_post_flush(1200).expect("first detect");
        assert_eq!(first, vec![0, 1]);
        assert_eq!(
            generation_cuts(&storage),
            [(1, 0)],
            "empty invalidated batches still record the old head"
        );
        assert_eq!(
            storage
                .history_state()
                .expect("history after recovery")
                .version
                .recovery_generation
                .get(),
            1,
            "one invalidating recovery advances exactly once"
        );
        assert_eq!(
            storage
                .history_state()
                .expect("history after recovery")
                .version
                .era_id,
            initial_history.version.era_id,
            "standard recovery stays within the same era"
        );

        let second = storage.recover_post_flush(1200).expect("second detect");
        assert!(second.is_empty());
        assert_eq!(
            generation_cuts(&storage),
            [(1, 0)],
            "a no-op must not add a cut"
        );
        assert_eq!(
            storage
                .history_state()
                .expect("history after no-op")
                .version
                .recovery_generation
                .get(),
            1,
            "a recovery no-op must not invent a new history reality"
        );
    }

    #[test]
    fn no_op_when_recovery_batch_landed_fresh_in_new_generation() {
        // Post-flush contract: `recover_post_flush` is called only after the
        // gold frontier has been re-synced. If the recovery batch from a
        // previous cycle landed on L1 fresh (within MAX_WAIT) and was
        // accepted by `populate_safe_accepted_batches`, the new generation
        // is gold and a fresh `recover_post_flush` call should be a no-op —
        // the stale ancestor's safe-input must not false-match the
        // nonce-reused gen-2 batch.
        let (_db, mut storage) = open_temp_storage("post-flush-no-op-after-fresh-gen2");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        // Gen 1 cascade. Stale batch 0 + open Tip 1 invalidated; recovery
        // batch 2 opened with nonce reused (= 0).
        let first = storage.recover_post_flush(1200).expect("gen1 recovery");
        assert_eq!(first, vec![0, 1]);
        assert_eq!(
            storage
                .history_state()
                .expect("generation one")
                .version
                .recovery_generation
                .get(),
            1
        );

        // Submitter posts the recovery batch; it lands fresh on L1.
        let mut head = storage.open_state().expect("load").unwrap();
        storage
            .close_batch_at(&mut head, 1300)
            .expect("close recovery batch");
        let landed = local_batch_payload(&mut storage, 0);
        storage.land(1310, &[(SENDER_A, landed)]); // append fresh safe input for recovery batch

        // populate_safe_accepted_batches accepts the gen-2 batch (nonce 0 at
        // first_frame=1300, inclusion=1310 → inclusion-staleness 10 < MAX_WAIT).
        // Gold advances. A fresh recover_post_flush is a no-op: the only
        // closed-non-gold candidate would have to be past the new frontier,
        // but everything is gold.
        let second = storage.recover_post_flush(1200).expect("post-flush no-op");
        assert!(
            second.is_empty(),
            "fresh gen-2 batch must be gold; old stale row must not false-match \
             via reused nonce, got: {second:?}"
        );
    }

    #[test]
    fn detects_stale_reused_nonce_in_new_generation() {
        let (_db, mut storage) = open_temp_storage("detect-reused-stale");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        // Append gen1 stale safe input.
        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        let first = storage.recover_post_flush(1200).expect("gen1 recovery");
        assert_eq!(first, vec![0, 1]);

        let mut head = storage.open_state().expect("load").unwrap();
        storage
            .close_batch_at(&mut head, 1210)
            .expect("close gen2 batch");

        // Append gen2 stale safe input.
        storage.land(2410, &[(SENDER_A, make_stale_batch_payload(0, 1210))]);
        let second = storage.recover_post_flush(1200).expect("gen2 recovery");
        assert_eq!(
            second,
            vec![2, 3],
            "stale reused nonce in gen2 must still be detected"
        );
        assert_eq!(
            storage
                .history_state()
                .expect("generation two")
                .version
                .recovery_generation
                .get(),
            2
        );
    }

    #[test]
    fn cascades_aging_tip_when_no_closed_pivot_exists() {
        // Regression for the no-pivot Tip case: the closed batch landed fresh
        // (becomes gold) but the Tip's `first_frame.safe_block` matches the
        // closed batch's, and after the flush wait the current safe block has
        // pushed the Tip's age into the danger zone. Pure monotonicity
        // (`S_tip >= S_closed`) doesn't rule this out — equality is allowed
        // when the lane rotates without a safe-block advance between frames.
        //
        // Earlier `recover_post_flush` only checked the first non-gold closed
        // batch; on no closed pivot it became a no-op, leaving the stale Tip
        // for the next danger trip to catch. Fix: fall through to a Tip
        // check against `danger_threshold` and cascade the Tip directly.
        let (_db, mut storage) = open_temp_storage("post-flush-tip-no-pivot");

        // S_closed = S_tip = 10 (lane rotated without a safe-block advance).
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0; open Tip with same safe_block=10");

        // Closed batch 0 landed fresh: inclusion_block=200 → inclusion-staleness 190
        // is well below MAX_WAIT, so populate accepts it as gold.
        // Then advance safe head to 1100: batch 0 age (gold) is irrelevant,
        // but Tip's age = 1100 - 10 = 1090 > danger_threshold (let's pass 1000).
        let landed = local_batch_payload(&mut storage, 0);
        storage.land(200, &[(SENDER_A, landed)]); // append fresh safe input — batch 0 becomes gold
        storage.advance_safe_head(1100); // advance safe head past Tip's danger threshold

        // Sanity: no closed batch is past gold (all gold).
        assert_eq!(
            find_closed_frontier_batch_in_danger(&storage.conn, 900).expect("strict check"),
            None,
            "all closed batches gold; closed-frontier check returns None"
        );

        // recover_post_flush(danger_threshold=1000): Tip's age (1090) > 1000.
        // Fall-through Tip check fires; Tip cascaded; recovery batch opened.
        let invalidated = storage
            .recover_post_flush(1000)
            .expect("recover_post_flush with danger_threshold=1000");
        assert_eq!(
            invalidated,
            vec![1],
            "Tip (batch 1) cascaded via danger_threshold fall-through"
        );

        // A fresh recovery Tip exists with a higher batch_index.
        let head = storage.open_state().expect("load").expect("recovery Tip");
        assert_eq!(head.batch_index, 2);
    }

    #[test]
    fn no_op_when_tip_age_below_danger_threshold() {
        // Negative companion to the above: closed batch is gold, Tip exists,
        // but Tip's age is below `danger_threshold`. Cascade must not fire.
        let (_db, mut storage) = open_temp_storage("post-flush-tip-below-danger");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        let landed = local_batch_payload(&mut storage, 0);
        storage.land(200, &[(SENDER_A, landed)]); // append fresh safe input — batch 0 becomes gold
        // current=500: Tip's age = 490, below threshold 1000.
        storage.advance_safe_head(500); // advance safe head, but not past danger threshold

        let invalidated = storage
            .recover_post_flush(1000)
            .expect("recover_post_flush with fresh Tip");
        assert!(
            invalidated.is_empty(),
            "Tip below danger_threshold must not be cascaded, got: {invalidated:?}"
        );

        // Tip preserved.
        let head = storage.open_state().expect("load").expect("Tip");
        assert_eq!(head.batch_index, 1);
    }
}

mod tip_staleness {
    use super::*;

    // ── Tip staleness — `recover_aging_tip` and post-flush combined cases ──
    //
    // `recover_aging_tip` backs the RecoverTip startup action: all closed
    // batches are outside the danger zone, but the open Tip has aged past the
    // configured threshold. The Tip has no L1 footprint (no `w_nonce`, no
    // `safe_input`), so it can be invalidated directly without a flush.
    //
    // The first four tests cover that path:
    //   - positive: Tip IS stale → invalidated
    //   - negative: Tip is fresh → NOT invalidated (no false positives)
    //   - boundary at threshold: invalidated
    //   - boundary just below threshold: not invalidated
    //
    // The remaining tests cover the guarded post-flush Cascade phase's combined
    // closed+open behavior (`recover_post_flush`'s `batch_index >= N`
    // cascade rule catches the Tip too).

    #[test]
    fn open_batch_stale_by_current_safe_block_is_invalidated() {
        // Scenario: sequencer opened batch 0 at safe_block=10, never closed it,
        // then stayed down until safe advanced to 1500 (>= the 1200-block threshold
        // past safe_block).
        // Recovery must invalidate the open batch.
        let (_db, mut storage) = open_temp_storage("open-batch-stale");

        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize open state at safe_block=10");

        // Advance the safe head so the open batch's first frame (safe_block=10)
        // is now stale: 1500 - 10 >= 1200.
        storage.advance_safe_head(1500); // advance safe head past the 1200-block danger threshold

        let invalidated = storage
            .recover_aging_tip(1200)
            .expect("recover from stale open batch");
        assert_eq!(
            invalidated,
            vec![0],
            "open batch 0 should be invalidated by current staleness"
        );
        assert_eq!(
            storage
                .history_state()
                .expect("history after Tip recovery")
                .version
                .recovery_generation
                .get(),
            1,
            "the direct RecoverTip path advances the shared generation boundary"
        );

        // A fresh recovery batch must be opened at batch_index=1.
        let head = storage.open_state().expect("load").expect("head");
        assert_eq!(head.batch_index, 1, "recovery batch is the next index");
    }

    #[test]
    fn open_batch_not_yet_stale_is_not_invalidated() {
        // Negative: open batch's first frame safe_block=10 with current safe=1100.
        // 1100 - 10 = 1090 < 1200. Must NOT cascade.
        // Catches false-positive regressions in `recover_aging_tip`.
        let (_db, mut storage) = open_temp_storage("open-batch-fresh");

        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize open state at safe_block=10");

        storage.advance_safe_head(1100); // advance safe head below threshold

        let invalidated = storage
            .recover_aging_tip(1200)
            .expect("recover with non-stale open batch");
        assert!(
            invalidated.is_empty(),
            "fresh open batch must not be cascade-invalidated, got: {invalidated:?}"
        );

        // The open batch must still be the live one (no recovery batch opened).
        let head = storage.open_state().expect("load").expect("head");
        assert_eq!(
            head.batch_index, 0,
            "original open batch 0 must still be the head"
        );
    }

    #[test]
    fn open_batch_exactly_at_threshold_is_invalidated() {
        // Boundary: 1210 - 10 = 1200, which is >= the danger_threshold argument (1200).
        // The staleness comparison is `>=`, so this must invalidate.
        let (_db, mut storage) = open_temp_storage("open-batch-boundary");

        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");

        storage.advance_safe_head(1210); // advance safe head to exact threshold

        let invalidated = storage.recover_aging_tip(1200).expect("recover");
        assert_eq!(invalidated, vec![0], "boundary (>= threshold) invalidates");
    }

    #[test]
    fn open_batch_one_block_below_threshold_is_not_invalidated() {
        // Boundary: 1209 - 10 = 1199 < 1200. One-block margin must NOT invalidate.
        let (_db, mut storage) = open_temp_storage("open-batch-below-boundary");

        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");

        storage.advance_safe_head(1209); // advance safe head to one block below threshold

        let invalidated = storage.recover_aging_tip(1200).expect("recover");
        assert!(
            invalidated.is_empty(),
            "one-block-below-threshold must not invalidate, got: {invalidated:?}"
        );
    }

    #[test]
    fn closed_unsubmitted_stale_and_open_stale_both_cascade() {
        // Scenario: batch 0 is closed and nonced but never submitted to L1
        // (safe_accepted_batches is empty). Batch 1 is open and also stale.
        // `find_first_batch_in_danger` should return closed batch 0 at the
        // frontier (nonce 0, no acceptance yet) and cascade through batch 1.
        let (_db, mut storage) = open_temp_storage("closed-unsubmitted-and-open-stale");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize at safe_block=10");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        // Advance safe head so batch 0's first frame (safe_block=10) is stale.
        storage.advance_safe_head(1500); // advance safe head past staleness

        let invalidated = storage.recover_post_flush(1200).expect("recover");
        assert_eq!(
            invalidated,
            vec![0, 1],
            "closed unsubmitted batch 0 and subsequent open batch 1 cascade together"
        );
    }

    #[test]
    fn opens_batch_after_torn_invalidation() {
        let (_db, mut storage) = open_temp_storage("detect-torn");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        storage.insert_invalid_batch(0).expect("invalidate 0");
        storage.insert_invalid_batch(1).expect("invalidate 1");
        storage.advance_safe_head(10); // record observed safe head

        let invalidated = storage
            .recover_post_flush(1200)
            .expect("recover from torn state");
        assert!(invalidated.is_empty(), "no new invalidations");
        assert_eq!(
            storage
                .history_state()
                .expect("history after structural repair")
                .version
                .recovery_generation
                .get(),
            0,
            "opening a missing Tip without invalidating history is not a recovery generation"
        );
        assert!(generation_cuts(&storage).is_empty());

        let head = storage.open_state().expect("load open state");
        assert!(head.is_some(), "recovery should have opened a fresh batch");
        assert_eq!(head.unwrap().batch_index, 2);
    }

    #[test]
    fn rolls_back_when_cascade_update_aborts() {
        let (db, mut storage) = open_temp_storage("detect-cascade-abort");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        // Advance safe head so batch 0's first frame (safe_block=10) is stale.
        storage.advance_safe_head(1500); // advance safe head past staleness

        storage
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_cascade_invalidation
                     AFTER UPDATE OF invalidated_at_ms ON batches
                     WHEN NEW.invalidated_at_ms IS NOT NULL
                      AND OLD.invalidated_at_ms IS NULL
                     BEGIN
                         SELECT RAISE(ABORT, 'injected cascade failure');
                     END;",
            )
            .expect("install failure trigger");

        let err = storage
            .recover_post_flush(1200)
            .expect_err("trigger should abort recovery transaction");
        assert!(
            err.to_string().contains("injected cascade failure"),
            "unexpected error: {err:?}"
        );
        drop(storage);

        let conn = Storage::open_connection(db.path.as_str()).expect("open read conn");
        let invalidated_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM batches WHERE invalidated_at_ms IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("count invalidated");
        assert_eq!(
            invalidated_count, 0,
            "failed cascade must not persist torn invalidation state"
        );

        let batch_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM batches", [], |row| row.get(0))
            .expect("count batches");
        assert_eq!(
            batch_count, 2,
            "failed recovery must not open an extra batch"
        );

        let open_batch_index: i64 = conn
            .query_row("SELECT batch_index FROM valid_open_batch", [], |row| {
                row.get(0)
            })
            .expect("query valid open batch");
        assert_eq!(
            open_batch_index, 1,
            "failed recovery must leave the original Tip in place"
        );
    }

    #[test]
    fn rolls_back_generation_and_invalidation_when_tip_reopen_aborts() {
        let (_db, mut storage) = open_temp_storage("detect-tip-reopen-abort");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.land(10, &[(Address::ZERO, vec![0xd1])]); // append mapped direct
        storage
            .close_frame_only_with_executions(
                &mut head,
                10,
                SafeInputRange::new(0, 1),
                &[DirectInputExecution {
                    safe_input_index: 0,
                    executed_input_offset: ExecutedInputCount::ZERO,
                }],
            )
            .expect("attribute mapped direct");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");
        assert_eq!(
            storage
                .next_executed_input_count()
                .expect("pre-recovery history head"),
            ExecutedInputCount::new(1)
        );
        storage.advance_safe_head(1500); // advance safe head past staleness

        storage
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_recovery_tip_insert
                     BEFORE INSERT ON batches
                     BEGIN
                         SELECT RAISE(ABORT, 'injected Tip reopen failure');
                     END;",
            )
            .expect("install failure trigger");

        let err = storage
            .recover_post_flush(1200)
            .expect_err("Tip reopen failure must abort the whole recovery transaction");
        assert!(
            err.to_string().contains("injected Tip reopen failure"),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            storage
                .history_state()
                .expect("history after rollback")
                .version
                .recovery_generation
                .get(),
            0,
            "the generation bump must roll back with the failed Tip reopen"
        );
        assert!(
            generation_cuts(&storage).is_empty(),
            "the pre-reopen cut must roll back too"
        );
        let invalidated_count: i64 = storage
            .conn
            .query_row(
                "SELECT COUNT(*) FROM batches WHERE invalidated_at_ms IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("count invalidated");
        assert_eq!(
            invalidated_count, 0,
            "the cascade must roll back with the generation bump"
        );
        let mappings: Vec<i64> = storage
            .conn
            .prepare("SELECT offset FROM application_inputs ORDER BY offset")
            .expect("prepare mapping query")
            .query_map([], |row| row.get(0))
            .expect("query mappings")
            .collect::<rusqlite::Result<_>>()
            .expect("collect mappings");
        assert_eq!(
            mappings,
            vec![0],
            "trigger-deleted mappings must roll back with the failed recovery"
        );
        assert_eq!(
            storage
                .next_executed_input_count()
                .expect("history head after rollback"),
            ExecutedInputCount::new(1),
            "the canonical history head must roll back with its mapping"
        );
        let open_batch_index: i64 = storage
            .conn
            .query_row("SELECT batch_index FROM valid_open_batch", [], |row| {
                row.get(0)
            })
            .expect("query original Tip");
        assert_eq!(open_batch_index, 1);
    }

    #[test]
    fn recovery_redrains_direct_inputs_and_replay_sees_them_once() {
        let (_db, mut storage) = open_temp_storage("recovery-redrain-e2e");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.land(
            10,
            &[(Address::ZERO, vec![0xd1]), (Address::ZERO, vec![0xd2])],
        ); // two deposits
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, 2))
            .expect("close frame with deposits");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        let before = all_ordered_l2_txs(&mut storage);
        assert_eq!(before.len(), 2, "both deposits should be visible");

        let batch_submitter = Address::repeat_byte(0xAA);
        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        let invalidated = storage
            .recover_post_flush(1200)
            .expect("detect and recover");
        assert!(!invalidated.is_empty(), "should have invalidated batches");
        assert_eq!(
            generation_cuts(&storage),
            [(1, 0)],
            "replacement directs must not enlarge the preserved prefix"
        );
        assert_eq!(
            storage.next_executed_input_count().unwrap(),
            ExecutedInputCount::new(2)
        );

        let after = all_ordered_l2_txs(&mut storage);
        let direct_payloads: Vec<&[u8]> = after
            .iter()
            .filter_map(|tx| match tx {
                SequencedL2Tx::Direct(d) if d.sender != batch_submitter => {
                    Some(d.payload.as_slice())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            direct_payloads,
            vec![&[0xd1][..], &[0xd2][..]],
            "deposits must appear exactly once in replay after recovery"
        );

        let recovery_batch = storage.open_state().expect("load").unwrap();
        let recovery_txs = storage
            .ordered_l2_txs_for_batch(recovery_batch.batch_index)
            .expect("load recovery batch txs");
        let recovery_direct_count = recovery_txs
            .iter()
            .filter(|tx| matches!(tx, SequencedL2Tx::Direct(d) if d.sender != batch_submitter))
            .count();
        assert_eq!(
            recovery_direct_count, 2,
            "both deposits should be in the recovery batch"
        );
    }

    #[test]
    fn undrained_safe_input_appears_in_recovery_batch_first_frame() {
        // a deposit ingested into safe_inputs but not yet drained
        // into any frame must be sequenced into the recovery batch's first
        // frame after cascade. Complements  (re-drain from
        // invalidated) with the never-drained case.
        let (_db, mut storage) = open_temp_storage("recovery-includes-undrained");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0 with no deposits");

        let non_submitter = Address::repeat_byte(0xCC);
        storage.land(20, &[(non_submitter, vec![0xde, 0xad])]); // append undrained deposit
        let before = all_ordered_l2_txs(&mut storage);
        assert!(
            before.iter().all(|tx| !matches!(
                tx,
                SequencedL2Tx::Direct(d) if d.sender == non_submitter
            )),
            "undrained deposit must not be sequenced before drain",
        );

        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        let invalidated = storage.recover_post_flush(1200).expect("recover");
        assert!(!invalidated.is_empty(), "stale batch must cascade");

        let recovery = storage.open_state().expect("load").unwrap();
        let recovery_txs = storage
            .ordered_l2_txs_for_batch(recovery.batch_index)
            .expect("load recovery batch txs");
        let deposit_payloads: Vec<&[u8]> = recovery_txs
            .iter()
            .filter_map(|tx| match tx {
                SequencedL2Tx::Direct(d) if d.sender == non_submitter => Some(d.payload.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(
            deposit_payloads,
            vec![&[0xde, 0xad][..]],
            "undrained deposit must land in the recovery batch's first frame",
        );
    }

    #[test]
    fn recovery_batch_opens_empty_when_no_direct_inputs_pending() {
        // no drained-into-invalidated inputs AND no undrained safe
        // inputs → recovery batch opens with an empty first frame (aside
        // from the batch-submitter's own self-submission, which is drained
        // but carries no user-visible payload).
        let (_db, mut storage) = open_temp_storage("recovery-empty-first-frame");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        let batch_submitter = SENDER_A;
        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        let invalidated = storage.recover_post_flush(1200).expect("recover");
        assert_eq!(invalidated, vec![0, 1]);

        let recovery = storage.open_state().expect("load").unwrap();
        let recovery_txs = storage
            .ordered_l2_txs_for_batch(recovery.batch_index)
            .expect("load recovery batch txs");
        let user_visible: Vec<_> = recovery_txs
            .iter()
            .filter(|tx| match tx {
                SequencedL2Tx::Direct(d) => d.sender != batch_submitter,
                SequencedL2Tx::UserOp(_) => true,
            })
            .collect();
        assert!(
            user_visible.is_empty(),
            "recovery batch must have no deposits or user-ops when none were pending: {user_visible:?}",
        );
    }

    #[test]
    fn first_batch_stale_recovery_reuses_nonce_zero() {
        // first-ever batch (nonce 0) goes stale before reaching
        // Gold. Cascade invalidates it; recovery opens a fresh batch that
        // reuses nonce 0 (no valid ancestor exists to advance the nonce).
        let (db, mut storage) = open_temp_storage("first-batch-stale-nonce-zero");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0 (nonce 0)");

        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        let invalidated = storage.recover_post_flush(1200).expect("recover");
        assert_eq!(
            invalidated,
            vec![0, 1],
            "closed batch 0 and open batch 1 must both invalidate",
        );

        let recovery = storage.open_state().expect("load").unwrap();
        assert_eq!(recovery.batch_index, 2, "batch_index is monotonic (PK)");
        drop(storage);

        // Read the new Tip's nonce and parent pointer via raw SQL — no
        // public accessor surfaces them.
        let conn = Storage::open_connection(db.path.as_str()).expect("open read conn");
        let recovery_i64 = recovery.batch_index as i64;
        let nonce: i64 = conn
            .query_row(
                "SELECT nonce FROM batches WHERE batch_index = ?1",
                [recovery_i64],
                |row| row.get(0),
            )
            .expect("query nonce");
        assert_eq!(
            nonce, 0,
            "recovery batch must reuse nonce 0 after torn cascade",
        );
        let parent: Option<i64> = conn
            .query_row(
                "SELECT parent_batch_index FROM batches WHERE batch_index = ?1",
                [recovery_i64],
                |row| row.get(0),
            )
            .expect("query parent");
        assert_eq!(
            parent, None,
            "torn recovery has no valid ancestor; parent_batch_index is NULL",
        );
    }

    #[test]
    fn after_post_recovery_crash_is_no_op() {
        // simulate a crash AFTER open_recovery_batch has run. On
        // restart, the state contains a valid open recovery batch (no stale
        // tail remains). A fresh `recover_post_flush` call must be a no-op:
        // no new invalidations, and the same recovery batch remains the Tip.
        //
        // Distinct from `is_idempotent` (idempotent back-to-back call on the
        // same Storage handle): this test drops and reopens Storage to model
        // a full restart over the persisted DB.
        let (db, mut storage) = open_temp_storage("post-recovery-crash-idempotent");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");

        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        // First call: full recovery runs to completion and opens a new Tip.
        let invalidated = storage.recover_post_flush(1200).expect("recover");
        assert_eq!(invalidated, vec![0, 1]);
        let recovery_index = storage
            .open_state()
            .expect("load open")
            .expect("recovery batch exists")
            .batch_index;

        // Simulate "crash immediately after open_recovery_batch" by
        // dropping Storage (mimics process exit) and reopening against the
        // same on-disk DB.
        drop(storage);
        let mut storage = Storage::open(db.path.as_str()).expect("reopen storage");

        let second = storage.recover_post_flush(1200).expect("second detect");
        assert!(
            second.is_empty(),
            "post-recovery restart must be a no-op, got invalidations: {second:?}",
        );
        let after = storage
            .open_state()
            .expect("load after restart")
            .expect("recovery batch still Tip after restart");
        assert_eq!(
            after.batch_index, recovery_index,
            "the same recovery batch must remain the Tip after restart",
        );
    }
}

mod check_danger_zone {
    use super::*;

    // ── check_danger_zone ──────────────────────────────────────────────

    #[test]
    fn check_danger_zone_ignores_old_gold_batches() {
        // Batch 0 is Gold (accepted, first_frame_safe_block=10). Batch 1 is
        // the open tip at first_frame_safe_block=100. Advance safe head to
        // 1200 so batch 0 is age=1190 > 1125 (past threshold, but it's Gold
        // and therefore excluded) while batch 1 is age=1100 < 1125 (fresh).
        //
        // `check_danger_zone` must return None: no unresolved batch is in
        // danger. Gold batches (accepted past the frontier) never participate,
        // and the open tip isn't old enough to trip the threshold.
        let (_db, mut storage) = open_temp_storage("danger-zone-gold");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 100)
            .expect("close batch 0");

        let landed = local_batch_payload(&mut storage, 0);
        storage.land(20, &[(SENDER_A, landed)]);
        // Advance to a current safe block where batch 0 (safe_block=10) is
        // past threshold (1200-10=1190>=1125) but batch 1 (safe_block=100)
        // is still fresh (1200-100=1100<1125).
        storage.advance_safe_head(1200); // advance safe block

        let result =
            find_closed_frontier_batch_in_danger(&storage.conn, 1125).expect("check danger zone");
        assert!(
            result.is_none(),
            "old Gold batches should not trigger danger zone; got batch_index={result:?}"
        );
    }

    #[test]
    fn check_danger_zone_does_not_flag_open_batch_zombie() {
        // `check_danger_zone` is for zombie detection: it must NOT flag the
        // open batch (which has no L1 tx to become a zombie). Flagging open
        // batches here would put the live submitter into a shutdown/restart
        // loop when an open batch ages into the danger zone without any
        // pending wallet-nonce slots to flush.
        //
        // Scenario: only an open batch exists, aged past the danger
        // threshold. `check_danger_zone` returns None.
        let (_db, mut storage) = open_temp_storage("danger-zone-open-no-zombie");

        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize open batch at safe_block=10");

        storage.advance_safe_head(1200); // advance safe head past danger threshold

        let result =
            find_closed_frontier_batch_in_danger(&storage.conn, 1125).expect("check danger zone");
        assert!(
            result.is_none(),
            "open batch (no zombie) must not trigger check_danger_zone; got batch_index={result:?}"
        );
    }
}

mod check_any_unresolved {
    use super::*;

    // ── check_any_unresolved_batch_in_danger ───────────────────────────────

    #[test]
    fn check_any_unresolved_flags_stale_open_batch() {
        // Wall-clock fallback regression: `check_any_unresolved_batch_in_danger`
        // MUST flag a stale open batch. This is the semantic the wall-clock
        // fallback relies on — if L1 is unreachable and an open batch may be
        // past the threshold, refuse to boot rather than accept user ops
        // into a batch that can't land.
        let (_db, mut storage) = open_temp_storage("any-unresolved-open-stale");

        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize open batch at safe_block=10");

        storage.advance_safe_head(1200); // advance safe head past threshold

        let result = find_first_batch_in_danger(&storage.conn, 1125)
            .expect("check any unresolved in danger");
        assert_eq!(
            result,
            Some(0),
            "stale open batch (batch 0) must be flagged by the unified check"
        );
    }

    #[test]
    fn check_any_unresolved_does_not_flag_fresh_open_batch() {
        // Negative counterpart. Fresh open batch below threshold must not
        // trigger false positives in the unified check.
        let (_db, mut storage) = open_temp_storage("any-unresolved-open-fresh");

        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize open batch at safe_block=10");

        storage.advance_safe_head(1100); // advance safe head below threshold

        let result = find_first_batch_in_danger(&storage.conn, 1125)
            .expect("check any unresolved in danger");
        assert!(
            result.is_none(),
            "fresh open batch must not trigger the unified check; got batch_index={result:?}"
        );
    }

    #[test]
    fn check_danger_zone_triggers_on_frontier_batch() {
        let (_db, mut storage) = open_temp_storage("danger-zone-frontier");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");
        storage
            .close_batch_at(&mut head, 100)
            .expect("close batch 1");

        let landed = local_batch_payload(&mut storage, 0);
        storage.land(20, &[(SENDER_A, landed)]);
        storage.advance_safe_head(1200); // advance safe block

        let result =
            find_closed_frontier_batch_in_danger(&storage.conn, 1125).expect("check danger zone");
        assert_eq!(result, Some(1), "frontier batch should trigger danger zone");
    }

    #[test]
    fn check_danger_zone_does_not_trigger_below_threshold() {
        let (_db, mut storage) = open_temp_storage("danger-zone-below");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");
        storage
            .close_batch_at(&mut head, 100)
            .expect("close batch 1");

        let landed = local_batch_payload(&mut storage, 0);
        storage.land(20, &[(SENDER_A, landed)]);
        storage.advance_safe_head(1134); // advance safe block

        let result =
            find_closed_frontier_batch_in_danger(&storage.conn, 1125).expect("check danger zone");
        assert!(
            result.is_none(),
            "should not trigger below threshold; got batch_index={result:?}"
        );
    }

    #[test]
    fn find_first_batch_in_danger_prefers_closed_frontier_over_aged_tip() {
        // When BOTH the closed frontier batch and the open Tip are aged past the
        // threshold, find_first_batch_in_danger must return the CLOSED frontier:
        // cascading from it covers the Tip (batch_index >= pivot), and the scoped
        // pending-snapshot clear keys on the pivot's nonce. Both existing
        // find_*_in_danger tests use open-batch-only scenarios; the closed-over-Tip
        // preference (the helper's whole point) was unasserted.
        let (_db, mut storage) = open_temp_storage("danger-prefers-closed-frontier");

        // batch 0 (frame sb 10) → made gold below; batch 1 (frame sb 50) → the
        // closed frontier; batch 2 (Tip, frame sb 50) → open at the same safe
        // block (the lane rotated without a safe-block advance).
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_batch_at(&mut head, 50)
            .expect("seal batch 0, open batch 1 @ sb 50");
        storage
            .close_batch_at(&mut head, 50)
            .expect("seal batch 1, open the Tip @ sb 50");

        // Make batch 0 gold: land its genuine bytes (accepted) → frontier → nonce 1.
        let landed = local_batch_payload(&mut storage, 0);
        storage.land(20, &[(SENDER_A, landed)]); // land batch 0
        assert_eq!(
            storage
                .submitter_frontier()
                .expect("frontier")
                .accepted_next_nonce,
            1,
            "batch 0 is gold; the closed frontier is now batch 1 (nonce 1)"
        );

        // Advance the safe head so both batch 1 and the Tip (both sb 50) are aged:
        // 1200 - 50 = 1150 >= threshold 1125.
        storage.advance_safe_head(1200);

        let result = find_first_batch_in_danger(&storage.conn, 1125).expect("find first in danger");
        assert_eq!(
            result,
            Some(1),
            "must return the closed frontier batch (1), not the aged Tip (2)"
        );
    }
}

mod boundary {
    use super::*;

    // ── boundary tests ─────────────────────────────────────────────────

    #[test]
    fn boundary_exactly_max_wait_is_stale() {
        let (_db, mut storage) = open_temp_storage("detect-boundary-exact");

        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.close_batch_at(&mut head, 100).expect("close batch");

        storage.land(1300, &[(SENDER_A, make_stale_batch_payload(0, 100))]);
        let invalidated = storage.recover_post_flush(1200).expect("detect");
        assert_eq!(invalidated, vec![0, 1], "exactly at max_wait must be stale");
        assert_eq!(storage.open_state().expect("load").unwrap().batch_index, 2);
    }

    #[test]
    fn boundary_one_below_max_wait_is_not_stale() {
        let (_db, mut storage) = open_temp_storage("detect-boundary-one-below");

        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.close_batch_at(&mut head, 100).expect("close batch");

        let landed = local_batch_payload(&mut storage, 0);
        storage.land(1299, &[(SENDER_A, landed)]);
        let invalidated = storage.recover_post_flush(1200).expect("detect");
        assert!(
            invalidated.is_empty(),
            "one below max_wait must not be stale"
        );
    }

    #[test]
    fn all_batches_invalidated_frontier_zero() {
        let (_db, mut storage) = open_temp_storage("detect-frontier-zero");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        for _ in 0..3 {
            storage.close_batch_at(&mut head, 10).expect("close");
        }

        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        let inv = storage.recover_post_flush(1200).expect("detect");
        assert_eq!(inv, vec![0, 1, 2, 3]);
        assert!(storage.open_state().expect("open").is_some());
    }

    #[test]
    fn recovery_batch_itself_becomes_stale() {
        let (_db, mut storage) = open_temp_storage("detect-recovery-stale");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.close_batch_at(&mut head, 10).expect("close");

        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]); // append gen1
        let inv1 = storage.recover_post_flush(1200).expect("recover gen1");
        assert_eq!(inv1, vec![0, 1]);

        let mut head2 = storage.open_state().expect("load").unwrap();
        storage
            .close_batch_at(&mut head2, 1210)
            .expect("close gen2");

        storage.land(2410, &[(SENDER_A, make_stale_batch_payload(0, 1210))]); // append gen2
        let inv2 = storage.recover_post_flush(1200).expect("recover gen2");
        assert_eq!(inv2, vec![2, 3]);
        assert!(storage.open_state().expect("open").is_some());
    }

    #[test]
    fn multi_round_gen3_recovery() {
        let (_db, mut storage) = open_temp_storage("detect-gen3");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_batch_at(&mut head, 10).expect("close");
        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        storage.recover_post_flush(1200).expect("recover gen1");

        let mut head2 = storage.open_state().expect("load").unwrap();
        storage
            .close_batch_at(&mut head2, 1210)
            .expect("close gen2");
        storage.land(2410, &[(SENDER_A, make_stale_batch_payload(0, 1210))]); // append gen2
        storage.recover_post_flush(1200).expect("recover gen2");

        let mut head3 = storage.open_state().expect("load").unwrap();
        storage
            .close_batch_at(&mut head3, 2410)
            .expect("close gen3");
        let landed = local_batch_payload(&mut storage, 0);
        storage.land(2420, &[(SENDER_A, landed)]); // append gen3
        let inv3 = storage.recover_post_flush(1200).expect("recover gen3");
        assert!(inv3.is_empty(), "gen3 should be healthy");
    }

    #[test]
    fn large_cascade_50_batches() {
        let (_db, mut storage) = open_temp_storage("detect-large-cascade");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        for _ in 0..50 {
            storage.close_batch_at(&mut head, 10).expect("close");
        }

        storage.land(1210, &[(SENDER_A, make_stale_batch_payload(0, 10))]);
        let inv = storage.recover_post_flush(1200).expect("detect");
        assert_eq!(inv.len(), 51);
    }
}

mod tree_invariants {
    use super::*;

    // ── Parent-pointer tree invariants ──────────────────────────────
    use crate::storage::convert::{i64_to_u64, u64_to_i64};
    use rusqlite::params;

    /// The schema triggers check parent links, the root and the single Tip on
    /// every write; invalidation is an UPDATE no trigger checks, so assert the
    /// valid spine here. The e2e harness asserts the same on the binary's DB.
    fn assert_valid_spine_from_anchor(storage: &mut Storage) {
        let anchor = u64_to_i64(storage.batch_tree_anchor().expect("anchor"));
        let nonces: Vec<i64> = storage
            .conn
            .prepare("SELECT nonce FROM valid_batches ORDER BY nonce ASC")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        let expected: Vec<i64> = (anchor..).take(nonces.len()).collect();
        assert_eq!(
            nonces, expected,
            "valid spine must be contiguous from anchor {anchor}"
        );
    }

    #[test]
    fn tree_invariants_hold_across_mixed_workload() {
        // Exercises every mutating code path: genesis, rotations, partial
        // cascades (ancestor survives), cascades across accepted frontier,
        // torn cascades (no valid ancestor), and back-to-back generations.
        // Every write is checked by the schema triggers (an ABORT fails its
        // expect); the valid spine is asserted after each step.
        let (_db, mut storage) = open_temp_storage("tree-invariants-workload");

        // Phase 1: genesis + 4 rotations. Simple chain.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        assert_valid_spine_from_anchor(&mut storage);
        for _ in 0..4 {
            storage.close_batch_at(&mut head, 100).expect("close");
            assert_valid_spine_from_anchor(&mut storage);
        }
        // Tree: 0(Gold sentinel in concept)→1→2→3→4 (Tip)

        // Phase 2: cascade with a valid ancestor. Batch 0 is accepted first.
        let landed = local_batch_payload(&mut storage, 0);
        storage.land(20, &[(SENDER_A, landed)]); // append accepted
        storage.advance_safe_head(1400); // advance past threshold
        let inv = storage.recover_post_flush(1200).expect("recover");
        assert!(!inv.is_empty(), "partial cascade should invalidate");
        assert_valid_spine_from_anchor(&mut storage);

        // Phase 3: more rotations after partial cascade.
        let mut head = storage.open_state().expect("load").unwrap();
        for _ in 0..3 {
            storage.close_batch_at(&mut head, 1500).expect("close gen2");
            assert_valid_spine_from_anchor(&mut storage);
        }

        // Phase 4: torn cascade — invalidate everything including batch 0.
        let latest = storage.latest_batch_index().expect("latest").unwrap();
        for bi in 0..=latest {
            storage.insert_invalid_batch(bi).expect("invalidate");
        }
        storage.recover_post_flush(1200).expect("recover from torn");
        assert_valid_spine_from_anchor(&mut storage);

        // Phase 5: rotations after torn cascade — new Tip has parent=NULL, nonce=0.
        let mut head = storage.open_state().expect("load").unwrap();
        for _ in 0..5 {
            storage.close_batch_at(&mut head, 2000).expect("close gen3");
            assert_valid_spine_from_anchor(&mut storage);
        }
    }

    #[test]
    fn subtree_by_batch_index_equals_subtree_by_parent_walk() {
        // cascade queries use `batch_index >= N` as a shortcut for
        // "subtree rooted at N". This test asserts the equivalence on a
        // realistic scenario with multiple cascade generations.
        let (_db, mut storage) = open_temp_storage("subtree-equivalence");

        // Build: 5 batches, cascade from 2 (partial), 3 more, cascade from 1 (torn-ish).
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        for _ in 0..4 {
            storage.close_batch_at(&mut head, 100).expect("close");
        }
        let landed = local_batch_payload(&mut storage, 0);
        storage.land(20, &[(SENDER_A, landed)]); // append accepted
        storage.advance_safe_head(1400); // advance
        let _ = storage.recover_post_flush(1200).expect("cascade 1");

        let mut head = storage.open_state().expect("load").unwrap();
        for _ in 0..2 {
            storage.close_batch_at(&mut head, 1500).expect("close");
        }

        // Assert equivalence among VALID batches for every valid N.
        // Restricting both sides to `valid_batches` is the invariant cascade
        // relies on: its WHERE filters invalidated rows, so the two sets need
        // only agree on the valid subset.
        let valid_bi: Vec<u64> = {
            let mut stmt = storage
                .conn
                .prepare("SELECT batch_index FROM valid_batches ORDER BY batch_index")
                .expect("prepare");
            stmt.query_map([], |row| row.get::<_, i64>(0).map(i64_to_u64))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("collect")
        };
        for &n in &valid_bi {
            let by_index: Vec<u64> = {
                let mut stmt = storage
                    .conn
                    .prepare(
                        "SELECT batch_index FROM valid_batches \
                         WHERE batch_index >= ?1 ORDER BY batch_index",
                    )
                    .expect("prepare");
                stmt.query_map(params![u64_to_i64(n)], |row| {
                    row.get::<_, i64>(0).map(i64_to_u64)
                })
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("collect")
            };
            let by_subtree: Vec<u64> = {
                let mut stmt = storage
                    .conn
                    .prepare(
                        "WITH RECURSIVE subtree(batch_index) AS ( \
                           SELECT batch_index FROM valid_batches WHERE batch_index = ?1 \
                           UNION ALL \
                           SELECT b.batch_index FROM valid_batches b \
                           JOIN subtree s ON b.parent_batch_index = s.batch_index \
                         ) \
                         SELECT batch_index FROM subtree ORDER BY batch_index",
                    )
                    .expect("prepare");
                stmt.query_map(params![u64_to_i64(n)], |row| {
                    row.get::<_, i64>(0).map(i64_to_u64)
                })
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("collect")
            };
            assert_eq!(
                by_index, by_subtree,
                "cascade root {n}: valid batch_index >= N diverged from valid parent-walk subtree"
            );
        }
    }
}

mod recovery_snapshot_selection {
    use super::*;
    use std::path::Path;

    fn baseline(storage: &mut Storage) -> i64 {
        storage
            .insert_baseline_snapshot(
                Path::new("/tmp/recovery-baseline"),
                ExecutedInputCount::ZERO,
            )
            .expect("baseline")
    }

    fn batch_snapshot(storage: &mut Storage, batch_index: u64) -> i64 {
        storage
            .insert_batch_snapshot(
                Path::new(&format!("/tmp/recovery-batch-{batch_index}")),
                batch_index,
            )
            .expect("batch snapshot")
    }

    #[test]
    fn cascade_hides_every_doomed_snapshot_and_retains_baseline() {
        let (_db, mut storage) = open_temp_storage("recovery-baseline-fallback");
        let baseline_id = baseline(&mut storage);
        seed_closed_batches(&mut storage, 3);
        for index in 0..3 {
            batch_snapshot(&mut storage, index);
        }
        assert_ne!(
            storage.latest_snapshot().unwrap().unwrap().dump.id,
            baseline_id
        );
        storage.advance_safe_head(1500); // safe head
        assert_eq!(
            storage.recover_post_flush(1200).expect("cascade"),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            storage.latest_snapshot().unwrap().unwrap().dump.id,
            baseline_id
        );
        assert!(
            storage
                .has_rollback_safe_snapshot()
                .expect("rollback checkpoint")
        );
        let garbage = storage
            .gc_unreferenced_dumps()
            .expect("gc doomed snapshots");
        assert_eq!(garbage.len(), 3);
        assert!(garbage.iter().all(|dump| dump.id != baseline_id));
    }

    #[test]
    fn accepted_snapshot_survives_cascade_after_baseline_gc_and_restart() {
        let (db, mut storage) = open_temp_storage("recovery-accepted-fallback");
        let baseline_id = baseline(&mut storage);
        seed_closed_batches(&mut storage, 2);
        let accepted_id = batch_snapshot(&mut storage, 0);
        let doomed_id = batch_snapshot(&mut storage, 1);
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 10, &[0]);
        let garbage = storage.gc_unreferenced_dumps().expect("retire baseline");
        assert_eq!(
            garbage.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![baseline_id]
        );
        assert_eq!(
            storage.latest_snapshot().unwrap().unwrap().dump.id,
            doomed_id
        );
        drop(storage);
        let mut storage = Storage::open(&db.path).expect("restart");
        assert!(
            storage
                .has_rollback_safe_snapshot()
                .expect("accepted checkpoint")
        );
        assert_eq!(
            storage.recover_post_flush(1200).expect("cascade suffix"),
            vec![1, 2]
        );
        assert_eq!(
            storage.latest_snapshot().unwrap().unwrap().dump.id,
            accepted_id
        );
        assert_eq!(
            storage.finalized_dump().unwrap().unwrap().dump.id,
            accepted_id
        );
        assert_eq!(
            storage
                .gc_unreferenced_dumps()
                .unwrap()
                .iter()
                .map(|d| d.id)
                .collect::<Vec<_>>(),
            vec![doomed_id]
        );
        assert!(
            storage
                .recover_post_flush(1200)
                .expect("idempotent recovery")
                .is_empty()
        );
    }

    #[test]
    fn an_optimistic_snapshot_alone_does_not_admit_recovery() {
        let (_db, mut storage) = open_temp_storage("recovery-optimistic-is-not-fallback");
        seed_closed_batches(&mut storage, 1);
        batch_snapshot(&mut storage, 0);
        let protocol = default_protocol_timing();
        let now = crate::clock::unix_now_ms();
        storage
            .append_safe_inputs_with_timestamp(
                1500,
                now / 1000,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("fresh observation");
        assert!(
            storage
                .latest_snapshot()
                .expect("optimistic snapshot")
                .is_some()
        );
        assert!(
            !storage
                .has_rollback_safe_snapshot()
                .expect("no stable fallback")
        );
        let err = storage
            .recover_post_flush_for_recovery(1500, &protocol, now)
            .expect_err("refuse before losing only snapshot");
        assert!(matches!(
            err,
            crate::storage::RecoveryMutationError::MissingRecoveryCheckpoint
        ));
        assert_eq!(
            storage
                .latest_snapshot()
                .unwrap()
                .unwrap()
                .executed_input_count,
            ExecutedInputCount::ZERO
        );
        assert_eq!(
            storage
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM batches WHERE invalidated_at_ms IS NOT NULL",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}

mod check_danger {
    use crate::clock::unix_now_ms;
    use crate::storage::test_helpers::{SENDER_A, default_protocol_timing, local_batch_payload};
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput};
    use crate::test_support::temp_db;

    #[test]
    fn check_danger_reports_observed_closed_batch_danger() {
        let db = temp_db("check-danger-observed-closed");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 1");

        let protocol = default_protocol_timing();
        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                1135,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 20,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append accepted batch 0");

        let status = storage
            .check_danger(&protocol, unix_now_ms())
            .expect("check_danger");
        assert_eq!(status, crate::storage::DangerStatus::ClosedBatchInDanger(1));
    }

    #[test]
    fn check_danger_reports_estimated_batch_danger_on_wall_clock_drift() {
        // Observed block-based checks wouldn't fire (batch 1 has
        // first_frame_safe_block = 100 and safe_block = 1200, age = 1100 <
        // 1125). But wall-clock says the safe head hasn't advanced in ~25
        // blocks: the effective threshold drops to 1100, so batch 1 reaches
        // danger via the wall-clock correction.
        let db = temp_db("check-danger-estimated");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 1");

        let protocol = default_protocol_timing();
        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 200,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append accepted batch 0");

        // Pretend safe-progress was recorded 25 blocks' worth of wall-clock ago.
        let now_ms = unix_now_ms();
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms.saturating_sub(25 * 12 * 1000)).unwrap_or(i64::MAX)],
            )
            .expect("rewind safe-progress timestamp");

        let status = storage
            .check_danger(&protocol, now_ms)
            .expect("check_danger");
        assert_eq!(
            status,
            crate::storage::DangerStatus::EstimatedBatchInDanger(1)
        );
    }

    #[test]
    fn check_danger_prefers_observed_closed_over_estimated_when_both_fire() {
        // If the observed safe head already puts the closed frontier past
        // `danger_threshold`, route to observed closed-batch recovery. The
        // wall-clock arm is a fallback, not a mask over observed danger.
        let db = temp_db("check-danger-observed-closed-wins");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        let protocol = default_protocol_timing();
        // Advance safe head so batch 0 is past `danger_threshold` (1125):
        // current=1200 - first_frame=10 = 1190 >= 1125.
        // No safe_input ingested for batch 0, so it stays non-gold.
        storage
            .append_safe_inputs(1200, &[], SENDER_A, &protocol)
            .expect("advance safe head past observed danger threshold");

        // Pretend safe-progress was recorded 25 blocks' worth of wall-clock
        // ago. With the wall-clock correction, the adjusted threshold is
        // 1100 — batch 0's age (1190) also clears that, so wall-clock fires.
        let now_ms = unix_now_ms();
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms.saturating_sub(25 * 12 * 1000)).unwrap_or(i64::MAX)],
            )
            .expect("rewind safe-progress timestamp");

        let status = storage
            .check_danger(&protocol, now_ms)
            .expect("check_danger");
        assert_eq!(
            status,
            crate::storage::DangerStatus::ClosedBatchInDanger(0),
            "observed closed-batch danger must win once safe-state has crossed danger",
        );
    }

    #[test]
    fn check_danger_prefers_observed_tip_over_estimated_when_both_fire() {
        // Same fallback rule for the open Tip: if observed safe-state already
        // puts the Tip in danger, recover the Tip directly instead of refusing
        // under the wall-clock classification.
        let db = temp_db("check-danger-observed-tip-wins");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0; batch 1 is Tip");

        let protocol = default_protocol_timing();
        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 20,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append accepted batch 0");

        // The Tip's observed age is 1190 >= 1125, and the wall-clock-adjusted
        // arm would also fire. Observed Tip danger must win.
        let now_ms = unix_now_ms();
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms.saturating_sub(25 * 12 * 1000)).unwrap_or(i64::MAX)],
            )
            .expect("rewind safe-progress timestamp");

        let status = storage
            .check_danger(&protocol, now_ms)
            .expect("check_danger");
        assert_eq!(
            status,
            crate::storage::DangerStatus::TipInDanger(1),
            "Tip must win when observed safe-state has already crossed danger",
        );
    }

    #[test]
    fn check_danger_refuses_when_l1_view_is_stale() {
        let db = temp_db("check-danger-l1-view-stale");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        let protocol = default_protocol_timing();
        let old_safe_timestamp = 1_000_u64;
        storage
            .append_safe_inputs_with_timestamp(
                1200,
                old_safe_timestamp,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("advance safe head with stale L1 timestamp");

        let now_ms =
            (old_safe_timestamp + protocol.l1_read_stale_after_secs()).saturating_mul(1000);
        let status = storage
            .check_danger(&protocol, now_ms)
            .expect("check_danger");
        assert_eq!(
            status,
            crate::storage::DangerStatus::L1ViewStale,
            "global L1 view staleness must refuse before observed batch recovery",
        );
    }

    #[test]
    fn check_danger_refuses_when_safe_block_timestamp_is_in_the_future() {
        let db = temp_db("check-danger-future-safe-timestamp");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_protocol_timing();
        let now_ms = 1_000_000_u64;

        // A full block-time (12 s) ahead of the local clock: unusable. Sub-block
        // ahead-ness is NTP-scale skew and tolerated (see the companion assert).
        storage
            .append_safe_inputs_with_timestamp(
                10,
                1_012,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("record future L1 timestamp");
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms).expect("test time fits")],
            )
            .expect("make local progress baseline usable");

        assert_eq!(
            storage
                .check_danger(&protocol, now_ms)
                .expect("check danger"),
            crate::storage::DangerStatus::L1ViewStale
        );

        // Sub-block skew (1 s ahead) is the freshest possible view, not a fault.
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET block_timestamp = 1001 WHERE singleton_id = 0",
                [],
            )
            .expect("set sub-block-ahead timestamp");
        assert_eq!(
            storage
                .check_danger(&protocol, now_ms)
                .expect("check danger"),
            crate::storage::DangerStatus::Safe
        );
    }

    #[test]
    fn check_danger_refuses_when_local_progress_timestamp_is_in_the_future() {
        let db = temp_db("check-danger-future-progress");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_protocol_timing();
        let now_ms = 1_000_000_u64;

        storage
            .append_safe_inputs_with_timestamp(
                10,
                1_000,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("record safe head");
        // One full block-time (12 s) of regression: the wall-clock
        // extrapolation is genuinely invalid — refuse. A sub-block step is
        // tolerated (companion assert below).
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms + 12_000).expect("test time fits")],
            )
            .expect("move local progress baseline into future");

        assert_eq!(
            storage
                .check_danger(&protocol, now_ms)
                .expect("check danger"),
            crate::storage::DangerStatus::L1ViewStale
        );

        // A 1 ms step is quantization noise for a block-granular estimate.
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms + 1).expect("test time fits")],
            )
            .expect("move baseline a sub-block step into the future");
        assert_eq!(
            storage
                .check_danger(&protocol, now_ms)
                .expect("check danger"),
            crate::storage::DangerStatus::Safe
        );
    }

    #[test]
    fn observed_batch_danger_outranks_regressed_progress_clock() {
        let db = temp_db("check-danger-future-progress-priority");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        let protocol = default_protocol_timing();
        let now_ms = 1_000_000_u64;

        storage
            .append_safe_inputs_with_timestamp(
                1200,
                1_000,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("advance observed safe block past danger");
        // Even a full-block clock regression (a real fault, not noise) must
        // not suppress the observed arm: batch age here is pure block
        // arithmetic on persisted L1 observations, valid regardless of the
        // local clock. The refusal only wins when no observed danger stands
        // (2026-07-31 review of the containment commit).
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms + 12_000).expect("test time fits")],
            )
            .expect("move local progress baseline into future");

        assert_eq!(
            storage
                .check_danger(&protocol, now_ms)
                .expect("check danger"),
            crate::storage::DangerStatus::ClosedBatchInDanger(0),
            "observed danger stands on L1 observation alone; a wall-clock \
             fault must not delay recovery of a batch aging toward staleness"
        );

        // Same verdict with BOTH baselines faulted: the safe-block timestamp
        // a full block ahead of the clock AND the progress baseline regressed.
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET block_timestamp = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms / 1000 + 12).expect("test time fits")],
            )
            .expect("move safe-block timestamp a block into the future");
        assert_eq!(
            storage
                .check_danger(&protocol, now_ms)
                .expect("check danger"),
            crate::storage::DangerStatus::ClosedBatchInDanger(0),
            "both clock-fault baselines together still yield to observed danger"
        );
    }

    #[test]
    fn check_danger_safe_when_never_synced() {
        // Fresh DB, no prior safe block timestamp. The L1 view is unusable
        // until the input reader records a real safe-head observation.
        let db = temp_db("check-danger-never-synced");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let status = storage
            .check_danger(&default_protocol_timing(), unix_now_ms())
            .expect("check_danger");
        assert_eq!(status, crate::storage::DangerStatus::L1ViewStale);
    }

    #[test]
    fn check_danger_errors_when_valid_tip_has_no_first_frame() {
        let db = temp_db("check-danger-missing-first-frame");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        let protocol = default_protocol_timing();
        storage
            .append_safe_inputs(10, &[], SENDER_A, &protocol)
            .expect("record fresh safe head");
        storage
            .conn
            .execute("DELETE FROM frames WHERE batch_index = 0", [])
            .expect("inject missing-frame corruption");

        let err = storage
            .check_danger(&protocol, unix_now_ms())
            .expect_err("a valid batch without a first frame must fail loud");
        assert!(matches!(err, rusqlite::Error::QueryReturnedNoRows));
    }
}
