// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Process orchestration for the `run` subcommand, plus the shared
//! bootstrap helpers used by all three subcommands.
//!
//! The phase split: [`setup`] establishes the timeless deployment
//! state (identity, initial sync, genesis snapshot, `setup_complete` marker);
//! [`run`] boots workers from an already-set-up DB; [`flush`] settles the
//! wallet nonce. The CLI harness that dispatches them lives in
//! [`crate::harness`].
//!
//! `run`'s phases:
//!
//! 1. **Gate + identity**: refuse unless `setup` completed; read the pinned
//!    deployment identity from the DB (chain id / app address are no longer
//!    CLI args — they come from the identity).
//! 2. **Preemptive recovery**: run the startup recovery procedure
//!    ([`crate::recovery::run_preemptive_recovery`]).
//! 3. **Workers**: hand off to `workers::Workers` for spawn → select →
//!    finish.
//!
//! Errors live in [`error`]; worker lifecycle in `workers`.

pub mod clock;
pub mod config;
pub mod error;
pub mod flush;
pub mod setup;
mod setup_fill;
pub mod shutdown;
#[cfg(test)]
pub(crate) mod test_support;
mod workers;

use std::time::Duration;

use crate::l1::reader::{InputReader, InputReaderConfig};
use crate::storage::{self, DeploymentIdentity};
use alloy_primitives::Address;
use config::{L1Config, RunConfig};
use sequencer_core::application::Application;

pub use error::{
    BatchSubmitterExit, BootstrapError, DangerDetectorExit, IdentityError, InputReaderExit,
    LaneExit, RunError, ServerExit, SetupRecoveryError, SetupRefuse, WorkerExit,
};

use workers::{Workers, WorkersConfig};

pub(crate) const INPUT_READER_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Boot the sequencer from an already-set-up DB. Generic over the app type
/// (for the lane's `from_dump`, the egress state-file path, and the
/// max-payload bound) but takes no app *value* — `setup` already registered
/// the genesis snapshot, so the lane reloads via `A::from_dump`.
pub async fn run<A>(config: RunConfig) -> Result<(), RunError>
where
    A: Application + Clone + Sync + 'static,
{
    // ── Gate + identity ──────────────────────────────────────
    std::fs::create_dir_all(&config.data_dir)?;
    let db_path = config.db_path();
    let timing = config.protocol_timing()?;

    // Refuse to boot unless `setup` completed; the identity it pinned
    // supplies chain id / app address / InputBox address / app deployment block /
    // submitter address — none of which are CLI args on `run`.
    let identity = load_setup_identity(&db_path)?;

    // `run` holds the signing key (it submits). The key's address must match
    // the pinned submitter address — running with the wrong key against a DB
    // pinned to another submitter is a fail-loud identity mismatch.
    let key = verify_submitter_key(config.resolve_private_key()?, &identity)?;

    // Validate the RPC chain id against the *pinned* chain id when L1 is
    // reachable (guards against a wrong-chain RPC after setup, review F6);
    // tolerate an unreachable L1 (warm boot — identity is already pinned). The
    // tolerated case is backstopped by the input reader, which re-verifies the
    // chain id on its first successful contact (`InputReaderConfig::expected_chain_id`),
    // so a provider that reconnects on the wrong chain fails loud before
    // ingesting any address-filtered foreign logs.
    match validate_rpc_chain_id(
        &config.eth_rpc_url,
        identity.chain_id,
        config.allow_insecure_rpc,
    )
    .await
    {
        Ok(()) => {}
        Err(RunError::Bootstrap(BootstrapError::ChainIdRpc { message })) => {
            tracing::warn!(
                error = %message,
                "L1 unreachable at boot — continuing from pinned deployment identity"
            );
        }
        Err(other) => return Err(other),
    }

    let l1_config = L1Config {
        eth_rpc_url: config.eth_rpc_url.clone(),
        input_box_address: identity.input_box_address,
        app_address: identity.app_address,
        batch_submitter_private_key: key,
        batch_submitter_address: identity.batch_submitter_address,
        chain_id: identity.chain_id,
        allow_insecure_rpc: config.allow_insecure_rpc,
    };

    // `run` never re-discovers identity from L1 — it builds the reader from
    // the pinned InputBox address + app deployment block and syncs incrementally.
    let mut input_reader = InputReader::from_parts(
        InputReaderConfig {
            rpc_url: config.eth_rpc_url.clone(),
            allow_insecure_rpc: config.allow_insecure_rpc,
            app_address: identity.app_address,
            poll_interval: INPUT_READER_POLL_INTERVAL,
            long_block_range_error_codes: config.long_block_range_error_codes.clone(),
            expected_chain_id: identity.chain_id,
        },
        identity.input_box_address,
        identity.app_deployment_block,
        db_path.clone(),
        identity.batch_submitter_address,
        timing,
    );

    tracing::info!(
        http_addr = %config.http_addr,
        data_dir = %config.data_dir,
        eth_rpc_url = %l1_config.eth_rpc_url,
        input_box_address = %l1_config.input_box_address,
        app_deployment_block = input_reader.app_deployment_block(),
        chain_id = identity.chain_id,
        app_address = %l1_config.app_address,
        batch_submitter_address = %l1_config.batch_submitter_address,
        max_wait_blocks = timing.max_wait_blocks,
        preemptive_margin_blocks = timing.preemptive_margin_blocks,
        danger_threshold = timing.danger_threshold(),
        "sequencer startup"
    );

    // Always-load invariant, checked at the gate (before any recovery write):
    // setup registers the genesis finalized snapshot, so a marker-present DB
    // with no snapshot is a corrupt/incomplete setup. Fail loud here — ahead
    // of preemptive recovery's DB mutations — rather than only at the lane.
    {
        let mut storage = storage::Storage::open(&db_path)?;
        if storage.finalized_dump()?.is_none() {
            return Err(BootstrapError::SetupNotComplete.into());
        }
    }

    // ── Preemptive recovery ──────────────────────────────────
    // See docs/recovery/ for the full design and TLA+ spec.
    crate::recovery::run_preemptive_recovery(&db_path, &mut input_reader, &l1_config, &timing)
        .await?;

    // ── Workers ──────────────────────────────────────────────
    let domain = sequencer_core::build_input_domain(identity.chain_id, identity.app_address);
    let mut workers = Workers::spawn::<A>(WorkersConfig {
        run_config: config,
        l1_config,
        timing,
        input_reader,
        domain,
    })
    .await?;

    let first_exit = workers.select_first_exit().await;
    workers.finish(first_exit).await
}

// ── Bootstrap helpers (shared by setup / run / flush) ──────────────────

pub(crate) fn batch_submitter_address_from_private_key(
    private_key: &str,
) -> Result<Address, RunError> {
    use alloy::signers::local::PrivateKeySigner;
    use std::str::FromStr;

    Ok(PrivateKeySigner::from_str(private_key)
        .map_err(|_| RunError::Io(std::io::Error::other("invalid private key")))?
        .address())
}

/// Gate `run`/`flush` on a completed `setup` and return the pinned identity.
/// A missing marker — or a marker present but no identity (a corrupt /
/// incomplete setup) — is a terminal `SetupNotComplete`: the operator must
/// (re-)run `setup`, not retry `run`.
pub(crate) fn load_setup_identity(db_path: &str) -> Result<DeploymentIdentity, RunError> {
    let storage = storage::Storage::open(db_path)?;
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
) -> Result<(), RunError> {
    use alloy::providers::Provider;
    let check_provider = crate::l1::provider::create_provider(eth_rpc_url, allow_insecure)
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

pub(crate) fn ensure_deployment_identity(
    db_path: &str,
    expected: DeploymentIdentity,
) -> Result<(), RunError> {
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

/// Keyed-writer preflight shared by `run` and `flush`: confirm a resolved
/// batch-submitter signing `key` signs for the submitter `setup` pinned in
/// `identity`, returning the key on success. Both subcommands broadcast keyed
/// L1 txs, so signing under the wrong key would consume the wrong wallet's
/// nonce slots — a fail-loud identity mismatch, not a recoverable condition.
pub(crate) fn verify_submitter_key(
    key: String,
    identity: &DeploymentIdentity,
) -> Result<String, RunError> {
    let key_address = batch_submitter_address_from_private_key(&key)?;
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
            app_deployment_block: 42,
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
