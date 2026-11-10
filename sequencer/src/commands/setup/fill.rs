// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Step 7 of setup: write the baseline dump of the application at the end of
//! the stop block, then publish the era with it in one transaction. Setup
//! opens no root frame; `run`'s startup does.

use std::path::{Path, PathBuf};

use sequencer_core::application::Application;
use sequencer_core::history::L2TxCount;
use sequencer_core::scheduler::SchedulerState;

use crate::commands::error::CommandError;
use crate::ingress::inclusion_lane::dump_info::{
    self, CreateDumpDirError, create_dump_dir_with_info,
};
use crate::storage::{EraRecord, Storage};

/// Publish the era: `app` has executed the whole rebuilt history, and
/// `scheduler` is the scheduler at the end of the stop block. Every rebuilt
/// row must already carry its count: publishing seals the table.
pub(crate) fn publish<A: Application>(
    app: &mut A,
    storage: &mut Storage,
    dumps_dir: &Path,
    base_count: L2TxCount,
    stop_block: u64,
    scheduler: SchedulerState,
) -> Result<EraRecord, CommandError> {
    let era = EraRecord {
        base_count,
        rebuilt_history_end: app.l2_tx_count(),
        stop_block,
        first_queued_input: scheduler.first_queued_input,
        next_batch_nonce: scheduler.next_batch_nonce,
    };
    let prefix = write_baseline_dump(app, era.next_batch_nonce, dumps_dir)?;
    storage.publish_era(&prefix, &era)?;
    Ok(era)
}

fn write_baseline_dump<A: Application>(
    app: &mut A,
    next_batch_nonce: u64,
    dumps_dir: &Path,
) -> Result<PathBuf, CommandError> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    let prefix = dumps_dir.join(format!("baseline-{nanos}"));
    create_dump_dir_with_info(
        app,
        &prefix,
        &dump_info::DumpInfo {
            format_version: dump_info::FORMAT_VERSION,
            next_batch_nonce,
        },
    )
    .map_err(|err| match err {
        CreateDumpDirError::App(e) => CommandError::from(e),
        CreateDumpDirError::Io(e) => CommandError::from(e),
    })?;
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingress::inclusion_lane::{InclusionLane, InclusionLaneConfig};
    use crate::runtime::shutdown::RuntimeScope;
    use crate::storage::test_helpers::{TEST_MAX_DIRECT_PAYLOAD_BYTES, default_protocol_timing};
    use crate::storage::{FrontierMode, LifecycleCommand, StoredSafeInput};
    use crate::test_support::{pin_test_deployment_identity, temp_db};
    use crate::test_support::{shutdown_lane, wait_until};
    use alloy_primitives::{Address, U256};
    use app_core::application::{WalletApp, WalletConfig};
    use sequencer_core::application::{Admission, AppError, AppOutputs, SchedulerLimits};
    use sequencer_core::history::HistoryClaim;
    use sequencer_core::l2_tx::{Direct, UserOpTx};
    use sequencer_core::user_op::UserOp;
    use std::time::Duration;

    /// Counts the directs it executes and persists only that count.
    #[derive(Clone)]
    struct CountedSweepTestApp(L2TxCount);

    impl CountedSweepTestApp {
        fn new(l2_tx_count: u64) -> Self {
            Self(L2TxCount::new(l2_tx_count))
        }
    }

    impl Application for CountedSweepTestApp {
        fn user_nonce(&self, _sender: Address) -> Result<u32, AppError> {
            unreachable!("not used by setup-fill tests")
        }

        fn apply_user_op(&mut self, _op: &UserOpTx) -> Result<AppOutputs, AppError> {
            unreachable!("not used by setup-fill tests")
        }

        fn apply_direct(&mut self, _direct: &Direct) -> Result<AppOutputs, AppError> {
            self.0.advance();
            Ok(Vec::new())
        }

        fn l2_tx_count(&self) -> L2TxCount {
            self.0
        }

        fn max_method_payload_bytes() -> usize {
            0
        }

        fn scheduler_limits() -> SchedulerLimits {
            app_core::application::WALLET_SCHEDULER_LIMITS
        }

        fn admit_user_op(
            &self,
            _sender: Address,
            _op: &UserOp,
            _fee: u16,
        ) -> Result<Admission, AppError> {
            unreachable!("not used by setup-fill tests")
        }

        fn from_dump(prefix: &Path) -> Result<Self, AppError> {
            let bytes = std::fs::read(prefix.join("state"))?;
            let bytes: [u8; 8] = bytes.try_into().map_err(|_| AppError::Internal {
                reason: "invalid counted test dump".to_string(),
            })?;
            Ok(Self::new(u64::from_le_bytes(bytes)))
        }

        fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError> {
            std::fs::create_dir(prefix)?;
            std::fs::write(prefix.join("state"), self.0.get().to_le_bytes())?;
            Ok(())
        }

        fn state_file_in_dump(prefix: &Path) -> PathBuf {
            prefix.join("state")
        }

        fn from_canonical_state(
            _state_file: &Path,
            _l2_tx_count: L2TxCount,
        ) -> Result<Self, AppError> {
            unreachable!("not used by setup-fill tests")
        }
    }

    const SUBMITTER: Address = Address::repeat_byte(0x99);

    /// A setup database holding a direct at block 100 and one at `later`,
    /// synced through `safe_block`.
    fn synced_setup(
        name: &str,
        later: u64,
        safe_block: u64,
    ) -> (crate::test_support::TestDb, Storage) {
        let db = temp_db(name);
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
        pin_test_deployment_identity(&mut storage, SUBMITTER);
        storage
            .append_safe_inputs_with_timestamp(
                safe_block,
                crate::clock::unix_now_ms() / 1000,
                &[
                    StoredSafeInput {
                        sender: Address::repeat_byte(0x22),
                        payload: vec![1],
                        block_number: 100,
                    },
                    StoredSafeInput {
                        sender: Address::repeat_byte(0x22),
                        payload: vec![2],
                        block_number: later,
                    },
                ],
                SUBMITTER,
                &default_protocol_timing(),
                FrontierMode::DeferUntilAnchorSet,
            )
            .unwrap();
        (db, storage)
    }

    /// The scheduler at stop block 100: it has consumed only input 0, and
    /// the later direct (input 1) lies past the stop block, so the lane drains
    /// from input 1.
    fn queued_at_stop(next_batch_nonce: u64) -> SchedulerState {
        SchedulerState {
            l1_input_count: 1,
            next_batch_nonce,
            clock: 100,
            first_queued_input: 1,
            fridge_len: 0,
        }
    }

    #[test]
    fn publish_records_the_era_and_its_baseline_and_opens_no_tip() {
        let (_db, mut storage) = synced_setup("publish-era", 120, 150);
        let dumps = tempfile::tempdir().unwrap();
        let mut app = CountedSweepTestApp::new(41);

        let era = publish(
            &mut app,
            &mut storage,
            dumps.path(),
            L2TxCount::new(41),
            100,
            queued_at_stop(3),
        )
        .unwrap();

        assert_eq!(
            era,
            EraRecord {
                base_count: L2TxCount::new(41),
                rebuilt_history_end: L2TxCount::new(41),
                stop_block: 100,
                first_queued_input: 1,
                next_batch_nonce: 3,
            }
        );
        assert!(storage.is_setup_complete().unwrap());
        assert_eq!(storage.batch_tree_anchor().unwrap(), 3);
        let history = storage.history_state().unwrap();
        assert_eq!(history.base_l2_tx_count, 41);
        assert_eq!(history.rebuilt_history_end, 41);
        assert_eq!(history.base_safe_block, 100);
        assert_eq!(history.first_queued_input, 1);
        assert!(
            storage.open_state().unwrap().is_none(),
            "run opens the root"
        );
        // The baseline is canonical at the stop block: the first checkpoint.
        let checkpoint = storage.finalized_dump().unwrap().expect("era baseline");
        assert_eq!(checkpoint.inclusion_block, 100);
        assert_eq!(checkpoint.l2_tx_count, L2TxCount::new(41));
        assert_eq!(checkpoint.next_batch_nonce, 3);
        let snapshot = storage.latest_snapshot().unwrap().expect("baseline");
        let restored =
            CountedSweepTestApp::from_dump(&dump_info::app_prefix(&snapshot.dump.prefix)).unwrap();
        assert_eq!(restored.l2_tx_count(), L2TxCount::new(41));

        let err = publish(
            &mut CountedSweepTestApp::new(41),
            &mut storage,
            dumps.path(),
            L2TxCount::new(41),
            150,
            queued_at_stop(4),
        )
        .expect_err("an era is published once");
        assert!(matches!(
            err,
            CommandError::Lifecycle(crate::storage::LifecycleError::NotAdmissible { .. })
        ));
        assert_eq!(storage.history_state().unwrap(), history);
    }

    /// Run's root frame drains from the era's first queued input, and so does
    /// a replacement after a cascade tears the root; the baseline survives.
    #[test]
    fn the_root_and_its_replacement_drain_from_the_first_queued_input() {
        let (_db, mut storage) = synced_setup("root-drains-queued", 1400, 1500);
        let dumps = tempfile::tempdir().unwrap();
        publish(
            &mut CountedSweepTestApp::new(41),
            &mut storage,
            dumps.path(),
            L2TxCount::new(41),
            100,
            queued_at_stop(3),
        )
        .unwrap();
        let generation = storage.history_state().unwrap().version.recovery_generation;
        storage
            .ensure_open_tip_for_recovery(
                &default_protocol_timing(),
                crate::clock::unix_now_ms(),
                TEST_MAX_DIRECT_PAYLOAD_BYTES,
            )
            .unwrap();
        assert_eq!(storage.open_state().unwrap().unwrap().safe_block, 1500);

        let drained = |storage: &mut Storage| {
            let current = storage.history_state().unwrap();
            let page = storage
                .canonical_history_page(
                    HistoryClaim {
                        version: current.version,
                        next_input: L2TxCount::new(41),
                    },
                    16,
                )
                .unwrap();
            page.rows
                .iter()
                .map(|row| match &row.context {
                    crate::storage::L2TxContext::Direct { tx, .. } => {
                        (row.offset.get(), tx.input_index)
                    }
                    other => panic!("expected a drained direct, got {other:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(drained(&mut storage), vec![(41, 1)]);

        // The root ages past the threshold and is torn.
        storage
            .append_safe_inputs(2800, &[], SUBMITTER, &default_protocol_timing())
            .unwrap();
        assert_eq!(storage.recover_aging_tip(1200).unwrap(), vec![0]);
        let current = storage.history_state().unwrap();
        assert_eq!(
            current.version.recovery_generation.get(),
            generation.get() + 1
        );
        assert_eq!(current.base_safe_block, 100);
        assert_eq!(current.rebuilt_history_end, 41);
        assert_eq!(drained(&mut storage), vec![(41, 1)]);
        assert!(
            storage.latest_snapshot().unwrap().is_some(),
            "the baseline survives"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_after_setup_credits_a_queued_deposit_exactly_once() {
        let db = temp_db("setup-then-lane-credits-once");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");
        let timing = default_protocol_timing();

        // The real `WalletApp`: only it exposes a queryable balance. Its config
        // round-trips through the SSZ dump, so the lane reloads it.
        let cfg = WalletConfig::devnet();
        let portal = cfg.erc20_portal_address;
        let token = cfg.supported_erc20_token;
        let mut base = WalletApp::new(cfg);
        let other = Address::repeat_byte(0x22);
        let recipient = Address::repeat_byte(0x77);
        let deposit_value = U256::from(2_000_000_u64);
        let mut deposit_payload = Vec::with_capacity(20 + 20 + 32);
        deposit_payload.extend_from_slice(token.as_slice());
        deposit_payload.extend_from_slice(recipient.as_slice());
        deposit_payload.extend_from_slice(deposit_value.to_be_bytes::<32>().as_slice());

        // Three directs the base already executed, then a deposit at block
        // 120, past stop block 100: the era's first queued input.
        let inputs: Vec<StoredSafeInput> = [10, 20, 30]
            .into_iter()
            .map(|block_number| StoredSafeInput {
                sender: other,
                payload: vec![1],
                block_number,
            })
            .chain([StoredSafeInput {
                sender: portal,
                payload: deposit_payload,
                block_number: 120,
            }])
            .collect();
        for _ in 0..3 {
            base.apply_direct(&Direct {
                input_index: 0,
                sender: other,
                block: 10,
                timestamp: 0,
                payload: vec![1],
            })
            .unwrap();
        }
        {
            let mut storage =
                Storage::initialize_for_command(db.path.as_str(), LifecycleCommand::Setup).unwrap();
            pin_test_deployment_identity(&mut storage, Address::repeat_byte(0x99));
            storage
                .append_safe_inputs(150, &inputs, Address::repeat_byte(0x99), &timing)
                .unwrap();
            publish(
                &mut base,
                &mut storage,
                dumps_dir.path(),
                L2TxCount::new(3),
                100,
                SchedulerState {
                    l1_input_count: 3,
                    next_batch_nonce: 0,
                    clock: 30,
                    first_queued_input: 3,
                    fridge_len: 0,
                },
            )
            .unwrap();
            let baseline = storage.latest_snapshot().unwrap().expect("baseline");
            let at_stop =
                WalletApp::from_dump(&dump_info::app_prefix(&baseline.dump.prefix)).unwrap();
            assert_eq!(at_stop.current_user_balance(recipient), U256::ZERO);
            // Run's startup opens the root frame at its own safe head.
            storage
                .ensure_open_tip_for_recovery(
                    &timing,
                    crate::clock::unix_now_ms(),
                    WalletApp::scheduler_limits().max_direct_payload_bytes,
                )
                .unwrap();
        }

        let storage = Storage::open(db.path.as_str()).expect("reopen for lane");
        let config = InclusionLaneConfig {
            dumps_dir: dumps_dir.path().to_path_buf(),
            max_user_ops_per_chunk: 16,
            safe_input_buffer_capacity: 16,
            max_batch_open: Duration::from_millis(10),
            idle_poll_interval: Duration::from_millis(2),
            frontier_min_interval: Duration::ZERO,
        };
        let shutdown = RuntimeScope::default();
        let (_tx, handle) =
            InclusionLane::<WalletApp>::start(128, shutdown.clone(), storage, config);

        // The lane's state shows only through the dump it writes at batch close.
        let credited = wait_until(Duration::from_secs(5), || {
            let mut s = Storage::open(db.path.as_str()).expect("open observer");
            match s.latest_snapshot().expect("read pending") {
                Some(p) => {
                    WalletApp::from_dump(&dump_info::app_prefix(&p.dump.prefix))
                        .expect("load lane snapshot")
                        .current_user_balance(recipient)
                        == deposit_value
                }
                None => false,
            }
        })
        .await;
        assert!(credited, "the root frame executes the queued deposit");
        {
            let mut s = Storage::open(db.path.as_str()).expect("open observer");
            assert_eq!(s.next_undrained_safe_input_index().unwrap(), 4);
            let dump = s.latest_snapshot().unwrap().expect("a post-deposit dump");
            let app = WalletApp::from_dump(&dump_info::app_prefix(&dump.dump.prefix)).unwrap();
            assert_eq!(
                app.current_user_balance(recipient),
                deposit_value,
                "credited once"
            );
            assert_eq!(app.l2_tx_count(), L2TxCount::new(4));
        }

        shutdown_lane(&shutdown, handle).await;
    }
}
