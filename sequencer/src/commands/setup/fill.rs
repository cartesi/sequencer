// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! File-first baseline creation for setup. The complete baseline, optional
//! recovery root, and setup-complete fact become visible in one transaction.

use crate::commands::error::{CommandError, SetupRecoveryError};
use crate::ingress::inclusion_lane::dump_info::{
    self, CreateDumpDirError, create_dump_dir_with_info,
};
use sequencer_core::application::Application;

pub(crate) fn register_genesis_baseline<A: Application + 'static>(
    mut initial_app: A,
    storage: &mut crate::storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<(), CommandError> {
    let count = initial_app.executed_input_count();
    if count.get() != 0 {
        return Err(CommandError::AppBootstrap(
            sequencer_core::application::AppError::Internal {
                reason: format!(
                    "a genesis application must start at executed_input_count = 0, got {}",
                    count.get()
                ),
            },
        ));
    }
    let prefix = write_baseline_dump(&mut initial_app, 0, dumps_dir)?;
    storage.complete_baseline_setup(&prefix, count, 0, 0, false)?;
    Ok(())
}

/// The terminal fold has already applied every direct through C. Its dump
/// is a local resume baseline; it is not a canonical comparison checkpoint.
pub(crate) fn fill_recovery_state<A: Application + 'static>(
    mut recovered_app: A,
    resume_nonce: u64,
    stop_block: u64,
    storage: &mut crate::storage::Storage,
    dumps_dir: &std::path::Path,
) -> Result<(), CommandError> {
    if storage.is_setup_complete()? {
        return Err(SetupRecoveryError::AlreadySetUp.into());
    }
    let count = recovered_app.executed_input_count();
    let prefix = write_baseline_dump(&mut recovered_app, resume_nonce, dumps_dir)?;
    storage.complete_baseline_setup(&prefix, count, stop_block, resume_nonce, true)?;
    Ok(())
}

fn write_baseline_dump<A: Application>(
    app: &mut A,
    next_batch_nonce: u64,
    dumps_dir: &std::path::Path,
) -> Result<std::path::PathBuf, CommandError> {
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
    use crate::commands::test_support::SweepTestApp;
    use crate::ingress::inclusion_lane::{InclusionLane, InclusionLaneConfig, InclusionLaneError};
    use crate::runtime::shutdown::RuntimeScope;
    use crate::storage::test_helpers::{pin_test_deployment_identity, temp_db};
    use crate::storage::{LifecycleCommand, Storage};
    use alloy_primitives::{Address, U256};
    use app_core::application::{WalletApp, WalletConfig};
    use sequencer_core::application::{
        AppError, AppOutputs, ApplicationProgress, ValidationOutcome,
    };
    use sequencer_core::history::ExecutedInputCount;
    use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
    use sequencer_core::user_op::UserOp;
    use std::path::Path;
    use std::time::Duration;

    #[derive(Clone)]
    struct CountedSweepTestApp(ApplicationProgress);

    impl CountedSweepTestApp {
        fn new(executed_input_count: u64) -> Self {
            Self(
                ApplicationProgress::try_new(ExecutedInputCount::new(executed_input_count), 0)
                    .expect("coherent progress"),
            )
        }
    }

    impl Application for CountedSweepTestApp {
        fn max_method_payload_bytes() -> usize {
            0
        }

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _current_fee: u16,
        ) -> Result<ValidationOutcome, AppError> {
            Ok(ValidationOutcome::Accept)
        }

        fn apply_valid_user_op(
            &mut self,
            _user_op: &ValidUserOp,
            _safe_block: u64,
        ) -> Result<AppOutputs, AppError> {
            unreachable!("not used by setup-fill tests")
        }

        fn apply_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError> {
            self.0.advance(input.block_number);
            Ok(Vec::new())
        }

        fn progress(&self) -> ApplicationProgress {
            self.0
        }

        fn from_dump(prefix: &Path) -> Result<Self, AppError> {
            let bytes = std::fs::read(prefix.join("state"))?;
            let bytes: [u8; 16] = bytes.try_into().map_err(|_| AppError::Internal {
                reason: "invalid counted test dump".to_string(),
            })?;
            let count = u64::from_le_bytes(bytes[..8].try_into().expect("eight-byte count"));
            let safe_block =
                u64::from_le_bytes(bytes[8..].try_into().expect("eight-byte safe block"));
            Ok(Self(
                ApplicationProgress::try_new(ExecutedInputCount::new(count), safe_block)
                    .expect("coherent progress"),
            ))
        }

        fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError> {
            std::fs::create_dir(prefix)?;
            let mut bytes = Vec::with_capacity(16);
            bytes.extend_from_slice(&self.0.executed_input_count().get().to_le_bytes());
            bytes.extend_from_slice(&self.0.last_executed_safe_block().to_le_bytes());
            std::fs::write(prefix.join("state"), bytes)?;
            Ok(())
        }

        fn state_file_in_dump(prefix: &Path) -> std::path::PathBuf {
            <SweepTestApp as Application>::state_file_in_dump(prefix)
        }
    }

    #[test]
    fn recovery_publishes_complete_baseline_without_application_padding() {
        use crate::storage::test_helpers::default_protocol_timing;
        use crate::storage::{FrontierMode, StoredSafeInput};
        let db = temp_db("complete-recovery-baseline");
        let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild)
            .expect("initialize rebuild");
        let dumps = tempfile::tempdir().expect("dumps");
        let submitter = Address::repeat_byte(0x99);
        pin_test_deployment_identity(&mut storage, submitter);
        storage
            .append_safe_inputs_with_timestamp(
                150,
                150,
                &[
                    StoredSafeInput {
                        sender: Address::repeat_byte(0x22),
                        payload: vec![1],
                        block_number: 100,
                    },
                    StoredSafeInput {
                        sender: Address::repeat_byte(0x22),
                        payload: vec![2],
                        block_number: 120,
                    },
                ],
                submitter,
                &default_protocol_timing(),
                FrontierMode::DeferUntilAnchorSet,
            )
            .expect("ingest prefix and later direct");
        fill_recovery_state(
            CountedSweepTestApp::new(41),
            3,
            100,
            &mut storage,
            dumps.path(),
        )
        .expect("complete baseline");
        assert!(storage.is_setup_complete().expect("completion"));
        assert_eq!(storage.batch_tree_anchor().expect("anchor"), 3);
        let history = storage.history_state().expect("history");
        assert_eq!(history.base_executed_input_count, 41);
        assert_eq!(history.base_safe_block, 100);
        let root = storage.open_state().expect("root").expect("open root");
        assert_eq!(root.safe_block, 100);
        assert_eq!(
            storage.next_executed_input_count().expect("head"),
            ExecutedInputCount::new(41)
        );
        assert!(
            storage
                .finalized_dump()
                .expect("accepted checkpoint")
                .is_none()
        );
        let snapshot = storage
            .latest_snapshot()
            .expect("baseline")
            .expect("baseline");
        assert_eq!(snapshot.executed_input_count, ExecutedInputCount::new(41));
        let restored =
            CountedSweepTestApp::from_dump(&dump_info::app_prefix(&snapshot.dump.prefix))
                .expect("restore baseline");
        assert_eq!(
            restored.executed_input_count(),
            snapshot.executed_input_count
        );
        assert_eq!(
            storage
                .read(|tx| tx
                    .query_row("SELECT COUNT(*) FROM application_inputs", [], |row| row
                        .get::<_, i64>(0)))
                .expect("application rows"),
            0
        );
        let err = fill_recovery_state(
            CountedSweepTestApp::new(42),
            4,
            150,
            &mut storage,
            dumps.path(),
        )
        .expect_err("completed rebuild is one-shot");
        assert!(matches!(
            err,
            CommandError::Bootstrap(crate::commands::error::BootstrapError::SetupRecovery(
                SetupRecoveryError::AlreadySetUp
            ))
        ));
        assert_eq!(
            storage.history_state().expect("unchanged baseline"),
            history
        );
    }

    #[test]
    fn failed_baseline_registration_rolls_back_root_history_and_completion() {
        let db = temp_db("baseline-registration-rollback");
        let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild)
            .expect("initialize rebuild");
        let dumps = tempfile::tempdir().expect("dumps");
        storage
            .write(|tx| {
                tx.execute_batch(
                    "CREATE TRIGGER fail_baseline BEFORE INSERT ON snapshots
            BEGIN SELECT RAISE(ABORT, 'injected baseline failure'); END;",
                )
            })
            .expect("inject failure");
        let err = fill_recovery_state(
            CountedSweepTestApp::new(41),
            3,
            100,
            &mut storage,
            dumps.path(),
        )
        .expect_err("registration must fail");
        assert!(err.to_string().contains("injected baseline failure"));
        assert!(!storage.is_setup_complete().expect("completion absent"));
        assert!(storage.open_state().expect("root absent").is_none());
        assert_eq!(storage.batch_tree_anchor().expect("unchanged anchor"), 0);
        assert_eq!(
            storage
                .read(
                    |tx| tx.query_row("SELECT COUNT(*) FROM history_state", [], |row| row
                        .get::<_, i64>(0))
                )
                .expect("no history"),
            0
        );
        assert_eq!(
            std::fs::read_dir(dumps.path())
                .expect("orphan dump")
                .count(),
            1
        );
        storage
            .write(|tx| tx.execute_batch("DROP TRIGGER fail_baseline"))
            .expect("remove failure");
        fill_recovery_state(
            CountedSweepTestApp::new(42),
            4,
            150,
            &mut storage,
            dumps.path(),
        )
        .expect("fresh retry after atomic rollback");
        assert_eq!(
            storage
                .history_state()
                .expect("history")
                .base_executed_input_count,
            42
        );
        assert_eq!(storage.open_tip_nonce().expect("root nonce"), Some(4));
    }

    #[test]
    fn plain_setup_refuses_a_nonzero_genesis_application_boundary() {
        let db = temp_db("nonzero-genesis");
        let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Setup)
            .expect("initialize setup");
        let dumps = tempfile::tempdir().expect("dumps");
        assert!(
            register_genesis_baseline(CountedSweepTestApp::new(1), &mut storage, dumps.path())
                .is_err()
        );
        assert!(!storage.is_setup_complete().expect("completion absent"));
        assert!(
            storage
                .latest_snapshot()
                .expect("snapshot absent")
                .is_none()
        );
        assert_eq!(
            std::fs::read_dir(dumps.path())
                .expect("no artifacts")
                .count(),
            0
        );
    }

    #[test]
    fn root_invalidation_keeps_baseline_floor_and_replays_only_post_baseline_directs() {
        use crate::storage::test_helpers::default_protocol_timing;
        use crate::storage::{FrontierMode, StoredSafeInput};
        use sequencer_core::history::HistoryClaim;
        let db = temp_db("baseline-survives-root-invalidation");
        let mut storage = Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild)
            .expect("initialize rebuild");
        let dumps = tempfile::tempdir().expect("dumps");
        let submitter = Address::repeat_byte(0x99);
        pin_test_deployment_identity(&mut storage, submitter);
        storage
            .append_safe_inputs_with_timestamp(
                1500,
                1500,
                &[
                    StoredSafeInput {
                        sender: Address::repeat_byte(0x22),
                        payload: vec![1],
                        block_number: 100,
                    },
                    StoredSafeInput {
                        sender: Address::repeat_byte(0x22),
                        payload: vec![2],
                        block_number: 1400,
                    },
                ],
                submitter,
                &default_protocol_timing(),
                FrontierMode::DeferUntilAnchorSet,
            )
            .expect("ingest inputs");
        fill_recovery_state(
            CountedSweepTestApp::new(41),
            3,
            100,
            &mut storage,
            dumps.path(),
        )
        .expect("complete baseline");
        let history = storage.history_state().expect("history");
        assert_eq!(
            storage.recover_aging_tip(1200).expect("recover root"),
            vec![0]
        );
        let current = storage.history_state().expect("current history");
        assert_eq!(current.base_safe_block, 100);
        assert_eq!(current.base_executed_input_count, 41);
        assert_eq!(
            current.version.recovery_generation.get(),
            history.version.recovery_generation.get() + 1
        );
        let snapshot = storage
            .latest_snapshot()
            .expect("baseline")
            .expect("baseline survives");
        let mut app = CountedSweepTestApp::from_dump(&dump_info::app_prefix(&snapshot.dump.prefix))
            .expect("restore");
        let page = storage
            .canonical_history_page(
                HistoryClaim {
                    version: current.version,
                    next_input: snapshot.executed_input_count,
                },
                16,
            )
            .expect("replacement history");
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].offset, ExecutedInputCount::new(41));
        match &page.rows[0].context {
            crate::storage::L2TxContext::DirectInput { tx, .. } => {
                assert_eq!(tx.block_number, 1400);
                sequencer_core::application::execute_direct_input(&mut app, tx)
                    .expect("replay direct");
            }
            _ => panic!("expected post-baseline direct"),
        }
        assert_eq!(app.executed_input_count(), ExecutedInputCount::new(42));
        assert!(
            storage
                .recover_aging_tip(1200)
                .expect("fresh replacement")
                .is_empty()
        );
        assert_eq!(
            storage.next_executed_input_count().expect("head"),
            ExecutedInputCount::new(42)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_then_lane_credits_post_c_deposit_exactly_once() {
        use crate::storage::StoredSafeInput;
        use crate::storage::test_helpers::default_protocol_timing;

        let db = temp_db("recovery-credited-once");
        let dumps_dir = tempfile::tempdir().expect("dumps dir");
        let timing = default_protocol_timing();

        // The recovered checkpoint state S' is the real `WalletApp` (only it
        // exposes a queryable balance). Its config round-trips through the SSZ
        // dump, so the lane reloads a devnet-configured app.
        let cfg = WalletConfig::devnet();
        let portal = cfg.erc20_portal_address;
        let token = cfg.supported_erc20_token;
        let recovered = WalletApp::new(cfg);
        let submitter = Address::repeat_byte(0x99); // ingest classifier (not the portal)
        let pre_c_direct = Address::repeat_byte(0x22); // non-portal => decode None => inert
        let nested_sender = Address::repeat_byte(0x77); // fresh account, zero balance in S'
        let deposit_value = U256::from(2_000_000_u64);

        // C = 100; the resync reached H1 = 150. Three `<= C` directs (already
        // folded into S'), plus ONE portal USDC deposit at block 120 in the
        // (C, H1] window. Deposit payload mirrors `encode_erc20_deposit_payload`:
        // token || nested_sender || value_be32.
        let deposit_payload = {
            let mut p = Vec::with_capacity(20 + 20 + 32);
            p.extend_from_slice(token.as_slice());
            p.extend_from_slice(nested_sender.as_slice());
            p.extend_from_slice(deposit_value.to_be_bytes::<32>().as_slice());
            p
        };
        let inputs = vec![
            StoredSafeInput {
                sender: pre_c_direct,
                payload: vec![0x01],
                block_number: 10,
            },
            StoredSafeInput {
                sender: pre_c_direct,
                payload: vec![0x02],
                block_number: 20,
            },
            StoredSafeInput {
                sender: pre_c_direct,
                payload: vec![0x03],
                block_number: 30,
            },
            StoredSafeInput {
                sender: portal,
                payload: deposit_payload,
                block_number: 120,
            },
        ];
        {
            let mut storage =
                Storage::initialize_for_command(db.path.as_str(), LifecycleCommand::Rebuild)
                    .expect("initialize rebuild");
            pin_test_deployment_identity(&mut storage, submitter);
            storage
                .append_safe_inputs(150, &inputs, submitter, &timing)
                .expect("sync to H1 = 150");
            // Fill at N' = 3, C = 100: drains only the three `<= C` directs into
            // the recovery root frame; the deposit (index 3) stays undrained.
            fill_recovery_state(recovered, 3, 100, &mut storage, dumps_dir.path())
                .expect("recovery fill");

            // Preconditions (the (C, H1] cap — covered by the cap tests; pinned
            // here so a regression that drained the deposit fails early).
            assert_eq!(
                storage.next_undrained_safe_input_index().expect("cursor"),
                3,
                "the (C, H1] deposit must be left UNDRAINED at index 3"
            );
            let finalized = storage
                .latest_snapshot()
                .expect("read")
                .expect("baseline exists");
            let s_prime = WalletApp::from_dump(&dump_info::app_prefix(&finalized.dump.prefix))
                .expect("load S'");
            assert_eq!(
                s_prime.current_user_balance(nested_sender),
                U256::ZERO,
                "S' itself must NOT have credited the deposit"
            );
        }

        // Drive the REAL run-side inclusion lane against the recovered DB. The
        // frontier advance `C -> H1` is already staged (l1_safe_head = 150, the
        // recovery tip frame's safe block = 100), so the lane's first iteration
        // leads the deposit at index 3 and executes it once. `batch_submitter`
        // differs from the portal, so the deposit is not skipped as an own-batch
        // input; the short `max_batch_open` forces a batch-close snapshot.
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

        // Observe via a SECOND storage handle (WAL); the lane owns its own. The
        // executed-deposit state is externalized only through the pending dump
        // the lane writes at batch close. Wait for it to reflect the credit.
        let credited = wait_until(Duration::from_secs(5), || {
            let mut s = Storage::open(db.path.as_str()).expect("open observer");
            match s.latest_snapshot().expect("read pending") {
                Some(p) => {
                    WalletApp::from_dump(&dump_info::app_prefix(&p.dump.prefix))
                        .expect("load lane snapshot")
                        .current_user_balance(nested_sender)
                        == deposit_value
                }
                None => false,
            }
        })
        .await;
        assert!(
            credited,
            "the lane must lead + execute the (C, H1] deposit, crediting it once"
        );

        // Exactly once: the drain cursor advanced to 4 (one past the deposit), so
        // a restart would not re-lead it; and the balance is the deposit value
        // (== once; 0 would be lost, 2x would be double-credited).
        {
            let mut s = Storage::open(db.path.as_str()).expect("open observer");
            assert_eq!(
                s.next_undrained_safe_input_index().expect("cursor"),
                4,
                "the deposit's drain cursor must advance exactly one past it"
            );
            let dump = s
                .latest_snapshot()
                .expect("pending")
                .expect("a post-deposit pending dump");
            let app =
                WalletApp::from_dump(&dump_info::app_prefix(&dump.dump.prefix)).expect("load");
            assert_eq!(
                app.current_user_balance(nested_sender),
                deposit_value,
                "credited EXACTLY once"
            );
        }

        shutdown_lane(&shutdown, handle).await;
    }

    async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let started = tokio::time::Instant::now();
        while started.elapsed() < timeout {
            if predicate() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        predicate()
    }

    async fn shutdown_lane(
        shutdown: &RuntimeScope,
        handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
    ) {
        shutdown.request_shutdown();
        let joined = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("wait for lane shutdown");
        let result = joined.expect("join lane task");
        assert!(result.is_ok(), "lane should shut down cleanly: {result:?}");
    }
}
