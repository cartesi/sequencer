// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The operator commands: each child module is one command *bracket* —
//! acquire the exclusive process lock, preflight the admission facts,
//! execute the body, and best-effort record a terminal cause in the black
//! box. The *mechanisms* the brackets invoke live in their domain modules
//! (`crate::recovery` for the reducer/flusher, `crate::runtime` for the
//! shared authority machinery: process lock, `RuntimeScope`).
//!
//! - [`run`] — boot workers from a set-up DB (plus `workers`, its supervisor)
//! - [`setup`] — establish the timeless deployment state
//! - [`flush`] — settle the wallet nonce without launching
//!
//! This module hosts the helpers shared by more than one bracket (identity
//! gates, keyed-writer verification, lifecycle preflights). The CLI harness
//! that dispatches the commands lives in [`crate::harness`].

pub mod config;
pub mod error;
pub mod flush;
pub mod run;
pub mod setup;
#[cfg(test)]
pub(crate) mod test_support;

pub use error::{
    BootstrapError, CommandError, IdentityError, SetupRecoveryError, SetupRefuse, WorkerExit,
};

use std::time::Duration;

use crate::storage::{self, DeploymentIdentity, LifecycleCommand};
use alloy_primitives::Address;

pub(crate) const INPUT_READER_POLL_INTERVAL: Duration = Duration::from_secs(2);

fn preflight_lifecycle_command(
    db_path: &str,
    command: LifecycleCommand,
) -> Result<(), CommandError> {
    if !std::path::Path::new(db_path).try_exists()? {
        return Err(BootstrapError::SetupNotComplete.into());
    }
    let storage = storage::Storage::open_read_only(db_path)?;
    storage.preflight_lifecycle_command(command)?;
    Ok(())
}

/// Verdict-neutral black-box settlement: when the command ended terminal,
/// record its cause best-effort. Telemetry must never change a verdict — a
/// failed record loses only the black-box copy, and the exit code and logs
/// still carry it.
pub(crate) fn record_terminal_fault_best_effort(
    db_path: &str,
    command: LifecycleCommand,
    result: &Result<(), CommandError>,
) {
    let Err(error) = result else { return };
    if !error.failure_verdict().is_terminal() {
        return;
    }
    let cause = error.to_string();
    let recorded = storage::Storage::open_writer(db_path)
        .map_err(|open_error| open_error.to_string())
        .and_then(|mut storage| {
            storage
                .record_terminal_fault(command, &cause)
                .map_err(|record_error| record_error.to_string())
        });
    if let Err(record_error) = recorded {
        tracing::warn!(
            error = %record_error,
            cause = %cause,
            "terminal cause not recorded in the black box; the exit code and this log carry the verdict"
        );
    }
}

/// Verdict-neutral startup read of the black box: if a previous command on
/// this data directory died terminal, say so once at boot, so the cause is in
/// this process's log even when the last process's final lines were lost.
/// Nothing branches on the value. Bounded by the settlement writer: a death
/// that never returned through its command bracket (an abort at the
/// two-second deadline, a controller panic, SIGKILL) left no row, so there
/// is nothing to report. It runs ahead of the admission preflight so that a
/// boot the preflight refuses still logs why the last one died; a data
/// directory with no database yet is the ordinary case, not a warning.
pub(crate) fn warn_on_previous_terminal_fault(db_path: &str) {
    let read = storage::Storage::open_read_only(db_path)
        .map_err(|open_error| open_error.to_string())
        .and_then(|storage| {
            storage
                .latest_terminal_fault()
                .map_err(|read_error| read_error.to_string())
        });
    match read {
        Ok(Some(fault)) => tracing::warn!(
            command = %fault.command,
            cause = %fault.cause,
            recorded_at_ms = fault.recorded_at_ms,
            "a previous command on this data directory ended in a terminal fault"
        ),
        Ok(None) => {}
        Err(error) => tracing::debug!(
            error = %error,
            "no readable black box at startup (fresh data directory, or the \
             preflight is about to report why); continuing"
        ),
    }
}

pub(crate) fn batch_submitter_address_from_private_key(
    private_key: &str,
) -> Result<Address, CommandError> {
    use alloy::signers::local::PrivateKeySigner;
    use std::str::FromStr;

    // Deterministic operator misconfig — terminal, like every signer
    // misconfiguration. The message never echoes key material.
    Ok(PrivateKeySigner::from_str(private_key)
        .map_err(|_| {
            CommandError::Bootstrap(BootstrapError::SignerMisconfig {
                message: "invalid batch submitter private key".to_string(),
            })
        })?
        .address())
}

/// Gate `run`/`flush` on a completed `setup` and return the pinned identity.
/// A missing completion fact — or a completion fact without an identity (a
/// corrupt/incomplete setup) — is a terminal `SetupNotComplete`: the operator must
/// (re-)run `setup`, not retry `run`.
pub(crate) fn load_setup_identity(db_path: &str) -> Result<DeploymentIdentity, CommandError> {
    let storage = storage::Storage::open_read_only(db_path)?;
    if !storage.is_setup_complete()? {
        return Err(BootstrapError::SetupNotComplete.into());
    }
    match storage.deployment_identity()? {
        Some(identity) => Ok(identity),
        None => Err(BootstrapError::SetupNotComplete.into()),
    }
}

/// Verify that the RPC's `eth_chainId` matches the configured chain id.
///
/// Treated as fatal on mismatch *and* on RPC error: pinning a wrong or
/// unverified chain id into storage would poison subsequent L1-unreachable
/// boots and issue soft confirmations against the wrong chain. Caller is
/// expected to retry on `ChainIdRpc`.
pub(crate) async fn validate_rpc_chain_id(
    eth_rpc_url: &str,
    expected: u64,
    allow_insecure: bool,
) -> Result<(), CommandError> {
    use alloy::providers::Provider;
    let check_provider = crate::l1::provider::create_provider(eth_rpc_url, allow_insecure)
        .map_err(|e| CommandError::Io(std::io::Error::other(e)))?;
    match check_provider.get_chain_id().await {
        Ok(rpc_chain_id) if rpc_chain_id != expected => {
            Err(CommandError::Bootstrap(BootstrapError::ChainIdMismatch {
                rpc: rpc_chain_id,
                config: expected,
            }))
        }
        Ok(_) => Ok(()),
        Err(e) => Err(CommandError::Bootstrap(BootstrapError::ChainIdRpc {
            message: e.to_string(),
        })),
    }
}

pub(crate) fn ensure_deployment_identity(
    db_path: &str,
    expected: DeploymentIdentity,
) -> Result<(), CommandError> {
    let mut storage = storage::Storage::open(db_path)?;
    if let Some(stored) = storage.deployment_identity()? {
        return require_deployment_identity_match(stored, expected);
    }
    if storage.has_persisted_deployment_state()? {
        return Err(IdentityError::OrphanedState.into());
    }
    let stored = storage.load_or_insert_deployment_identity(expected)?;
    require_deployment_identity_match(stored, expected)
}

fn require_deployment_identity_match(
    stored: DeploymentIdentity,
    expected: DeploymentIdentity,
) -> Result<(), CommandError> {
    let fields = deployment_identity_mismatch_fields(stored, expected);
    if fields.is_empty() {
        return Ok(());
    }
    Err(IdentityError::Mismatch {
        fields: fields.join(", "),
        stored: Box::new(stored),
        expected: Box::new(expected),
    }
    .into())
}

/// Keyed-writer preflight shared by `run` and `flush`: confirm a resolved
/// batch-submitter signing `key` signs for the submitter `setup` pinned in
/// `identity`, returning the key on success. Both subcommands broadcast keyed
/// L1 txs, so signing under the wrong key would consume the wrong wallet's
/// nonce slots — a fail-loud identity mismatch, not a recoverable condition.
pub(crate) fn verify_submitter_key(
    key: crate::l1::SubmitterKey,
    identity: &DeploymentIdentity,
) -> Result<crate::l1::SubmitterKey, CommandError> {
    let key_address = batch_submitter_address_from_private_key(key.expose_secret())?;
    if key_address != identity.batch_submitter_address {
        let expected = DeploymentIdentity {
            batch_submitter_address: key_address,
            ..*identity
        };
        require_deployment_identity_match(*identity, expected)?;
    }
    Ok(key)
}

fn deployment_identity_mismatch_fields(
    stored: DeploymentIdentity,
    expected: DeploymentIdentity,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if stored.chain_id != expected.chain_id {
        fields.push("chain_id");
    }
    if stored.app_address != expected.app_address {
        fields.push("app_address");
    }
    if stored.input_box_address != expected.input_box_address {
        fields.push("input_box_address");
    }
    if stored.app_deployment_block != expected.app_deployment_block {
        fields.push("app_deployment_block");
    }
    if stored.batch_submitter_address != expected.batch_submitter_address {
        fields.push("batch_submitter_address");
    }
    if stored.fee_oracle != expected.fee_oracle {
        fields.push("fee_oracle");
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::{
        BootstrapError, CommandError, IdentityError, batch_submitter_address_from_private_key,
        deployment_identity_mismatch_fields, ensure_deployment_identity,
        require_deployment_identity_match,
    };
    use crate::recovery::{RecoveryError, RecoveryRetryReason};
    use crate::storage::test_helpers::{SENDER_A, default_protocol_timing, temp_db};
    use crate::storage::{DeploymentIdentity, Storage};
    use alloy_primitives::Address;
    use sequencer_core::protocol::ProtocolTimingError;

    // Margin/stale-boundary validation is exercised directly in
    // `sequencer-core/src/protocol.rs`. The runtime tests below only cover
    // the typed `From` conversions into `CommandError` and the bootstrap-time
    // identity guards. Worker `From<JoinResult>` conversions live in
    // `run::workers`.

    #[test]
    fn previous_terminal_fault_read_is_verdict_neutral() {
        use super::warn_on_previous_terminal_fault;
        use crate::storage::LifecycleCommand;

        // No database at all: a warning, never an error or a panic.
        warn_on_previous_terminal_fault("/nonexistent/sequencer.db");

        let db = temp_db("previous-terminal-fault");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).expect("initialize");
        warn_on_previous_terminal_fault(&db.path); // empty black box
        storage
            .record_terminal_fault(LifecycleCommand::Run, "prior terminal death")
            .expect("record");
        warn_on_previous_terminal_fault(&db.path); // a row to report
        // (An unknown command or an empty cause cannot be seeded: the engine's
        // CHECK constraints refuse them, so the reader's malformed arm is
        // unreachable from a real database.)
    }

    #[test]
    fn settlement_write_records_terminal_verdicts_only() {
        use super::record_terminal_fault_best_effort;
        use crate::storage::LifecycleCommand;

        let db = temp_db("settlement-write");
        let storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).expect("initialize");
        drop(storage);

        // A non-terminal failure (transient class) leaves the black box empty.
        let transient: Result<(), CommandError> =
            Err(CommandError::Bootstrap(BootstrapError::ChainIdRpc {
                message: "provider unavailable".into(),
            }));
        record_terminal_fault_best_effort(&db.path, LifecycleCommand::Run, &transient);
        assert_eq!(
            Storage::open_read_only(&db.path)
                .expect("reopen")
                .latest_terminal_fault()
                .expect("read"),
            None
        );

        // A terminal verdict — the shape `finish` returns for a contained
        // fault — lands as one row carrying the error's Display form.
        let terminal: Result<(), CommandError> = Err(CommandError::StorageInvariantViolation {
            cause: "lane invariant broke".into(),
        });
        record_terminal_fault_best_effort(&db.path, LifecycleCommand::Run, &terminal);
        let fault = Storage::open_read_only(&db.path)
            .expect("reopen")
            .latest_terminal_fault()
            .expect("read")
            .expect("terminal verdict recorded");
        assert_eq!(fault.command, LifecycleCommand::Run);
        assert_eq!(
            fault.cause,
            "persistent storage invariant violation: lane invariant broke"
        );
    }

    #[test]
    fn invalid_protocol_config_propagates_through_run_error() {
        let err: CommandError = ProtocolTimingError::MarginNotLessThanMaxWait {
            margin: 1200,
            max_wait: 1200,
        }
        .into();
        assert!(matches!(
            err,
            CommandError::Bootstrap(BootstrapError::InvalidProtocolTiming(_))
        ));
    }

    #[test]
    fn startup_recovery_error_preserves_recovery_category() {
        let err: CommandError = RecoveryError::retry(RecoveryRetryReason::L1ViewStale).into();
        assert!(matches!(
            err,
            CommandError::Bootstrap(BootstrapError::Recovery(RecoveryError::Retry(_)))
        ));
    }

    fn identity() -> DeploymentIdentity {
        DeploymentIdentity {
            chain_id: 31337,
            app_address: Address::repeat_byte(0x11),
            input_box_address: Address::repeat_byte(0x22),
            app_deployment_block: 42,
            batch_submitter_address: Address::repeat_byte(0x33),
            fee_oracle: crate::storage::FeeOracleIdentity::Fixed { log_gas_price: 0 },
        }
    }

    #[test]
    fn deployment_identity_match_accepts_same_identity() {
        let identity = identity();
        require_deployment_identity_match(identity, identity).expect("same identity should match");
    }

    #[test]
    fn deployment_identity_mismatch_reports_changed_fields() {
        let stored = identity();
        let expected = DeploymentIdentity {
            chain_id: 31338,
            app_address: Address::repeat_byte(0x44),
            batch_submitter_address: Address::repeat_byte(0x55),
            ..stored
        };

        assert_eq!(
            deployment_identity_mismatch_fields(stored, expected),
            vec!["chain_id", "app_address", "batch_submitter_address"]
        );
        let err = require_deployment_identity_match(stored, expected)
            .expect_err("mismatch should refuse startup");
        assert!(matches!(
            err,
            CommandError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch { fields, .. }))
                if fields == "chain_id, app_address, batch_submitter_address"
        ));
    }

    #[test]
    fn deployment_identity_refuses_non_empty_unpinned_db() {
        let db = temp_db("runtime-unpinned-deployment-state");
        {
            let mut storage = Storage::open(db.path.as_str()).expect("open storage");
            storage
                .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
                .expect("seed deployment-bound state");
        }

        let err = ensure_deployment_identity(db.path.as_str(), identity())
            .expect_err("non-empty unpinned DB must refuse");
        assert!(matches!(
            err,
            CommandError::Bootstrap(BootstrapError::Identity(IdentityError::OrphanedState))
        ));
    }

    #[test]
    fn invalid_private_key_is_terminal_misconfig_and_does_not_echo_key_material() {
        let secret = "0xabc123SECRET";
        let err = batch_submitter_address_from_private_key(secret)
            .expect_err("invalid private key should be rejected");
        let message = err.to_string();

        assert!(
            matches!(
                err,
                CommandError::Bootstrap(BootstrapError::SignerMisconfig { .. })
            ),
            "a malformed key is deterministic operator misconfig (terminal), got {err:?}"
        );
        assert_eq!(err.exit_code(), crate::commands::error::EXIT_TERMINAL);
        assert!(
            !message.contains(secret),
            "private key material must not be reflected in startup errors"
        );
    }
}
