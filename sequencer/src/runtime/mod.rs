// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Process orchestration. Three phases:
//!
//! 1. **Bootstrap**: parse config, validate identity, build the L1 config.
//! 2. **Preemptive recovery**: run the startup recovery procedure
//!    ([`crate::recovery::run_preemptive_recovery`]).
//! 3. **Workers**: hand off to `workers::Workers` for spawn → select →
//!    finish.
//!
//! Errors live in [`error`]; worker lifecycle in `workers`.

pub mod clock;
pub mod config;
pub mod error;
pub mod shutdown;
mod workers;

use std::time::Duration;

use crate::l1::reader::{InputReader, InputReaderConfig, InputReaderError};
use crate::storage::{self, DeploymentIdentity};
use alloy_primitives::Address;
use config::{L1Config, RunConfig};
use sequencer_core::application::Application;
use sequencer_core::protocol::ProtocolTiming;

pub use error::{
    BatchSubmitterExit, BootstrapError, DangerDetectorExit, IdentityError, InputReaderExit,
    LaneExit, RunError, ServerExit, WorkerExit,
};

use workers::{Workers, WorkersConfig};

const INPUT_READER_POLL_INTERVAL: Duration = Duration::from_secs(2);

pub async fn run<A>(app: A, config: RunConfig) -> Result<(), RunError>
where
    A: Application + 'static,
{
    // ── Bootstrap ────────────────────────────────────────────
    std::fs::create_dir_all(&config.data_dir)?;
    let db_path = config.db_path();
    let key = config.resolve_private_key()?;
    let batch_submitter_address = batch_submitter_address_from_private_key(&key)?;

    // One ProtocolTiming shared across the whole process. `try_new` validates
    // margin/stale relationships up front — including before the startup log
    // below — so a bad config produces a clean typed error instead of
    // panicking mid-log.
    let timing = config.protocol_timing()?;

    let (mut input_reader, l1_config) = bootstrap_l1_config(
        &config,
        db_path.as_str(),
        timing,
        batch_submitter_address,
        key,
    )
    .await?;

    tracing::info!(
        http_addr = %config.http_addr,
        data_dir = %config.data_dir,
        eth_rpc_url = %l1_config.eth_rpc_url,
        input_box_address = %l1_config.input_box_address,
        input_reader_genesis_block = input_reader.genesis_block(),
        chain_id = config.chain_id,
        app_address = %l1_config.app_address,
        batch_submitter_address = %l1_config.batch_submitter_address,
        max_wait_blocks = timing.max_wait_blocks,
        preemptive_margin_blocks = timing.preemptive_margin_blocks,
        danger_threshold = timing.danger_threshold(),
        "sequencer startup"
    );

    // ── Preemptive recovery ──────────────────────────────────
    // See docs/recovery/ for the full design and TLA+ spec.
    crate::recovery::run_preemptive_recovery(&db_path, &mut input_reader, &l1_config, &timing)
        .await?;

    // ── Workers ──────────────────────────────────────────────
    let mut workers = Workers::spawn(WorkersConfig {
        app,
        run_config: config,
        l1_config,
        timing,
        input_reader,
    })
    .await?;

    let first_exit = workers.select_first_exit().await;
    workers.finish(first_exit).await
}

// ── Bootstrap helpers ──────────────────────────────────────────────────

fn batch_submitter_address_from_private_key(private_key: &str) -> Result<Address, RunError> {
    use alloy::signers::local::PrivateKeySigner;
    use std::str::FromStr;

    Ok(PrivateKeySigner::from_str(private_key)
        .map_err(|_| RunError::Io(std::io::Error::other("invalid private key")))?
        .address())
}

/// Resolve `(InputReader, L1Config)` from the configured RPC, falling back to
/// the DB-pinned deployment identity when L1 is unreachable.
///
/// On first startup, L1 is required (no cached identity to fall back on). On
/// subsequent startups, the identity allows the sequencer to start without L1
/// while refusing to interpret the DB under a different deployment.
///
/// The genesis block is available via `input_reader.genesis_block()` on the
/// returned reader.
async fn bootstrap_l1_config(
    config: &RunConfig,
    db_path: &str,
    timing: ProtocolTiming,
    batch_submitter_address: Address,
    batch_submitter_private_key: String,
) -> Result<(InputReader, L1Config), RunError> {
    let input_reader_config = InputReaderConfig {
        rpc_url: config.eth_rpc_url.clone(),
        app_address: config.app_address,
        poll_interval: INPUT_READER_POLL_INTERVAL,
        long_block_range_error_codes: config.long_block_range_error_codes.clone(),
    };

    let (input_reader, input_box_address) = match InputReader::new(
        db_path.to_owned(),
        input_reader_config.clone(),
        batch_submitter_address,
        timing,
    )
    .await
    {
        Ok(reader) => {
            let input_box = reader.input_box_address();

            // Validate chain ID early — before any DB identity writes.
            validate_rpc_chain_id(&config.eth_rpc_url, config.chain_id).await?;

            let expected_identity = DeploymentIdentity {
                chain_id: config.chain_id,
                app_address: config.app_address,
                input_box_address: input_box,
                input_box_genesis_block: reader.genesis_block(),
                batch_submitter_address,
            };
            ensure_deployment_identity(db_path, expected_identity)?;

            (reader, input_box)
        }
        Err(InputReaderError::Provider(e)) => {
            tracing::error!(
                error = %e,
                "L1 unreachable during bootstrap — checking deployment identity"
            );
            let cached = cached_deployment_identity(
                db_path,
                config.chain_id,
                config.app_address,
                batch_submitter_address,
            )?;
            let reader = InputReader::from_parts(
                input_reader_config,
                cached.input_box_address,
                cached.input_box_genesis_block,
                db_path.to_owned(),
                batch_submitter_address,
                timing,
            );
            (reader, cached.input_box_address)
        }
        Err(source) => {
            // L1 reachable but `InputReader::new` failed for a non-provider
            // reason — wrap as a startup-time worker source error.
            return Err(RunError::Worker(WorkerExit::InputReader(
                InputReaderExit::Source(source),
            )));
        }
    };

    let l1_config = L1Config {
        eth_rpc_url: config.eth_rpc_url.clone(),
        input_box_address,
        app_address: config.app_address,
        batch_submitter_private_key,
        batch_submitter_address,
    };
    Ok((input_reader, l1_config))
}

/// Verify that the RPC's `eth_chainId` matches the configured chain id.
///
/// Treated as fatal on mismatch *and* on RPC error: pinning a wrong or
/// unverified chain id into storage would poison subsequent L1-unreachable
/// boots and issue soft confirmations against the wrong chain. Caller is
/// expected to retry on `ChainIdRpc`.
async fn validate_rpc_chain_id(eth_rpc_url: &str, expected: u64) -> Result<(), RunError> {
    use alloy::providers::Provider;
    let check_provider = crate::l1::provider::create_provider(eth_rpc_url)
        .map_err(|e| RunError::Io(std::io::Error::other(e)))?;
    match check_provider.get_chain_id().await {
        Ok(rpc_chain_id) if rpc_chain_id != expected => {
            Err(RunError::Bootstrap(BootstrapError::ChainIdMismatch {
                rpc: rpc_chain_id,
                config: expected,
            }))
        }
        Ok(_) => Ok(()),
        Err(e) => Err(RunError::Bootstrap(BootstrapError::ChainIdRpc {
            message: e.to_string(),
        })),
    }
}

fn ensure_deployment_identity(db_path: &str, expected: DeploymentIdentity) -> Result<(), RunError> {
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

fn cached_deployment_identity(
    db_path: &str,
    chain_id: u64,
    app_address: Address,
    batch_submitter_address: Address,
) -> Result<DeploymentIdentity, RunError> {
    let storage = storage::Storage::open(db_path)?;
    let Some(stored) = storage.deployment_identity()? else {
        return Err(IdentityError::FirstBootRequiresL1.into());
    };
    let expected = DeploymentIdentity {
        chain_id,
        app_address,
        input_box_address: stored.input_box_address,
        input_box_genesis_block: stored.input_box_genesis_block,
        batch_submitter_address,
    };
    require_deployment_identity_match(stored, expected)?;
    Ok(stored)
}

fn require_deployment_identity_match(
    stored: DeploymentIdentity,
    expected: DeploymentIdentity,
) -> Result<(), RunError> {
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
    if stored.input_box_genesis_block != expected.input_box_genesis_block {
        fields.push("input_box_genesis_block");
    }
    if stored.batch_submitter_address != expected.batch_submitter_address {
        fields.push("batch_submitter_address");
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::{
        BootstrapError, IdentityError, RunError, batch_submitter_address_from_private_key,
        deployment_identity_mismatch_fields, ensure_deployment_identity,
        require_deployment_identity_match,
    };
    use crate::recovery::{RecoveryError, RefuseReason};
    use crate::storage::test_helpers::{SENDER_A, default_protocol_timing, temp_db};
    use crate::storage::{DeploymentIdentity, Storage};
    use alloy_primitives::Address;
    use sequencer_core::protocol::ProtocolTimingError;

    // Margin/stale-boundary validation is exercised directly in
    // `sequencer-core/src/protocol.rs`. The runtime tests below only cover
    // the typed `From` conversions into `RunError` and the bootstrap-time
    // identity guards. Worker `From<JoinResult>` conversions live in
    // `runtime/workers.rs`.

    #[test]
    fn invalid_protocol_config_propagates_through_run_error() {
        let err: RunError = ProtocolTimingError::MarginNotLessThanMaxWait {
            margin: 1200,
            max_wait: 1200,
        }
        .into();
        assert!(matches!(
            err,
            RunError::Bootstrap(BootstrapError::InvalidProtocolTiming(_))
        ));
    }

    #[test]
    fn startup_recovery_error_preserves_recovery_category() {
        let err: RunError = RecoveryError::Refuse(RefuseReason::L1ViewStale).into();
        assert!(matches!(
            err,
            RunError::Bootstrap(BootstrapError::Recovery(RecoveryError::Refuse(
                RefuseReason::L1ViewStale
            )))
        ));
    }

    fn identity() -> DeploymentIdentity {
        DeploymentIdentity {
            chain_id: 31337,
            app_address: Address::repeat_byte(0x11),
            input_box_address: Address::repeat_byte(0x22),
            input_box_genesis_block: 42,
            batch_submitter_address: Address::repeat_byte(0x33),
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
            RunError::Bootstrap(BootstrapError::Identity(IdentityError::Mismatch { fields, .. }))
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
            RunError::Bootstrap(BootstrapError::Identity(IdentityError::OrphanedState))
        ));
    }

    #[test]
    fn invalid_private_key_error_does_not_echo_key_material() {
        let secret = "0xabc123SECRET";
        let err = batch_submitter_address_from_private_key(secret)
            .expect_err("invalid private key should be rejected");
        let message = err.to_string();

        assert_eq!(message, "invalid private key");
        assert!(
            !message.contains(secret),
            "private key material must not be reflected in startup errors"
        );
    }
}
