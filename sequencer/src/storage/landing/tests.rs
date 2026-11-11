// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use super::super::test_helpers::{
    LandingFixture, SENDER_A, local_batch_payload, open_temp_storage, record_canonical_divergence,
    set_up_storage_with_window,
};
use super::*;
use crate::storage::{SafeInputRange, Storage};

/// The window the test identity pins (the wallet's).
const W: u64 = app_core::application::WALLET_SCHEDULER_LIMITS.max_wait_blocks;

fn void_at(storage: &mut Storage, safe_block: u64) -> Option<TerminalFact> {
    storage.advance_safe_head(safe_block);
    storage.terminal_fact().expect("terminal fact")
}

/// The batches as `(batch_index, nonce, sealed)`.
fn batch_rows(storage: &mut Storage) -> Vec<(i64, i64, bool)> {
    storage
        .read(|tx| {
            tx.prepare(
                "SELECT batch_index, nonce, sealed_at_ms IS NOT NULL \
                 FROM batches ORDER BY batch_index",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect()
        })
        .expect("read batches")
}

mod void_history {
    use super::*;

    #[test]
    fn nothing_is_void_before_the_first_sync() {
        let (_db, mut storage) = open_temp_storage("landing-before-sync");
        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("open batch");
        assert_eq!(storage.landing_lag().expect("lag"), None);
        assert_eq!(storage.terminal_fact().expect("terminal fact"), None);
    }

    #[test]
    fn nothing_is_void_without_a_batch() {
        let (_db, mut storage) = open_temp_storage("landing-no-batch");
        assert_eq!(void_at(&mut storage, 10 * W), None);
        assert_eq!(storage.landing_lag().expect("lag"), None);
    }

    /// `H + 1 - s == W` is void: block `H + 1` is the first a landing can
    /// use, and the scheduler skips a batch `W` blocks past its first frame.
    /// One block less is not.
    #[test]
    fn the_exact_boundary_is_void_and_one_block_less_is_not() {
        let (_db, mut storage) = open_temp_storage("landing-boundary");
        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("open batch 0");
        storage
            .close_batch_at(&mut head, 100)
            .expect("close batch 0");

        let last_live = 100 + W - 2;
        assert_eq!(void_at(&mut storage, last_live), None);
        let lag = storage
            .landing_lag()
            .expect("lag")
            .expect("an unaccepted batch");
        assert_eq!(lag.batch, BatchId::new(0));
        assert_eq!(lag.blocks(), W - 1);
        assert!(!lag.is_void(W));

        let fact = void_at(&mut storage, last_live + 1).expect("void at the boundary");
        let lag = LandingLag {
            batch: BatchId::new(0),
            first_frame_safe_block: 100,
            safe_block: 100 + W - 1,
        };
        assert_eq!(
            fact,
            TerminalFact::VoidHistory {
                lag,
                max_wait_blocks: W,
            }
        );
        assert!(lag.is_void(W));
        assert_eq!(
            lag.blocks(),
            W,
            "void exactly when the lag reaches the window"
        );
        assert!(!lag.is_void(W + 1), "the window decides the boundary");
    }

    /// The window is the pinned identity's, not the wallet's: under a pinned
    /// window of 50, the lag is void at 50 and not at 49.
    #[test]
    fn the_boundary_follows_the_pinned_window() {
        let db = crate::test_support::temp_db("landing-pinned-window");
        let mut storage = set_up_storage_with_window(&db.path, SENDER_A, 50);
        storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("open batch 0");
        assert_eq!(void_at(&mut storage, 100 + 48), None);
        assert_eq!(
            storage.landing_lag().expect("lag").map(|lag| lag.blocks()),
            Some(49)
        );
        assert!(matches!(
            void_at(&mut storage, 100 + 49),
            Some(TerminalFact::VoidHistory {
                lag,
                max_wait_blocks: 50,
            }) if lag.blocks() == 50
        ));
    }

    /// The open batch has no L1 footprint, but its first frame fixes `s` all
    /// the same: sealed late, it lands stale.
    #[test]
    fn the_open_batch_counts() {
        let (_db, mut storage) = open_temp_storage("landing-open-batch");
        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("open batch 0");
        assert_eq!(void_at(&mut storage, 10 + W - 2), None);
        assert_eq!(
            storage.landing_lag().expect("lag"),
            Some(LandingLag {
                batch: BatchId::new(0),
                first_frame_safe_block: 10,
                safe_block: 10 + W - 2,
            })
        );
        assert!(matches!(
            void_at(&mut storage, 10 + W - 1),
            Some(TerminalFact::VoidHistory { lag, .. }) if lag.batch == BatchId::new(0)
        ));
    }

    /// An accepted batch is judged no more, however old its first frame: the
    /// open batch after it is the oldest unaccepted one.
    #[test]
    fn an_accepted_batch_never_makes_the_history_void() {
        let (_db, mut storage) = open_temp_storage("landing-accepted");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("open batch 0");
        // Batch 0 opens at 10; batch 1 opens at 1000.
        storage
            .close_batch_at(&mut head, 1000)
            .expect("close batch 0");
        let landed = local_batch_payload(&mut storage, 0);
        storage.land(1100, &[(SENDER_A, landed)]);
        assert_eq!(
            storage
                .submitter_frontier()
                .expect("frontier")
                .accepted_next_nonce,
            1,
            "batch 0 is accepted"
        );

        // Far past batch 0's window, inside batch 1's.
        let safe_block = 1000 + W - 2;
        assert!(
            age_exceeds(safe_block + 1, 10, W),
            "batch 0 would be void were it unaccepted"
        );
        assert_eq!(void_at(&mut storage, safe_block), None);
        assert_eq!(
            storage.landing_lag().expect("lag"),
            Some(LandingLag {
                batch: BatchId::new(1),
                first_frame_safe_block: 1000,
                safe_block,
            })
        );
    }

    /// The batch at the accepted frontier's next nonce is judged: an old
    /// unaccepted closed batch voids the history while younger batches,
    /// the open one included, could still land on their own.
    #[test]
    fn the_oldest_unaccepted_batch_is_the_one_judged() {
        let (_db, mut storage) = open_temp_storage("landing-oldest");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("open batch 0");
        // Batch 0 and batch 1 open at 10; the open batch 2 opens at 500.
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");
        storage
            .close_batch_at(&mut head, 500)
            .expect("close batch 1");
        assert_eq!(head.batch_index(), BatchId::new(2));

        assert_eq!(void_at(&mut storage, 20), None);
        assert_eq!(
            storage.landing_lag().expect("lag").map(|lag| lag.batch),
            Some(BatchId::new(0))
        );

        // Accepting batch 0 moves the judgement to batch 1, not to the open
        // batch.
        let landed = local_batch_payload(&mut storage, 0);
        storage.land(30, &[(SENDER_A, landed)]);
        assert_eq!(
            storage.landing_lag().expect("lag").map(|lag| lag.batch),
            Some(BatchId::new(1))
        );
        assert!(matches!(
            void_at(&mut storage, 10 + W - 1),
            Some(TerminalFact::VoidHistory { lag, .. })
                if lag.batch == BatchId::new(1) && lag.first_frame_safe_block == 10
        ));
    }

    /// A divergence outranks a void history: it is the first terminal fact.
    #[test]
    fn a_divergence_is_reported_before_a_void_history() {
        let (_db, mut storage) = open_temp_storage("landing-divergence-first");
        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("open batch 0");
        assert!(void_at(&mut storage, 10 + W).is_some());
        record_canonical_divergence(&mut storage, 7, 3);
        assert_eq!(
            storage.terminal_fact().expect("terminal fact"),
            Some(TerminalFact::Divergence(Divergence {
                nonce: 7,
                safe_input_index: 3,
            }))
        );
    }

    #[test]
    fn a_batch_without_a_first_frame_fails_loud() {
        let (_db, mut storage) = open_temp_storage("landing-missing-first-frame");
        storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("open batch 0");
        storage.advance_safe_head(10);
        storage
            .conn
            .execute_batch(
                "DROP TRIGGER trg_frames_not_deletable; \
                 DELETE FROM frames WHERE batch_index = 0;",
            )
            .expect("inject missing-frame corruption");
        let err = storage
            .landing_lag()
            .expect_err("a batch without a first frame must fail loud");
        assert!(matches!(err, rusqlite::Error::QueryReturnedNoRows));
    }
}

mod open_batch_if_none {
    use super::*;

    #[test]
    fn opens_one_batch_at_the_safe_head_and_only_once() {
        let (_db, mut storage) = open_temp_storage("open-batch-once");
        storage.advance_safe_head(42);
        assert!(storage.open_batch_if_none().expect("open"));
        let head = storage.open_state().expect("load").expect("an open batch");
        assert_eq!(head.batch_index(), BatchId::new(0));
        assert_eq!(head.safe_block(), 42);
        let opened = batch_rows(&mut storage);
        assert_eq!(opened, vec![(0, 0, false)]);

        assert!(!storage.open_batch_if_none().expect("no-op"));
        assert_eq!(batch_rows(&mut storage), opened);
    }

    #[test]
    fn refuses_a_divergence_without_mutation() {
        let (_db, mut storage) = open_temp_storage("open-batch-divergence");
        storage.advance_safe_head(42);
        record_canonical_divergence(&mut storage, 7, 0);
        let error = storage
            .open_batch_if_none()
            .expect_err("a divergence refuses the opening");
        assert!(matches!(
            error,
            OpenBatchError::Terminal(TerminalFact::Divergence(Divergence { nonce: 7, .. }))
        ));
        assert!(batch_rows(&mut storage).is_empty());
    }

    #[test]
    fn refuses_a_void_history_without_mutation() {
        let (_db, mut storage) = open_temp_storage("open-batch-void");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("open batch 0");
        storage
            .close_batch_at(&mut head, 10)
            .expect("close batch 0");
        storage.advance_safe_head(10 + W - 1);
        let before = batch_rows(&mut storage);
        let error = storage
            .open_batch_if_none()
            .expect_err("a void history refuses the opening");
        assert!(matches!(
            error,
            OpenBatchError::Terminal(TerminalFact::VoidHistory { lag, .. })
                if lag.batch == BatchId::new(0)
        ));
        assert_eq!(batch_rows(&mut storage), before);
    }

    #[test]
    fn refuses_and_rolls_back_if_its_transaction_leaves_no_open_batch() {
        let (db, mut storage) = open_temp_storage("open-batch-postcondition");
        storage.advance_safe_head(42);
        drop(storage);
        // Fault injection: seal the new batch the moment its first frame
        // lands, so `open_fresh_tip_in_tx` returns `Ok` having left no open
        // batch. (`open.rs` documents failure triggers as the sanctioned way
        // past the typed API in tests.)
        Storage::open_connection(db.path.as_str())
            .expect("raw connection")
            .execute_batch(
                "CREATE TRIGGER test_seal_new_tip AFTER INSERT ON frames \
                 BEGIN \
                   UPDATE batches SET sealed_at_ms = 1, payload_hash = zeroblob(32) \
                   WHERE batch_index = NEW.batch_index; \
                 END",
            )
            .expect("install failure trigger");
        let mut storage = Storage::open(db.path.as_str()).expect("reopen");

        let error = storage
            .open_batch_if_none()
            .expect_err("an opening that leaves no batch must refuse, not commit");
        assert!(
            matches!(error, OpenBatchError::OpenBatchMissing),
            "expected the postcondition refusal, got {error:?}"
        );
        assert!(
            batch_rows(&mut storage).is_empty(),
            "the refused opening must roll back its insert"
        );
    }
}
