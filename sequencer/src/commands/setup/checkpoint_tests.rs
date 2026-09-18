// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::{Path, PathBuf};

use alloy_primitives::{Address, U256};
use app_core::application::{WalletApp, WalletConfig};
use sequencer_core::application::{
    AppError, AppOutputs, Application, ApplicationProgress, ValidationOutcome, execute_direct_input,
};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
use sequencer_core::user_op::UserOp;

use super::{Checkpoint, rebuild_from_checkpoint};
use crate::commands::error::{BootstrapError, CommandError, EXIT_RESTART_TRANSIENT};
use crate::ingress::inclusion_lane::dump_info;
use crate::recovery::{RecoveryError, RecoveryFailure, RecoveryRetryReason};
use crate::storage::test_helpers::{
    SENDER_A, default_protocol_timing, pin_test_deployment_identity, temp_db,
};
use crate::storage::{FrontierMode, LifecycleCommand, Storage, StoredSafeInput};

struct ReplayForbiddenApp(ApplicationProgress);

impl Application for ReplayForbiddenApp {
    fn max_method_payload_bytes() -> usize {
        0
    }

    fn validate_user_op(
        &self,
        _: Address,
        _: &UserOp,
        _: u16,
    ) -> Result<ValidationOutcome, AppError> {
        panic!("an uncovered checkpoint must refuse before application execution")
    }

    fn apply_valid_user_op(&mut self, _: &ValidUserOp, _: u64) -> Result<AppOutputs, AppError> {
        panic!("an uncovered checkpoint must refuse before application execution")
    }

    fn apply_direct_input(&mut self, _: &DirectInput) -> Result<AppOutputs, AppError> {
        panic!("an uncovered checkpoint must refuse before application execution")
    }

    fn progress(&self) -> ApplicationProgress {
        self.0
    }

    fn from_dump(_: &Path) -> Result<Self, AppError> {
        unreachable!("the checkpoint is supplied by the fixture")
    }

    fn create_dump(&mut self, _: &Path) -> Result<(), AppError> {
        panic!("an uncovered checkpoint must refuse before artifact creation")
    }

    fn state_file_in_dump(_: &Path) -> PathBuf {
        unreachable!("the refused checkpoint has no new artifact")
    }
}

#[test]
fn recovery_refuses_stop_before_checkpoint_without_replay_or_publication() {
    // The last case has H1 >= B: resync reaching the checkpoint cannot replace
    // the fixed fold boundary C. The earlier cases model an honestly lagging node.
    for (application_clock, checkpoint_block, stop_block, resynced_head) in [
        (900, 901, 899, 899),
        (900, 964, 930, 930),
        (900, 964, 930, 964),
    ] {
        let db = temp_db("recovery-uncovered-checkpoint");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild).unwrap();
        pin_test_deployment_identity(&mut storage, SENDER_A);
        let identity = storage.deployment_identity().unwrap().unwrap();
        let inputs = if resynced_head > application_clock {
            vec![StoredSafeInput {
                sender: Address::repeat_byte(0x22),
                payload: vec![1],
                block_number: application_clock + 1,
            }]
        } else {
            vec![]
        };
        storage
            .append_safe_inputs_with_timestamp(
                resynced_head,
                resynced_head,
                &inputs,
                SENDER_A,
                &default_protocol_timing(),
                FrontierMode::DeferUntilAnchorSet,
            )
            .unwrap();
        let checkpoint = Checkpoint {
            app: ReplayForbiddenApp(
                ApplicationProgress::try_new(ExecutedInputCount::new(1), application_clock)
                    .unwrap(),
            ),
            executed_safe_block: application_clock,
            checkpoint_nonce: 1,
            checkpoint_block,
        };
        let dumps = tempfile::tempdir().unwrap();

        let error = rebuild_from_checkpoint(
            checkpoint,
            &identity,
            stop_block,
            &mut storage,
            dumps.path(),
        )
        .expect_err("the RPC stopping block must cover the trusted checkpoint");

        assert_eq!(error.exit_code(), EXIT_RESTART_TRANSIENT);
        assert!(matches!(
            error,
            CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::Retry(ref failure)))
                if matches!(failure.as_ref(), RecoveryFailure::PolicyRetry(
                    RecoveryRetryReason::CheckpointAheadOfStop {
                        checkpoint_block: found_checkpoint,
                        stop_block: found_stop,
                    }) if *found_checkpoint == checkpoint_block && *found_stop == stop_block)
        ));
        assert!(!storage.is_setup_complete().unwrap());
        assert!(matches!(
            storage.history_state(),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        assert!(storage.open_state().unwrap().is_none());
        assert!(storage.latest_snapshot().unwrap().is_none());
        assert_eq!(std::fs::read_dir(dumps.path()).unwrap().count(), 0);
        assert_eq!(storage.current_safe_block().unwrap(), Some(resynced_head));
        assert_eq!(
            storage.safe_input_end_exclusive().unwrap(),
            inputs.len() as u64
        );
    }
}

#[test]
fn recovery_publishes_at_checkpoint_or_later_including_genesis() {
    let owner = Address::repeat_byte(0x77);
    for (checkpoint_block, stop_block, expected_count, expected_balance) in
        [(10, 10, 2, 120_u64), (10, 15, 3, 150), (0, 0, 0, 0)]
    {
        let db = temp_db("recovery-covered-checkpoint");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild).unwrap();
        pin_test_deployment_identity(&mut storage, SENDER_A);
        let identity = storage.deployment_identity().unwrap().unwrap();
        let config = WalletConfig::devnet();
        let deposit = |block, amount: u64| DirectInput {
            sender: config.erc20_portal_address,
            block_number: block,
            payload: [
                config.supported_erc20_token.as_slice(),
                owner.as_slice(),
                U256::from(amount).to_be_bytes::<32>().as_slice(),
            ]
            .concat(),
        };
        let mut app = WalletApp::new(config.clone());
        if checkpoint_block != 0 {
            execute_direct_input(&mut app, &deposit(5, 100)).unwrap();
        }
        let checkpoint = Checkpoint {
            executed_safe_block: app.last_executed_safe_block(),
            app,
            checkpoint_nonce: u64::from(checkpoint_block != 0),
            checkpoint_block,
        };
        let resynced_head = stop_block + 5;
        let inputs = [(5, 100), (7, 20), (12, 30), (16, 40)]
            .into_iter()
            .filter(|(block, _)| *block <= resynced_head)
            .map(|(block, amount)| {
                let input = deposit(block, amount);
                StoredSafeInput {
                    sender: input.sender,
                    payload: input.payload,
                    block_number: block,
                }
            })
            .collect::<Vec<_>>();
        storage
            .append_safe_inputs_with_timestamp(
                resynced_head,
                resynced_head,
                &inputs,
                SENDER_A,
                &default_protocol_timing(),
                FrontierMode::DeferUntilAnchorSet,
            )
            .unwrap();
        let dumps = tempfile::tempdir().unwrap();

        rebuild_from_checkpoint(
            checkpoint,
            &identity,
            stop_block,
            &mut storage,
            dumps.path(),
        )
        .unwrap();

        assert!(storage.is_setup_complete().unwrap());
        let history = storage.history_state().unwrap();
        assert_eq!(history.base_safe_block, stop_block);
        assert_eq!(history.base_executed_input_count, expected_count);
        assert_eq!(
            storage.open_state().unwrap().unwrap().safe_block,
            stop_block
        );
        let snapshot = storage.latest_snapshot().unwrap().unwrap();
        let app = WalletApp::from_dump(&dump_info::app_prefix(&snapshot.dump.prefix)).unwrap();
        assert_eq!(app.executed_input_count().get(), expected_count);
        assert_eq!(
            app.current_user_balance(owner),
            U256::from(expected_balance)
        );
    }
}
