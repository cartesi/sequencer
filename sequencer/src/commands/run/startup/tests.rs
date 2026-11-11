// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::VecDeque;

use super::*;
use crate::commands::error::{EXIT_RESTART_TRANSIENT, EXIT_TERMINAL};
use crate::storage::test_helpers::{
    LandingFixture, SENDER_A, open_temp_storage, record_canonical_divergence,
};
use crate::storage::{SafeInputRange, StoredSafeInput, TerminalFact};
use crate::test_support::TestDb;

/// The window the test identity pins (the wallet's).
const W: u64 = app_core::application::WALLET_SCHEDULER_LIMITS.max_wait_blocks;

enum SyncStep {
    /// L1 shows nothing new.
    Keep,
    Fail(InputReaderError),
    /// L1's safe head moved to `block`, carrying `inputs`.
    Advance {
        block: u64,
        inputs: Vec<StoredSafeInput>,
    },
}

/// The substituted L1: each sync takes the next step, and the real storage
/// append records what it shows.
struct TestL1 {
    db_path: String,
    steps: VecDeque<SyncStep>,
    syncs: usize,
}

impl TestL1 {
    fn new(db: &TestDb, steps: Vec<SyncStep>) -> Self {
        Self {
            db_path: db.path.clone(),
            steps: steps.into(),
            syncs: 0,
        }
    }
}

impl StartupSync for TestL1 {
    async fn sync(&mut self) -> Result<(), InputReaderError> {
        self.syncs += 1;
        match self.steps.pop_front().expect("unexpected L1 sync") {
            SyncStep::Keep => Ok(()),
            SyncStep::Fail(error) => Err(error),
            SyncStep::Advance { block, inputs } => Storage::open_writer(&self.db_path)
                .expect("open writer")
                .append_safe_inputs(block, &inputs, SENDER_A)
                .map_err(InputReaderError::Storage),
        }
    }
}

/// What setup leaves: the era, the identity, a safe head at 50, no batch.
fn after_setup(name: &str) -> TestDb {
    let (db, mut storage) = open_temp_storage(name);
    storage.advance_safe_head(50);
    db
}

/// The batches as `(batch_index, nonce, sealed, invalidated)`.
fn batch_rows(db: &TestDb) -> Vec<(i64, i64, bool, bool)> {
    Storage::open(&db.path)
        .expect("open storage")
        .raw_read(|tx| {
            tx.prepare(
                "SELECT batch_index, nonce, sealed_at_ms IS NOT NULL, \
                 invalidated_at_ms IS NOT NULL FROM batches ORDER BY batch_index",
            )?
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect()
        })
        .expect("read batches")
}

#[tokio::test]
async fn the_first_start_of_an_era_opens_its_first_batch_once() {
    let db = after_setup("startup-first-batch");
    let mut l1 = TestL1::new(
        &db,
        vec![SyncStep::Advance {
            block: 60,
            inputs: vec![],
        }],
    );
    start(&db.path, &mut l1).await.expect("start");
    assert_eq!(l1.syncs, 1);
    assert_eq!(batch_rows(&db), vec![(0, 0, false, false)]);
    let head = Storage::open(&db.path)
        .unwrap()
        .open_state()
        .unwrap()
        .expect("the era's first batch");
    assert_eq!(head.safe_block(), 60, "opened at the synced safe head");

    // A restart finds it open and opens nothing.
    let mut l1 = TestL1::new(&db, vec![SyncStep::Keep]);
    start(&db.path, &mut l1).await.expect("restart");
    assert_eq!(batch_rows(&db), vec![(0, 0, false, false)]);
}

/// A start inside the window repairs nothing: the same batches, the same
/// nonces, the same open batch, nothing invalidated. That holds with the
/// provider down and the stored view hours old.
#[tokio::test]
async fn a_start_inside_the_window_changes_nothing() {
    let db = after_setup("startup-inside-window");
    {
        let mut storage = Storage::open(&db.path).unwrap();
        let mut head = storage
            .initialize_open_state(50, SafeInputRange::empty_at(0))
            .unwrap();
        storage.close_batch_at(&mut head, 50).unwrap();
        storage.close_batch_at(&mut head, 60).unwrap();
        // The safe block is stamped at the epoch: a view decades old.
        storage
            .append_safe_inputs_with_timestamp(50 + W - 2, 0, &[], SENDER_A)
            .unwrap();
    }
    let before = batch_rows(&db);
    assert_eq!(
        before,
        vec![
            (0, 0, true, false),
            (1, 1, true, false),
            (2, 2, false, false)
        ]
    );

    for step in [
        SyncStep::Keep,
        SyncStep::Fail(InputReaderError::Provider("offline".into())),
    ] {
        let mut l1 = TestL1::new(&db, vec![step]);
        start(&db.path, &mut l1)
            .await
            .expect("a start inside the window");
        assert_eq!(batch_rows(&db), before);
    }
}

/// The sync can show a void history the stored view did not: startup refuses
/// before opening or changing anything.
#[tokio::test]
async fn a_void_history_shown_by_the_sync_refuses() {
    let db = after_setup("startup-void-after-sync");
    {
        let mut storage = Storage::open(&db.path).unwrap();
        let mut head = storage
            .initialize_open_state(50, SafeInputRange::empty_at(0))
            .unwrap();
        storage.close_batch_at(&mut head, 50).unwrap();
    }
    let before = batch_rows(&db);

    let mut l1 = TestL1::new(
        &db,
        vec![SyncStep::Advance {
            block: 50 + W - 1,
            inputs: vec![],
        }],
    );
    let error = start(&db.path, &mut l1)
        .await
        .expect_err("a void history refuses");
    assert!(
        matches!(
            &error,
            CommandError::Bootstrap(BootstrapError::TerminalFact(TerminalFact::VoidHistory {
                lag,
                ..
            })) if lag.first_frame_safe_block == 50
        ),
        "{error:?}"
    );
    assert_eq!(error.exit_code(), EXIT_TERMINAL);
    assert_eq!(batch_rows(&db), before);
    // Every later start refuses before L1.
    assert!(matches!(
        refuse_terminal_fact(&db.path),
        Err(CommandError::Bootstrap(BootstrapError::TerminalFact(
            TerminalFact::VoidHistory { .. }
        )))
    ));
}

/// A foreign batch accepted at the era's first nonce, seen by the sync,
/// refuses before the era's first batch is opened.
#[tokio::test]
async fn a_divergence_shown_by_the_sync_refuses_before_opening() {
    let db = after_setup("startup-divergence-after-sync");
    let foreign = StoredSafeInput {
        sender: SENDER_A,
        payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![],
        }),
        block_number: 55,
    };
    let mut l1 = TestL1::new(
        &db,
        vec![SyncStep::Advance {
            block: 60,
            inputs: vec![foreign],
        }],
    );
    let error = start(&db.path, &mut l1)
        .await
        .expect_err("a divergence refuses");
    assert!(
        matches!(
            &error,
            CommandError::Bootstrap(BootstrapError::TerminalFact(TerminalFact::Divergence(
                divergence
            ))) if divergence.nonce == 0
        ),
        "{error:?}"
    );
    assert_eq!(error.exit_code(), EXIT_TERMINAL);
    assert!(batch_rows(&db).is_empty(), "no batch was opened");
}

#[test]
fn a_recorded_divergence_refuses_before_l1() {
    let db = after_setup("startup-recorded-divergence");
    record_canonical_divergence(&mut Storage::open(&db.path).unwrap(), 7, 0);
    let error = refuse_terminal_fact(&db.path).expect_err("a divergence refuses");
    assert!(matches!(
        error,
        CommandError::Bootstrap(BootstrapError::TerminalFact(TerminalFact::Divergence(_)))
    ));
    assert_eq!(error.exit_code(), EXIT_TERMINAL);
}

/// A void history refuses before any L1 call: the stub has no scripted
/// sync and panics if called.
#[tokio::test]
async fn a_void_history_refuses_before_any_l1_call() {
    let db = after_setup("startup-void-before-l1");
    {
        let mut storage = Storage::open(&db.path).unwrap();
        storage
            .initialize_open_state(50, SafeInputRange::empty_at(0))
            .unwrap();
        storage.advance_safe_head(50 + W - 1);
    }
    let before = batch_rows(&db);
    let mut l1 = TestL1::new(&db, vec![]);
    let error = start(&db.path, &mut l1)
        .await
        .expect_err("a void history refuses");
    assert!(
        matches!(
            error,
            CommandError::Bootstrap(BootstrapError::TerminalFact(
                TerminalFact::VoidHistory { .. }
            ))
        ),
        "{error:?}"
    );
    assert_eq!(l1.syncs, 0);
    assert_eq!(batch_rows(&db), before);
}

/// The first start of an era with the provider down opens no batch on the
/// stored view, whose safe block would fix the batch's first frame for good:
/// it refuses as retryable.
#[tokio::test]
async fn the_first_start_with_the_provider_down_opens_nothing() {
    let db = after_setup("startup-first-start-provider-down");
    let mut l1 = TestL1::new(
        &db,
        vec![SyncStep::Fail(InputReaderError::Provider("offline".into()))],
    );
    let error = start(&db.path, &mut l1)
        .await
        .expect_err("no batch is opened on the stored view");
    assert!(
        matches!(
            error,
            CommandError::Bootstrap(BootstrapError::StartupSync(InputReaderError::Provider(_)))
        ),
        "{error:?}"
    );
    assert_eq!(error.exit_code(), EXIT_RESTART_TRANSIENT);
    assert!(batch_rows(&db).is_empty());
}

/// Every sync failure other than an unreachable provider keeps its startup
/// class and opens nothing.
#[tokio::test]
async fn other_sync_failures_keep_their_startup_class() {
    for (error, code) in [
        (
            InputReaderError::InconsistentL1Response("gap".into()),
            EXIT_RESTART_TRANSIENT,
        ),
        (
            InputReaderError::ChainIdMismatch {
                rpc: 1,
                expected: 2,
            },
            EXIT_TERMINAL,
        ),
        (InputReaderError::Bootstrap("bad url".into()), EXIT_TERMINAL),
    ] {
        let db = after_setup("startup-sync-failure");
        let description = error.to_string();
        let mut l1 = TestL1::new(&db, vec![SyncStep::Fail(error)]);
        let refused = start(&db.path, &mut l1)
            .await
            .expect_err("a non-provider sync failure refuses");
        assert!(
            matches!(
                refused,
                CommandError::Bootstrap(BootstrapError::StartupSync(_))
            ),
            "{description}: {refused:?}"
        );
        assert_eq!(refused.exit_code(), code, "{description}");
        assert!(batch_rows(&db).is_empty(), "{description}");
    }
}
