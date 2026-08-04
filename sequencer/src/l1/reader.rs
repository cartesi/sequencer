// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Reads safe InputBox events from L1 and appends them to sequencer storage.
//! Minimal design: no epochs or consensus; flat contiguous indices only.

use std::time::Duration;

use alloy::eips::BlockNumberOrTag::Safe;
use alloy::providers::Provider;
use alloy::sol_types::SolInterface;
use alloy_primitives::{Address, U256};
use cartesi_rollups_contracts::application::Application;
use cartesi_rollups_contracts::data_availability::DataAvailability::{
    DataAvailabilityCalls, InputBoxAndEspressoCall, InputBoxCall,
};
use cartesi_rollups_contracts::input_box::InputBox;
use cartesi_rollups_contracts::input_box::InputBox::InputAdded;
use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::l1::partition::{
    GetLogsError, decode_evm_advance_input, get_input_added_events_ordered,
};
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{FrontierMode, IngestedSafeInput, Storage, StorageOpenError};
use sequencer_core::protocol::ProtocolTiming;

#[derive(Debug, Clone)]
pub struct InputReaderConfig {
    pub rpc_url: String,
    /// Opt into plaintext (`http://`) RPC against a non-loopback host — a
    /// trusted private network (Docker/K8s service, private-VPC IP). Off by
    /// default: the provider layer refuses remote plaintext otherwise. See
    /// [`crate::l1::provider`].
    pub allow_insecure_rpc: bool,
    pub app_address: Address,
    pub poll_interval: Duration,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    pub long_block_range_error_codes: Vec<String>,
    /// The chain id `setup` pinned (`run`) / is pinning (`setup`). The reader
    /// verifies the provider actually serves this chain on its first successful
    /// contact — the RPC URL is an operator CLI/env arg that may be repointed
    /// across restarts (token rotation, provider swap), so `run` cannot assume
    /// it still matches the pinned identity. This backstops the boot-time check
    /// ([`crate::runtime::validate_rpc_chain_id`]), which is skipped when L1 is
    /// unreachable at boot.
    pub expected_chain_id: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum InputReaderError {
    /// Could not reach the provider (transport failure). Retryable: the run
    /// loop logs and re-ticks.
    #[error("provider/transport: {0}")]
    Provider(String),
    /// The provider answered, but with data that contradicts itself or the
    /// InputBox's own accounting: an index gap or count mismatch (the F5
    /// witnesses), missing log provenance, an undecodable or self-inconsistent
    /// `EvmAdvance` payload. Retryable — a consistent node returns a coherent
    /// set on the next tick — but logged as its own alarm: a *persistent* one
    /// means the endpoint is broken or lying, not slow.
    #[error("inconsistent L1 response: {0}")]
    InconsistentL1Response(String),
    /// The provider serves a different chain than the pinned identity.
    /// Terminal — reconfiguration, not time, fixes it.
    #[error("RPC chain id {rpc} does not match the pinned chain id {expected}")]
    ChainIdMismatch { rpc: u64, expected: u64 },
    /// Deterministic deployment/config mismatch discovered at bootstrap (bad
    /// RPC URL, wrong contract, pre-v3 InputBox). Terminal — re-running
    /// against the same configuration re-fails identically.
    #[error("bootstrap: {0}")]
    Bootstrap(String),
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("input reader join error: {0}")]
    Join(String),
}

impl InputReaderError {
    /// Transient conditions the run loop logs and retries; everything else
    /// exits the worker. Exhaustive on purpose — a new variant must be
    /// classified here, not silently inherit "terminal".
    fn is_retryable(&self) -> bool {
        match self {
            Self::Provider(_) | Self::InconsistentL1Response(_) => true,
            Self::ChainIdMismatch { .. }
            | Self::Bootstrap(_)
            | Self::OpenStorage(_)
            | Self::Storage(_)
            | Self::Join(_) => false,
        }
    }
}

pub struct InputReader {
    config: InputReaderConfig,
    input_box_address: Address,
    /// The application's deployment block — the first block that can contain
    /// an `InputAdded` event for this app. Sound as the scan floor because the
    /// v3 InputBox reverts `addInput` for an app contract that does not exist
    /// yet (`ApplicationNotDeployed`); [`Self::new`] verifies the InputBox is
    /// v3+ before trusting this bound.
    app_deployment_block: u64,
    db_path: String,
    /// Scheduler-acceptance identity — passed into
    /// [`Storage::append_safe_inputs`] so the persisted scheduler-accepted
    /// frontier filters by the right sender.
    batch_submitter: Address,
    /// Protocol timing used to keep `safe_accepted_batches` consistent with
    /// every `append_safe_inputs` write.
    timing: ProtocolTiming,
    /// Whether each sync also populates the scheduler-accepted frontier
    /// ([`FrontierMode::Populate`] normally; `setup --recovery` defers it — see
    /// [`Self::set_frontier_mode`]).
    frontier_mode: FrontierMode,
    /// Set once the provider's chain id has been verified against
    /// [`InputReaderConfig::expected_chain_id`] on a successful RPC contact.
    /// Until then every `advance_once` re-attempts the check, so an unreachable
    /// L1 keeps retrying and the verification fires the moment L1 returns.
    chain_id_verified: bool,
}

impl InputReader {
    pub async fn new(
        db_path: impl Into<String>,
        config: InputReaderConfig,
        batch_submitter: Address,
        timing: ProtocolTiming,
    ) -> Result<Self, InputReaderError> {
        let provider =
            crate::l1::provider::create_provider(&config.rpc_url, config.allow_insecure_rpc)
                .map_err(InputReaderError::Bootstrap)?;
        let application = Application::new(config.app_address, &provider);
        let data_availability = application
            .getDataAvailability()
            .call()
            .await
            .map_err(map_contract_bootstrap_error)?;
        let input_box_address = decode_input_box_address(&data_availability)?;

        // The scan genesis is the *application's* deployment block, not the
        // InputBox's: the InputBox usually predates the app by a large block
        // gap, and scanning that gap degenerates into a long paginated
        // `get_logs` crawl on range-capped providers. Skipping it is sound
        // only because the v3 InputBox refuses inputs for apps that do not
        // exist yet — witnessed below via `version()`.
        let app_deployment_block = application
            .getDeploymentBlockNumber()
            .call()
            .await
            .map_err(map_contract_bootstrap_error)?
            .try_into()
            .map_err(|_| {
                InputReaderError::Bootstrap(
                    "application deployment block number did not fit into u64".to_string(),
                )
            })?;

        let input_box = InputBox::new(input_box_address, &provider);
        verify_input_box_v3(&input_box).await?;

        Ok(Self::from_parts(
            config,
            input_box_address,
            app_deployment_block,
            db_path.into(),
            batch_submitter,
            timing,
        ))
    }

    pub fn from_parts(
        config: InputReaderConfig,
        input_box_address: Address,
        app_deployment_block: u64,
        db_path: String,
        batch_submitter: Address,
        timing: ProtocolTiming,
    ) -> Self {
        Self {
            config,
            input_box_address,
            app_deployment_block,
            db_path,
            batch_submitter,
            timing,
            frontier_mode: FrontierMode::Populate,
            chain_id_verified: false,
        }
    }

    /// Set whether subsequent syncs populate the scheduler-accepted frontier.
    /// `setup --recovery` sets [`FrontierMode::DeferUntilAnchorSet`] for the
    /// syncs that feed the fold (the frontier is populated by `run`'s first
    /// sync, once the anchor = `N'`).
    pub fn set_frontier_mode(&mut self, mode: FrontierMode) {
        self.frontier_mode = mode;
    }

    pub fn input_box_address(&self) -> Address {
        self.input_box_address
    }

    pub fn app_deployment_block(&self) -> u64 {
        self.app_deployment_block
    }

    /// Spawn the worker loop. The `shutdown` signal is what the loop respects;
    /// passing it at start time (instead of construction time) keeps the
    /// construction phase pure and ensures the same instance can't accidentally
    /// be started under two different shutdown signals.
    pub fn start(
        self,
        shutdown: ShutdownSignal,
    ) -> Result<JoinHandle<Result<(), InputReaderError>>, StorageOpenError> {
        let _ = Storage::open(self.db_path.as_str())?;
        Ok(tokio::spawn(
            async move { self.run_forever(shutdown).await },
        ))
    }

    pub async fn sync_to_current_safe_head(&mut self) -> Result<(), InputReaderError> {
        let provider = crate::l1::provider::create_provider(
            &self.config.rpc_url,
            self.config.allow_insecure_rpc,
        )
        .map_err(InputReaderError::Bootstrap)?;
        self.advance_once(&provider).await
    }

    /// Top-level driver. Races the work loop against the shutdown signal.
    ///
    /// `biased;` polls the shutdown arm first on every wakeup so a concurrent
    /// shutdown wins over an in-flight `run_loop` step. Without `biased`,
    /// `select!` would pick randomly between two ready branches and could
    /// process one more iteration before shutting down.
    async fn run_forever(self, shutdown: ShutdownSignal) -> Result<(), InputReaderError> {
        tokio::select! {
            biased;
            _ = shutdown.wait_for_shutdown() => Ok(()),
            result = self.run_loop() => result,
        }
    }

    /// Tick → sleep → tick. Provider errors are logged and retried; other
    /// errors propagate. Shutdown is handled by the outer `run_forever`
    /// select, so this loop has no shutdown concerns.
    async fn run_loop(mut self) -> Result<(), InputReaderError> {
        let provider = crate::l1::provider::create_provider(
            &self.config.rpc_url,
            self.config.allow_insecure_rpc,
        )
        .map_err(InputReaderError::Bootstrap)?;
        loop {
            match self.advance_once(&provider).await {
                Ok(()) => {}
                Err(err @ InputReaderError::InconsistentL1Response(_)) => {
                    tracing::error!(
                        error = %err,
                        "L1 provider served an inconsistent response — will retry; \
                         if persistent, the RPC endpoint is the suspect"
                    );
                }
                Err(err) if err.is_retryable() => {
                    tracing::error!(error = %err, "L1 provider error in input reader — will retry");
                }
                Err(err) => return Err(err),
            }
            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    pub(crate) async fn advance_once(
        &mut self,
        provider: &impl Provider,
    ) -> Result<(), InputReaderError> {
        // Verify the provider serves the pinned chain before reading anything
        // from it — a wrong-chain RPC's address-filtered logs would otherwise
        // flow into `safe_inputs`.
        self.verify_chain_id(provider).await?;

        let current_safe_head = latest_safe_head(provider).await?;
        let current_safe_block = current_safe_head.block_number;
        let (previous_safe_block, expected_start) = self.scan_cursor().await?;

        // The next block to scan: one past the persisted safe head, or the
        // app's deployment block on the very first sync (no input can exist
        // before it).
        let start_block = previous_safe_block.map_or(self.app_deployment_block, |b| b + 1);

        // Nothing new to scan. On the first observation we still persist the
        // real safe head so storage distinguishes "observed L1" from "no L1
        // view yet".
        if current_safe_block < start_block {
            return match previous_safe_block {
                None => self.append_safe_inputs(current_safe_head, Vec::new()).await,
                Some(_) => Ok(()),
            };
        }

        // F5 completeness witness, fetched *before* the scan. Pinning the count
        // to `current_safe_block` (not latest) keeps it consistent with the
        // scanned range and forces the serving node to actually have that
        // block's state. Fetching it up front (rather than after `get_logs`,
        // where the check consumes it) additionally lets the scan be skipped
        // outright when the span holds no new inputs — see below.
        let onchain_count = input_count_at_block(
            provider,
            &self.input_box_address,
            self.config.app_address,
            current_safe_block,
        )
        .await?;

        // No new inputs in `[start_block, current_safe_block]` — advance the
        // safe head without scanning. On range-capped providers the skipped
        // `get_logs` crawl is hundreds of paginated queries for a long span
        // (first sync of a quiet app, restart after downtime), while the count
        // above is a single call. Strict equality only: a count *below* what we
        // hold is an impossible overcount and must fall through to the
        // completeness check, which fails loud on it.
        if onchain_count == expected_start {
            debug!(
                block_range = %format!("{}..={}", start_block, current_safe_block),
                "no new inputs — advancing safe head without scanning"
            );
            return self.append_safe_inputs(current_safe_head, Vec::new()).await;
        }

        // The dense-index contiguity witness below requires L1 event order.
        // `get_input_added_events_ordered` guarantees it — a raw `eth_getLogs`
        // reorder would otherwise turn the check into a permanent retry loop.
        // The InputBox `index` is monotonic with L1 order, so the sorted
        // stream aligns the on-chain indices with `expected_start + i`.
        let events = get_input_added_events_ordered(
            provider,
            self.config.app_address,
            &self.input_box_address,
            start_block,
            current_safe_block,
            self.config.long_block_range_error_codes.as_slice(),
        )
        .await
        .map_err(|err| match err {
            GetLogsError::Rpc(e) => {
                InputReaderError::Provider(format!("get_input_added_events: {e}"))
            }
            inconsistent @ GetLogsError::MissingBlockNumber { .. } => {
                InputReaderError::InconsistentL1Response(inconsistent.to_string())
            }
        })?;

        // F5: the InputBox assigns every input a per-app, gap-free `index`. We
        // ingest every event for this app from genesis, so each input's on-chain
        // index must equal the dense local `safe_input_index` it is assigned
        // (= the running count of already-stored inputs). Collect the on-chain
        // indices and verify contiguity below, *before* persisting — a gap means
        // the provider returned an incomplete `get_logs` set (a clamped or
        // lagging-replica response), and advancing the safe head past a missing
        // input would let the scheduler force-drain it while we never saw it.
        let mut batch = Vec::with_capacity(events.len());
        let mut onchain_indices = Vec::with_capacity(events.len());
        for (event, log) in events {
            let (onchain_index, input) = ingest_input_added(&event, &log)?;
            onchain_indices.push(onchain_index);
            batch.push(input);
        }

        // Fail loud on a hole rather than persist a partial set (retryable: a
        // consistent provider returns the full set on the next tick).
        check_input_index_contiguity(expected_start, &onchain_indices)?;

        // Completeness witness (count fetched above, pinned at the scanned safe
        // block). Contiguity above proves the returned rows are the right
        // *prefix*; this proves the prefix is *complete*. A truncated
        // `get_logs` (still a contiguous prefix) shows up as an InputBox input
        // count that exceeds what we received. Without it, a dropped tail
        // deposit `≤` the safe head would be persisted as complete and only
        // caught on the *next* input, after the lane may have stamped frames
        // past it (divergence).
        let received_total = expected_start.saturating_add(batch.len() as u64);
        check_input_count_complete(current_safe_block, received_total, onchain_count)?;

        info!(
            block_range = %format!("{}..={}", start_block, current_safe_block),
            count = batch.len(),
            "appending safe inputs"
        );

        self.append_safe_inputs(current_safe_head, batch).await
    }

    /// Verify the provider serves the pinned chain id, once per reader instance,
    /// on the first successful contact. A transport failure is retryable
    /// (`Provider`) and leaves the flag unset, so the check re-fires on the next
    /// tick until L1 is reachable; a value mismatch is fatal (`ChainIdMismatch`)
    /// and propagates out of the run loop / aborts a recovery sync. This is the
    /// backstop for `run`'s boot-time check, which is skipped when L1 is
    /// unreachable at boot — without it, an RPC reconnecting on the wrong chain
    /// would ingest address-filtered foreign logs unnoticed.
    async fn verify_chain_id(&mut self, provider: &impl Provider) -> Result<(), InputReaderError> {
        if self.chain_id_verified {
            return Ok(());
        }
        let rpc_chain_id = provider
            .get_chain_id()
            .await
            .map_err(|e| InputReaderError::Provider(e.to_string()))?;
        if rpc_chain_id != self.config.expected_chain_id {
            return Err(InputReaderError::ChainIdMismatch {
                rpc: rpc_chain_id,
                expected: self.config.expected_chain_id,
            });
        }
        self.chain_id_verified = true;
        Ok(())
    }

    /// Test-only convenience; production reads go through [`Self::scan_cursor`].
    #[cfg(test)]
    async fn current_safe_block(&self) -> Result<Option<u64>, InputReaderError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path)?;
            storage.current_safe_block().map_err(InputReaderError::from)
        })
        .await
        .map_err(|err| InputReaderError::Join(err.to_string()))?
    }

    /// Both scan-cursor reads in one connection: the persisted safe head
    /// (`None` before the first observation) and the count of already-stored
    /// safe inputs (= the next local `safe_input_index`, the expected on-chain
    /// index of the next ingested input — F5). Reading them together is safe:
    /// this reader is the sole writer of both, and `advance_once` holds
    /// `&mut self` on a single-task loop.
    async fn scan_cursor(&self) -> Result<(Option<u64>, u64), InputReaderError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path)?;
            Ok((
                storage.current_safe_block()?,
                storage.safe_input_end_exclusive()?,
            ))
        })
        .await
        .map_err(|err| InputReaderError::Join(err.to_string()))?
    }

    async fn append_safe_inputs(
        &self,
        current_safe_head: SafeHead,
        batch: Vec<IngestedSafeInput>,
    ) -> Result<(), InputReaderError> {
        let db_path = self.db_path.clone();
        let batch_submitter = self.batch_submitter;
        let timing = self.timing;
        let frontier_mode = self.frontier_mode;
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open(&db_path)?;
            storage
                .append_ingested_safe_inputs_with_timestamp(
                    current_safe_head.block_number,
                    current_safe_head.block_timestamp,
                    &batch,
                    batch_submitter,
                    &timing,
                    frontier_mode,
                )
                .map_err(InputReaderError::from)
        })
        .await
        .map_err(|err| InputReaderError::Join(err.to_string()))?
    }
}

fn decode_input_box_address(data_availability: &[u8]) -> Result<Address, InputReaderError> {
    let call = DataAvailabilityCalls::abi_decode(data_availability).map_err(|err| {
        InputReaderError::Bootstrap(format!(
            "application getDataAvailability returned invalid DataAvailability calldata: {err}"
        ))
    })?;

    match call {
        DataAvailabilityCalls::InputBox(InputBoxCall { inputBox }) => Ok(inputBox),
        DataAvailabilityCalls::InputBoxAndEspresso(InputBoxAndEspressoCall {
            inputBox,
            fromBlock,
            namespaceId,
        }) => Err(InputReaderError::Bootstrap(format!(
            "application getDataAvailability returned unsupported DataAvailability.InputBoxAndEspresso(inputBox={inputBox}, fromBlock={fromBlock}, namespaceId={namespaceId})"
        ))),
    }
}

/// Bootstrap-time `eth_call` classification. Retryable **only** when we could
/// not reach the node; anything the node *answered* with — a revert, empty
/// return data, undecodable output — is a deterministic deployment/config
/// mismatch and terminal: re-running against the same address re-fails
/// identically. (A contract revert arrives as
/// `TransportError(RpcError::ErrorResp)`, so matching on the outer variant
/// alone would misfile "wrong contract" as "L1 unreachable".)
fn map_contract_bootstrap_error(err: alloy::contract::Error) -> InputReaderError {
    use alloy::transports::RpcError;
    match &err {
        alloy::contract::Error::TransportError(RpcError::Transport(_) | RpcError::NullResp) => {
            InputReaderError::Provider(err.to_string())
        }
        _ => InputReaderError::Bootstrap(err.to_string()),
    }
}

/// Why the reader refuses pre-v3 InputBoxes: the scan genesis is the app's
/// deployment block, sound only under v3's "no inputs for a not-yet-deployed
/// app" rule.
const INPUT_BOX_V3_REQUIRED: &str = "rollups-contracts v3+ InputBox required \
     (v2 accepts inputs for not-yet-deployed apps, which would invalidate the \
     app-deployment scan genesis)";

/// Witness that the InputBox implements the v3 contract semantics the scan
/// genesis depends on. `version()` exists only from v3 on — a revert (v2 has
/// no such function) or a pre-3 major is a terminal bootstrap error; a
/// transport failure stays retryable.
async fn verify_input_box_v3<P: Provider>(
    input_box: &InputBox::InputBoxInstance<&P>,
) -> Result<(), InputReaderError> {
    let input_box_address = *input_box.address();
    let version =
        input_box.version().call().await.map_err(|err| {
            match map_contract_bootstrap_error(err) {
                retryable @ InputReaderError::Provider(_) => retryable,
                terminal => InputReaderError::Bootstrap(format!(
                    "InputBox at {input_box_address} does not report a version() — \
                 {INPUT_BOX_V3_REQUIRED}: {terminal}"
                )),
            }
        })?;
    check_input_box_version(
        input_box_address,
        version.major,
        version.minor,
        version.patch,
    )
}

/// The decidable half of [`verify_input_box_v3`], split out so the acceptance
/// bound is unit-testable without a deployed contract.
fn check_input_box_version(
    input_box_address: Address,
    major: u64,
    minor: u64,
    patch: u64,
) -> Result<(), InputReaderError> {
    if major < 3 {
        return Err(InputReaderError::Bootstrap(format!(
            "InputBox at {input_box_address} reports version {major}.{minor}.{patch} — \
             {INPUT_BOX_V3_REQUIRED}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct SafeHead {
    block_number: u64,
    block_timestamp: u64,
}

/// `U256 → u64` for values the InputBox contract bounds well below `2^64`.
/// Overflow means the response is not something the contract can produce, so
/// it is reported — with the value — as an inconsistent response, not a panic.
fn u256_to_u64(value: U256, what: &str) -> Result<u64, InputReaderError> {
    u64::try_from(value).map_err(|_| {
        InputReaderError::InconsistentL1Response(format!("{what} exceeds u64: {value}"))
    })
}

/// Decode one `InputAdded` log into the row we persist, plus the on-chain
/// index the F5 contiguity witness checks. Pure — unit tests drive it with
/// synthetic `(InputAdded, Log)` pairs, no provider required. Mirrors the
/// watchdog's `decode_and_validate_log`: the row mixes provenances (block
/// number and tx hash from the log, everything else from the `EvmAdvance`
/// payload), so the two fields both sides carry — block number and index —
/// are cross-checked here.
fn ingest_input_added(
    event: &InputAdded,
    log: &alloy::rpc::types::Log,
) -> Result<(u64, IngestedSafeInput), InputReaderError> {
    let onchain_index = u256_to_u64(event.index, "InputAdded index")?;

    let Some(block_number) = log.block_number else {
        return Err(InputReaderError::InconsistentL1Response(
            "InputAdded log missing block_number".to_string(),
        ));
    };
    let Some(transaction_hash) = log.transaction_hash else {
        return Err(InputReaderError::InconsistentL1Response(
            "InputAdded log missing transaction_hash".to_string(),
        ));
    };
    let evm_advance = decode_evm_advance_input(event.input.as_ref()).map_err(|err| {
        InputReaderError::InconsistentL1Response(format!(
            "decode EvmAdvance for InputAdded index {} at tx {transaction_hash}: {err}",
            event.index
        ))
    })?;
    if evm_advance.blockNumber != U256::from(block_number) {
        return Err(InputReaderError::InconsistentL1Response(format!(
            "InputAdded block number mismatch: log={block_number}, payload={}",
            evm_advance.blockNumber
        )));
    }
    if evm_advance.index != event.index {
        return Err(InputReaderError::InconsistentL1Response(format!(
            "InputAdded index mismatch: topic={}, payload={}",
            event.index, evm_advance.index
        )));
    }
    let block_timestamp = u256_to_u64(evm_advance.blockTimestamp, "EvmAdvance block timestamp")?;

    Ok((
        onchain_index,
        IngestedSafeInput {
            sender: evm_advance.msgSender,
            payload: evm_advance.payload.clone().into(),
            block_number,
            block_timestamp,
            transaction_hash,
        },
    ))
}

/// Verify the ingested InputBox indices continue contiguously from
/// `expected_start` (the running count of stored inputs). The InputBox assigns
/// every input a per-app, gap-free index, and we ingest every event for this app
/// from genesis, so the indices must be `expected_start, expected_start+1, …`. A
/// gap means the provider returned an incomplete `get_logs` set (a clamped or
/// lagging-replica response — F5, see `docs/threat-model/README.md`); the
/// retryable [`InconsistentL1Response`] refuses to persist the hole — a
/// consistent provider returns the full set on the next tick.
///
/// [`InconsistentL1Response`]: InputReaderError::InconsistentL1Response
fn check_input_index_contiguity(
    expected_start: u64,
    onchain_indices: &[u64],
) -> Result<(), InputReaderError> {
    for (offset, &index) in onchain_indices.iter().enumerate() {
        let expected = expected_start.saturating_add(offset as u64);
        if index != expected {
            return Err(InputReaderError::InconsistentL1Response(format!(
                "non-contiguous InputBox index: expected {expected}, got {index} — \
                 provider returned an incomplete InputAdded set (F5)"
            )));
        }
    }
    Ok(())
}

/// Verify the InputBox's own input count at the scanned safe block matches the
/// number of inputs we now hold (`received_total` = previously stored + just
/// received). A mismatch means the `get_logs` response was an incomplete prefix
/// — typically a truncated tail a contiguity check alone cannot see (F5, see
/// `docs/threat-model/README.md`). Retryable [`InconsistentL1Response`]: a
/// consistent provider returns the full set, and the matching count, on the
/// next tick.
///
/// [`InconsistentL1Response`]: InputReaderError::InconsistentL1Response
fn check_input_count_complete(
    safe_block: u64,
    received_total: u64,
    onchain_count: u64,
) -> Result<(), InputReaderError> {
    if onchain_count != received_total {
        return Err(InputReaderError::InconsistentL1Response(format!(
            "InputBox input count at block {safe_block} is {onchain_count}, expected \
             {received_total} — provider returned an incomplete get_logs set (F5)"
        )));
    }
    Ok(())
}

/// The InputBox's per-app input count, pinned at `block`. Used as the F5
/// completeness witness — see [`check_input_count_complete`]. Pinning to the
/// scanned safe block (not `latest`) keeps it consistent with the `get_logs`
/// range and forces the serving node to have that block's state.
async fn input_count_at_block(
    provider: &impl Provider,
    input_box_address: &Address,
    app_address: Address,
    block: u64,
) -> Result<u64, InputReaderError> {
    let input_box = InputBox::new(*input_box_address, provider);
    let count = input_box
        .getNumberOfInputs(app_address)
        .block(alloy::eips::BlockId::number(block))
        .call()
        .await
        .map_err(|e| InputReaderError::Provider(e.to_string()))?;
    u256_to_u64(count, "InputBox getNumberOfInputs")
}

async fn latest_safe_head(provider: &impl Provider) -> Result<SafeHead, InputReaderError> {
    let block = provider
        .get_block(Safe.into())
        .await
        .map_err(|e| InputReaderError::Provider(e.to_string()))?
        .ok_or_else(|| InputReaderError::Provider("get_block returned None".to_string()))?;
    Ok(SafeHead {
        block_number: block.header.number,
        block_timestamp: block.header.timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::node_bindings::Anvil;
    use alloy::sol_types::SolCall;
    use tempfile::NamedTempFile;

    fn test_timing() -> ProtocolTiming {
        ProtocolTiming {
            max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
            preemptive_margin_blocks: 75,
            l1_read_stale_after_blocks: 900,
            seconds_per_block: 12,
        }
    }

    fn test_reader(
        db_path: String,
        rpc_url: String,
        app_deployment_block: u64,
        poll_interval: Duration,
    ) -> InputReader {
        InputReader::from_parts(
            InputReaderConfig {
                rpc_url,
                allow_insecure_rpc: false,
                app_address: Address::ZERO,
                poll_interval,
                long_block_range_error_codes: Vec::new(),
                // Anvil's default chain id — tests that drive a real provider go
                // through the chain-id check in `advance_once`.
                expected_chain_id: 31337,
            },
            Address::ZERO,
            app_deployment_block,
            db_path,
            Address::ZERO,
            test_timing(),
        )
    }

    /// Verify that `anvil` is available. Panics with a clear message if not found.
    fn require_anvil() {
        assert!(
            std::process::Command::new("anvil")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok(),
            "anvil not found on PATH — install Foundry (https://getfoundry.sh)"
        );
    }

    #[tokio::test]
    async fn start_then_request_shutdown_joins_with_ok() {
        let db_file = NamedTempFile::new().expect("temp file");
        let shutdown = ShutdownSignal::default();
        let reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            "http://127.0.0.1:0".to_string(),
            0,
            Duration::from_millis(20),
        );
        let handle = reader.start(shutdown.clone()).expect("start input reader");

        shutdown.request_shutdown();
        let join_result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        let join_result = join_result.expect("reader should exit within timeout");
        assert!(
            matches!(join_result, Ok(Ok(()))),
            "expected Ok(Ok(())), got {:?}",
            join_result
        );
    }

    #[tokio::test]
    async fn start_with_anvil_request_shutdown_then_join_returns_ok() {
        require_anvil();

        let anvil = Anvil::default().block_time(1).timeout(30_000).spawn();
        let shutdown = ShutdownSignal::default();
        let db_file = NamedTempFile::new().expect("temp file");
        let reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            anvil.endpoint_url().to_string(),
            0,
            Duration::from_millis(50),
        );
        let handle = reader.start(shutdown.clone()).expect("start input reader");

        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.request_shutdown();

        let join_result = tokio::time::timeout(Duration::from_secs(3), handle).await;
        let join_result = join_result.expect("reader should exit within timeout");
        assert!(
            matches!(join_result, Ok(Ok(()))),
            "expected Ok(Ok(())), got {:?}",
            join_result
        );
    }

    #[tokio::test]
    async fn advance_once_with_anvil_updates_safe_head_when_block_available() {
        require_anvil();

        // No auto-mining: the head stays pinned at block 0, deterministically
        // below the app deployment block, so this exercises exactly the
        // first-observation empty-append path it asserts. (With a block timer
        // the head racing past the deployment block would send `advance_once`
        // into the scan path, whose count call fails against the no-code zero
        // address.)
        let anvil = Anvil::default().timeout(30_000).spawn();
        let db_file = NamedTempFile::new().expect("temp file");
        let mut reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            anvil.endpoint_url().to_string(),
            3,
            Duration::from_secs(1),
        );
        let provider = alloy::providers::ProviderBuilder::new()
            .connect(anvil.endpoint_url().to_string().as_str())
            .await
            .expect("connect provider");

        reader.advance_once(&provider).await.expect("advance_once");
        let safe_block = reader.current_safe_block().await.expect("read safe block");
        let safe_end = {
            let mut storage =
                Storage::open(db_file.path().to_string_lossy().as_ref()).expect("open storage");
            storage.safe_input_end_exclusive().expect("safe end")
        };
        assert_eq!(safe_end, 0, "no InputAdded contract so no direct inputs");
        assert!(
            safe_block.is_some(),
            "first successful safe-head observation should create the row"
        );
    }

    #[tokio::test]
    async fn advance_once_refuses_wrong_chain_id() {
        require_anvil();

        let anvil = Anvil::default().block_time(1).timeout(30_000).spawn();
        let db_file = NamedTempFile::new().expect("temp file");
        // Reader pinned to a chain id the Anvil provider does not serve.
        let mut reader = InputReader::from_parts(
            InputReaderConfig {
                rpc_url: anvil.endpoint_url().to_string(),
                allow_insecure_rpc: false,
                app_address: Address::ZERO,
                poll_interval: Duration::from_secs(1),
                long_block_range_error_codes: Vec::new(),
                expected_chain_id: 999_999,
            },
            Address::ZERO,
            0,
            db_file.path().to_string_lossy().into_owned(),
            Address::ZERO,
            test_timing(),
        );
        let provider = alloy::providers::ProviderBuilder::new()
            .connect(anvil.endpoint_url().to_string().as_str())
            .await
            .expect("connect provider");

        let result = reader.advance_once(&provider).await;
        assert!(
            matches!(
                result,
                Err(InputReaderError::ChainIdMismatch { rpc, expected })
                    if rpc == 31337 && expected == 999_999
            ),
            "expected ChainIdMismatch, got {result:?}"
        );

        // The check runs before any read/persist, so a wrong-chain provider
        // must not have written a safe-head row.
        let mut storage =
            Storage::open(db_file.path().to_string_lossy().as_ref()).expect("open storage");
        assert_eq!(
            storage.current_safe_block().expect("read safe block"),
            None,
            "a chain-id mismatch must abort before persisting any L1 view"
        );
    }

    #[tokio::test]
    async fn current_safe_block_is_unknown_before_first_observation() {
        let db_file = NamedTempFile::new().expect("temp file");
        let app_deployment_block = 2_u64;
        let reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            "http://127.0.0.1:0".to_string(),
            app_deployment_block,
            Duration::from_secs(1),
        );

        let safe_block = reader.current_safe_block().await.expect("read safe block");
        assert_eq!(safe_block, None);
    }

    #[tokio::test]
    async fn sync_to_current_safe_head_failure_leaves_safe_head_unknown() {
        let db_file = NamedTempFile::new().expect("temp file");
        let app_deployment_block = 5_u64;
        let mut reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            "http://127.0.0.1:0".to_string(),
            app_deployment_block,
            Duration::from_secs(1),
        );

        let result = reader.sync_to_current_safe_head().await;

        assert!(matches!(result, Err(InputReaderError::Provider(_))));

        let mut storage =
            Storage::open(db_file.path().to_string_lossy().as_ref()).expect("open storage");
        assert_eq!(
            storage.current_safe_block().expect("read safe block"),
            None,
            "failed sync must not create a synthetic safe-head row"
        );
    }

    #[tokio::test]
    async fn new_with_invalid_rpc_url_returns_bootstrap_error() {
        let db_file = NamedTempFile::new().expect("temp file");

        let result = InputReader::new(
            db_file.path().to_string_lossy().into_owned(),
            InputReaderConfig {
                rpc_url: "not-a-valid-url".to_string(),
                allow_insecure_rpc: false,
                app_address: Address::ZERO,
                poll_interval: Duration::from_secs(1),
                long_block_range_error_codes: Vec::new(),
                expected_chain_id: 31337,
            },
            Address::ZERO,
            test_timing(),
        )
        .await;

        match result {
            Err(InputReaderError::Bootstrap(_)) => {}
            Err(other) => panic!("expected bootstrap error, got {other:?}"),
            Ok(_) => panic!("invalid RPC URL should fail during bootstrap"),
        }
    }

    #[tokio::test]
    async fn advance_once_when_safe_head_ahead_of_chain_is_no_op() {
        require_anvil();

        let anvil = Anvil::default().block_time(1).timeout(30_000).spawn();
        let db_file = NamedTempFile::new().expect("temp file");
        let db_path = db_file.path().to_string_lossy().into_owned();
        let mut storage = Storage::open(&db_path).expect("open storage");
        let timing = test_timing();
        storage
            .append_safe_inputs(1000, &[], Address::ZERO, &timing)
            .expect("set safe head ahead of chain");
        let recorded_sync = storage
            .last_safe_progress_ms()
            .expect("read safe-progress timestamp")
            .expect("append_safe_inputs should stamp safe progress");
        drop(storage);

        let mut reader = test_reader(
            db_path,
            anvil.endpoint_url().to_string(),
            0,
            Duration::from_secs(1),
        );
        let provider = alloy::providers::ProviderBuilder::new()
            .connect(anvil.endpoint_url().to_string().as_str())
            .await
            .expect("connect provider");

        reader.advance_once(&provider).await.expect("advance_once");
        assert_eq!(
            reader.current_safe_block().await.expect("read"),
            Some(1000),
            "safe head should remain unchanged when already ahead of chain"
        );

        let storage =
            Storage::open(db_file.path().to_string_lossy().as_ref()).expect("re-open storage");
        assert_eq!(
            storage
                .last_safe_progress_ms()
                .expect("read unchanged safe-progress timestamp"),
            Some(recorded_sync),
            "same-head polls must not refresh the safe-progress marker"
        );
    }

    #[test]
    fn decode_input_box_address_rejects_non_abi_payloads() {
        let err = decode_input_box_address(&[0_u8; 19]).expect_err("short bytes should fail");
        assert!(
            err.to_string()
                .contains("invalid DataAvailability calldata")
        );

        let err = decode_input_box_address(&[0x22; 20]).expect_err("raw address bytes should fail");
        assert!(
            err.to_string()
                .contains("invalid DataAvailability calldata")
        );
    }

    #[test]
    fn decode_input_box_address_decodes_input_box_call() {
        let expected = Address::from([0x22; 20]);
        let encoded = InputBoxCall { inputBox: expected }.abi_encode();

        let address = decode_input_box_address(&encoded).expect("InputBox call should decode");
        assert_eq!(address, expected);
    }

    #[test]
    fn decode_input_box_address_rejects_unsupported_variants() {
        let encoded = InputBoxAndEspressoCall {
            inputBox: Address::from([0x33; 20]),
            fromBlock: U256::from(123_u64),
            namespaceId: 42,
        }
        .abi_encode();

        let err =
            decode_input_box_address(&encoded).expect_err("InputBoxAndEspresso should be rejected");
        assert!(
            err.to_string()
                .contains("unsupported DataAvailability.InputBoxAndEspresso")
        );
    }

    #[test]
    fn input_index_contiguity_accepts_a_gap_free_run() {
        assert!(check_input_index_contiguity(0, &[0, 1, 2]).is_ok());
        // Continues contiguously from a non-zero count of already-stored inputs.
        assert!(check_input_index_contiguity(7, &[7, 8, 9]).is_ok());
        // An empty batch (no new inputs) is trivially contiguous.
        assert!(check_input_index_contiguity(5, &[]).is_ok());
    }

    #[test]
    fn input_index_contiguity_rejects_a_dropped_middle_input() {
        // Provider returned 7,9 — input 8 was dropped (clamped/lagging get_logs).
        let err = check_input_index_contiguity(7, &[7, 9])
            .expect_err("a dropped middle input must be rejected");
        assert!(
            matches!(&err, InputReaderError::InconsistentL1Response(m) if m.contains("expected 8, got 9")),
            "got {err:?}"
        );
    }

    #[test]
    fn input_index_contiguity_rejects_a_truncated_tail_on_the_next_tick() {
        // A tail truncation on a prior tick (input 8 missing) surfaces here: the
        // next ingested input is 9 while the stored count is still 8. (The count
        // witness catches the same truncation on the *same* tick — see below.)
        let err = check_input_index_contiguity(8, &[9])
            .expect_err("a tail-truncated input must be caught on the next tick");
        assert!(
            matches!(&err, InputReaderError::InconsistentL1Response(m) if m.contains("expected 8, got 9")),
            "got {err:?}"
        );
    }

    #[test]
    fn input_count_complete_accepts_a_matching_count() {
        // Stored 7, received 3 more → 10 total; chain agrees → complete.
        assert!(check_input_count_complete(100, 10, 10).is_ok());
        // No new inputs and the chain has none beyond what we hold.
        assert!(check_input_count_complete(100, 7, 7).is_ok());
    }

    #[test]
    fn input_count_complete_rejects_a_truncated_tail_same_tick() {
        // We received 7..=8 (received_total 9) but the chain has 10 inputs through
        // this safe block — the tail (index 9) was dropped. Caught immediately,
        // before the safe head is persisted.
        let err = check_input_count_complete(100, 9, 10)
            .expect_err("a truncated tail must fail the completeness witness");
        assert!(
            matches!(&err, InputReaderError::InconsistentL1Response(m)
                if m.contains("count at block 100 is 10, expected 9")),
            "got {err:?}"
        );
    }

    #[test]
    fn input_count_complete_rejects_an_impossible_overcount() {
        // We somehow hold more than the chain reports — also fail loud.
        assert!(check_input_count_complete(100, 11, 10).is_err());
    }

    #[test]
    fn input_box_version_3_and_above_are_accepted() {
        let addr = Address::repeat_byte(0x42);
        assert!(check_input_box_version(addr, 3, 0, 0).is_ok());
        assert!(check_input_box_version(addr, 4, 1, 7).is_ok());
    }

    #[test]
    fn input_box_version_below_3_is_terminal_bootstrap_naming_the_address() {
        let addr = Address::repeat_byte(0x42);
        let err = check_input_box_version(addr, 2, 2, 0).expect_err("v2 must be refused");
        assert!(
            matches!(&err, InputReaderError::Bootstrap(m)
                if m.contains("2.2.0") && m.contains(&addr.to_string())),
            "got {err:?}"
        );
    }

    // ── ingest_input_added: pure decode of one (InputAdded, Log) pair ──

    fn evm_advance_payload(block_number: u64, index: u64) -> Vec<u8> {
        use alloy::sol_types::SolCall;
        cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall {
            chainId: U256::from(31337_u64),
            appContract: Address::ZERO,
            msgSender: Address::repeat_byte(0x11),
            blockNumber: U256::from(block_number),
            blockTimestamp: U256::from(1_700_000_000_u64),
            prevRandao: U256::ZERO,
            index: U256::from(index),
            payload: vec![0xaa, 0xbb].into(),
        }
        .abi_encode()
    }

    fn input_added_pair(
        topic_index: u64,
        payload: Vec<u8>,
        block_number: Option<u64>,
        transaction_hash: Option<alloy_primitives::B256>,
    ) -> (InputAdded, alloy::rpc::types::Log) {
        let event = InputAdded {
            appContract: Address::ZERO,
            index: U256::from(topic_index),
            input: payload.into(),
        };
        let log = alloy::rpc::types::Log {
            inner: alloy_primitives::Log {
                address: Address::ZERO,
                data: alloy_primitives::LogData::new_unchecked(
                    Vec::new(),
                    alloy_primitives::Bytes::new(),
                ),
            },
            block_hash: None,
            block_number,
            block_timestamp: None,
            transaction_hash,
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        };
        (event, log)
    }

    #[test]
    fn ingest_input_added_accepts_a_consistent_pair() {
        let tx = alloy_primitives::B256::repeat_byte(0xcc);
        let (event, log) = input_added_pair(5, evm_advance_payload(90, 5), Some(90), Some(tx));
        let (onchain_index, input) = ingest_input_added(&event, &log).expect("consistent pair");
        assert_eq!(onchain_index, 5);
        assert_eq!(input.block_number, 90);
        assert_eq!(input.block_timestamp, 1_700_000_000);
        assert_eq!(input.sender, Address::repeat_byte(0x11));
        assert_eq!(input.payload.as_slice(), &[0xaa, 0xbb]);
        assert_eq!(input.transaction_hash, tx);
    }

    #[test]
    fn ingest_input_added_rejects_missing_log_provenance() {
        let tx = alloy_primitives::B256::repeat_byte(0xcc);
        let (event, log) = input_added_pair(5, evm_advance_payload(90, 5), None, Some(tx));
        let err = ingest_input_added(&event, &log).expect_err("missing block_number");
        assert!(
            matches!(&err, InputReaderError::InconsistentL1Response(m) if m.contains("block_number")),
            "got {err:?}"
        );

        let (event, log) = input_added_pair(5, evm_advance_payload(90, 5), Some(90), None);
        let err = ingest_input_added(&event, &log).expect_err("missing transaction_hash");
        assert!(
            matches!(&err, InputReaderError::InconsistentL1Response(m) if m.contains("transaction_hash")),
            "got {err:?}"
        );
    }

    #[test]
    fn ingest_input_added_rejects_log_payload_block_number_mismatch() {
        let tx = alloy_primitives::B256::repeat_byte(0xcc);
        // Payload claims block 91, the log says 90 — the same self-contradiction
        // the watchdog's `decode_and_validate_log` raises (and retries) on.
        let (event, log) = input_added_pair(5, evm_advance_payload(91, 5), Some(90), Some(tx));
        let err = ingest_input_added(&event, &log).expect_err("block number mismatch");
        assert!(
            matches!(&err, InputReaderError::InconsistentL1Response(m)
                if m.contains("block number mismatch")),
            "got {err:?}"
        );
    }

    #[test]
    fn ingest_input_added_rejects_topic_payload_index_mismatch() {
        let tx = alloy_primitives::B256::repeat_byte(0xcc);
        // The F5 witness reads the index from the topic; the watchdog reads it
        // from the payload. The cross-check keeps the two provably aligned.
        let (event, log) = input_added_pair(5, evm_advance_payload(90, 6), Some(90), Some(tx));
        let err = ingest_input_added(&event, &log).expect_err("index mismatch");
        assert!(
            matches!(&err, InputReaderError::InconsistentL1Response(m)
                if m.contains("index mismatch")),
            "got {err:?}"
        );
    }

    #[test]
    fn ingest_input_added_rejects_undecodable_payload() {
        let tx = alloy_primitives::B256::repeat_byte(0xcc);
        let (event, log) = input_added_pair(5, vec![0xde, 0xad], Some(90), Some(tx));
        let err = ingest_input_added(&event, &log).expect_err("undecodable payload");
        assert!(
            matches!(&err, InputReaderError::InconsistentL1Response(m)
                if m.contains("decode EvmAdvance")),
            "got {err:?}"
        );
    }

    // ── count-based scan skip, driven through a scripted mock provider ──

    fn mock_safe_block(number: u64) -> alloy::rpc::types::Block {
        let mut block: alloy::rpc::types::Block = Default::default();
        block.header.inner.number = number;
        block.header.inner.timestamp = 1_700_000_000;
        block
    }

    fn abi_u256(value: u64) -> alloy_primitives::Bytes {
        U256::from(value).to_be_bytes::<32>().to_vec().into()
    }

    #[tokio::test]
    async fn advance_once_skips_get_logs_when_onchain_count_matches_stored() {
        let db_file = NamedTempFile::new().expect("temp file");
        let mut reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            "http://127.0.0.1:0".to_string(),
            3,
            Duration::from_secs(1),
        );

        // Scripted responses in call order: eth_chainId, eth_getBlockByNumber
        // (safe head 7), getNumberOfInputs eth_call (0 = matches the empty
        // store). NOTHING is queued after the count — if `advance_once` issued
        // the eth_getLogs crawl this test exists to forbid, the transport
        // would error and the expect below would fail.
        let asserter = alloy::transports::mock::Asserter::new();
        asserter.push_success(&alloy_primitives::U64::from(31337_u64));
        asserter.push_success(&mock_safe_block(7));
        asserter.push_success(&abi_u256(0));
        let provider = alloy::providers::ProviderBuilder::new().connect_mocked_client(asserter);

        reader
            .advance_once(&provider)
            .await
            .expect("count == stored must advance the head without scanning");

        assert_eq!(
            reader.current_safe_block().await.expect("read safe block"),
            Some(7),
            "the skip path must still persist the safe-head advance"
        );
        let mut storage =
            Storage::open(db_file.path().to_string_lossy().as_ref()).expect("open storage");
        assert_eq!(storage.safe_input_end_exclusive().expect("count"), 0);
    }

    #[tokio::test]
    async fn advance_once_falls_through_and_fails_completeness_when_count_exceeds_stored() {
        let db_file = NamedTempFile::new().expect("temp file");
        let mut reader = test_reader(
            db_file.path().to_string_lossy().into_owned(),
            "http://127.0.0.1:0".to_string(),
            3,
            Duration::from_secs(1),
        );

        // Count says one input exists but the (scripted) scan returns none:
        // the skip must NOT fire, and the completeness witness must refuse to
        // advance the head past an input we never received.
        let asserter = alloy::transports::mock::Asserter::new();
        asserter.push_success(&alloy_primitives::U64::from(31337_u64));
        asserter.push_success(&mock_safe_block(7));
        asserter.push_success(&abi_u256(1));
        asserter.push_success(&Vec::<alloy::rpc::types::Log>::new());
        let provider = alloy::providers::ProviderBuilder::new().connect_mocked_client(asserter);

        let err = reader
            .advance_once(&provider)
            .await
            .expect_err("count > stored with an empty scan must fail the witness");
        assert!(
            matches!(&err, InputReaderError::InconsistentL1Response(m)
                if m.contains("count at block 7 is 1, expected 0")),
            "got {err:?}"
        );
        assert_eq!(
            reader.current_safe_block().await.expect("read safe block"),
            None,
            "a failed completeness witness must not persist a safe head"
        );
    }
}
