// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Startup recovery while the process lock excludes other writers and no
//! workers are running. Local inspection selects one repair: Tip replacement
//! or Flush → Sync → Cascade. A fresh check follows repair and runtime
//! preparation; only the final check authorizes worker launch.

mod detector;
mod flusher;

use thiserror::Error;

use crate::l1::L1Config;
use crate::l1::reader::{InputReader, InputReaderError};
use crate::storage::{
    self, DangerStatus, RecoveryInspection, RecoveryMutationError, StorageOpenError,
};
pub use detector::{DangerDetector, DangerDetectorError, DetectorExit};
pub use flusher::{FlushError, MempoolFlusher};
use sequencer_core::protocol::ProtocolTiming;

/// A startup recovery failure is already classified when it leaves the
/// controller. Runtime lifecycle settlement projects only this outer class;
/// it never reinterprets raw provider/storage/phase errors.
#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("startup recovery should retry: {0}")]
    Retry(Box<RecoveryFailure>),
    #[error("startup recovery refused: {0}")]
    Refuse(Box<RecoveryFailure>),
}

/// Diagnostic provenance retained underneath the controller's retry/refuse
/// verdict.
#[derive(Debug, Error)]
pub enum RecoveryFailure {
    #[error(transparent)]
    PolicyRetry(#[from] RecoveryRetryReason),
    #[error(transparent)]
    PolicyRefusal(#[from] RecoveryRefusalReason),
    #[error("open storage: {0}")]
    OpenStorage(#[source] StorageOpenError),
    #[error("storage: {0}")]
    Storage(#[source] rusqlite::Error),
    #[error("flush: {0}")]
    Flush(#[source] FlushError),
    #[error("input reader: {0}")]
    InputReader(#[source] InputReaderError),
    /// The chain-id read on the flush's signer provider failed: transport,
    /// so retry.
    #[error("provider unreachable: {0}")]
    ProviderUnreachable(String),
    /// The flush's signer provider could not be constructed (bad RPC URL or
    /// key): deterministic misconfiguration, so refuse. Kept distinct from
    /// [`Self::ProviderUnreachable`] so the verdict is a function of the
    /// value, never of the construction site.
    #[error("signer provider misconfiguration: {0}")]
    SignerMisconfig(String),
    #[error("recovery flush chain-id mismatch: rpc {rpc} != pinned {expected}")]
    ChainIdMismatch { rpc: u64, expected: u64 },
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryRetryReason {
    #[error("the persisted L1 view is stale")]
    L1ViewStale,
    #[error("batch {batch_index} is in danger only under wall-clock estimation")]
    EstimatedBatchInDanger { batch_index: u64 },
    #[error("danger persists after repair: {status:?}")]
    DangerPersists { status: DangerStatus },
    #[error(
        "post-flush re-sync reached safe block {resynced_safe_block}, behind the flush observation at {flush_observed_safe_block}"
    )]
    ResyncBehindFlushView {
        resynced_safe_block: u64,
        flush_observed_safe_block: u64,
    },
    #[error("local recovery facts changed before phase execution: {status:?}")]
    StaleDecision { status: DangerStatus },
    #[error("the Tip was already open when the EnsureOpenTip phase ran")]
    TipAlreadyOpen,
    #[error("runtime preparation outlived its clean admission decision ({decision})")]
    AdmissionChanged { decision: &'static str },
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryRefusalReason {
    /// A fully accepted L1 landing failed content identity. Standard recovery
    /// assumes the opposite and is forbidden.
    #[error("canonical divergence at batch nonce {nonce}")]
    CanonicalDivergence { nonce: u64 },
    #[error("the completed setup has no finalized snapshot")]
    MissingFinalizedSnapshot,
    #[error("post-sync recovery has no persisted safe head")]
    MissingSafeHead,
    /// The `EnsureOpenTip` transaction violated its open-Tip postcondition.
    #[error("the EnsureOpenTip phase left no valid open Tip")]
    TipMissingAfterOpen,
}

/// Single-use proof that the final check found clean, consistent facts after
/// all fallible preparation. Only [`admit_runtime`] can construct it.
#[must_use = "runtime admission must be consumed by PreparedRuntime::launch"]
#[derive(Debug)]
pub(crate) struct RuntimeAdmission {
    _private: (),
}

impl RecoveryError {
    pub(crate) fn retry(failure: impl Into<RecoveryFailure>) -> Self {
        Self::Retry(Box::new(failure.into()))
    }

    pub(crate) fn refuse(failure: impl Into<RecoveryFailure>) -> Self {
        Self::Refuse(Box::new(failure.into()))
    }

    pub(crate) fn is_retryable(&self) -> bool {
        matches!(self, Self::Retry(_))
    }
}

/// The post-flush resync coherence check — the resynced safe block must
/// reach the flush observation before cascade — shared with
/// `setup --recovery`. Runtime recovery also enforces this inside the
/// guarded cascade transaction.
pub(crate) fn assert_resync_caught_up(
    resynced_safe_block: u64,
    flush_observed_safe_block: u64,
) -> Result<(), RecoveryError> {
    if resynced_safe_block < flush_observed_safe_block {
        return Err(RecoveryError::retry(
            RecoveryRetryReason::ResyncBehindFlushView {
                resynced_safe_block,
                flush_observed_safe_block,
            },
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryAction {
    Ready,
    EnsureOpenTip,
    RecoverTip { batch_index: u64 },
    Flush,
}

impl RecoveryAction {
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "admit",
            Self::EnsureOpenTip => "ensure_open_tip",
            Self::RecoverTip { .. } => "recover_tip",
            Self::Flush => "flush",
        }
    }
}

fn refuse_local_terminal(facts: RecoveryInspection) -> Result<(), RecoveryError> {
    if let DangerStatus::CanonicalDivergence(nonce) = facts.danger {
        return Err(RecoveryError::refuse(
            RecoveryRefusalReason::CanonicalDivergence { nonce },
        ));
    }
    if !facts.has_finalized_snapshot {
        return Err(RecoveryError::refuse(
            RecoveryRefusalReason::MissingFinalizedSnapshot,
        ));
    }
    Ok(())
}

/// The startup dispatch and final admission use the same exhaustive policy.
fn select_recovery(facts: RecoveryInspection) -> Result<RecoveryAction, RecoveryError> {
    refuse_local_terminal(facts)?;
    match facts.danger {
        DangerStatus::Safe if facts.has_open_tip => Ok(RecoveryAction::Ready),
        DangerStatus::Safe => Ok(RecoveryAction::EnsureOpenTip),
        DangerStatus::ClosedBatchInDanger(_) => Ok(RecoveryAction::Flush),
        DangerStatus::TipInDanger(batch_index) => Ok(RecoveryAction::RecoverTip { batch_index }),
        DangerStatus::L1ViewStale => Err(RecoveryError::retry(RecoveryRetryReason::L1ViewStale)),
        DangerStatus::EstimatedBatchInDanger(batch_index) => Err(RecoveryError::retry(
            RecoveryRetryReason::EstimatedBatchInDanger { batch_index },
        )),
        DangerStatus::CanonicalDivergence(_) => {
            unreachable!("terminal facts were checked before dispatch")
        }
    }
}

fn inspect_recovery(
    db_path: &str,
    protocol: &ProtocolTiming,
) -> Result<RecoveryInspection, RecoveryError> {
    let mut storage = storage::Storage::open_writer(db_path).map_err(classify_open)?;
    storage
        .inspect_recovery(protocol, crate::clock::unix_now_ms())
        .map_err(classify_storage)
}

/// Only L1 operations are abstracted: tests keep the real local inspections
/// and repair transactions and substitute the external system.
trait RecoveryL1 {
    async fn sync(&mut self) -> Result<(), InputReaderError>;
    async fn flush(&mut self) -> Result<u64, RecoveryError>;
}

struct StartupL1<'a> {
    db_path: &'a str,
    input_reader: &'a mut InputReader,
    l1_config: &'a L1Config,
    protocol: &'a ProtocolTiming,
}

impl RecoveryL1 for StartupL1<'_> {
    async fn sync(&mut self) -> Result<(), InputReaderError> {
        self.input_reader.sync_to_current_safe_head().await
    }

    async fn flush(&mut self) -> Result<u64, RecoveryError> {
        let provider = crate::l1::provider::create_verified_signer_provider(
            &self.l1_config.eth_rpc_url,
            self.l1_config.batch_submitter_private_key.expose_secret(),
            self.l1_config.identity.chain_id,
            self.l1_config.allow_insecure_rpc,
        )
        .await
        .map_err(classify_signer_provider)?;

        let watermark = {
            let mut storage = storage::Storage::open_writer(self.db_path).map_err(classify_open)?;
            storage.wallet_nonce_watermark().map_err(classify_storage)?
        };
        MempoolFlusher::flush_to_safe(
            provider,
            self.l1_config.identity.batch_submitter_address,
            self.protocol.seconds_per_block,
            self.db_path,
            watermark,
        )
        .await
        .map_err(classify_flush)
    }
}

/// Repair once, then require a clean view. Flush cannot change local recovery
/// facts except the wallet watermark; Sync is the only external operation that
/// can discover divergence. The guarded repair transaction checks those new
/// facts before mutation, including the post-flush safe-head floor.
async fn recover_startup(
    db_path: &str,
    protocol: &ProtocolTiming,
    l1: &mut impl RecoveryL1,
) -> Result<(), RecoveryError> {
    refuse_local_terminal(inspect_recovery(db_path, protocol)?)?;
    match l1.sync().await {
        Ok(()) => tracing::info!("L1 safe head synced"),
        // An unreachable provider does not invalidate a still-fresh persisted
        // view. Post-flush Sync has no such fallback: cascade needs its result.
        Err(InputReaderError::Provider(error)) => tracing::warn!(
            error = %error,
            "L1 unreachable during initial startup sync; inspecting persisted view"
        ),
        Err(error) => return Err(classify_input_reader(error)),
    }

    let facts = inspect_recovery(db_path, protocol)?;
    let action = select_recovery(facts)?;
    tracing::info!(
        danger_status = facts.danger.label(),
        danger_batch_index = ?facts.danger.batch_index(),
        recovery_decision = action.label(),
        "startup recovery decision"
    );
    let invalidated = match action {
        RecoveryAction::Ready => return Ok(()),
        RecoveryAction::EnsureOpenTip => {
            let mut storage = storage::Storage::open_writer(db_path).map_err(classify_open)?;
            storage
                .ensure_open_tip_for_recovery(protocol, crate::clock::unix_now_ms())
                .map_err(classify_mutation)?;
            Vec::new()
        }
        RecoveryAction::RecoverTip { batch_index } => {
            let mut storage = storage::Storage::open_writer(db_path).map_err(classify_open)?;
            storage
                .recover_aging_tip_for_recovery(batch_index, protocol, crate::clock::unix_now_ms())
                .map_err(classify_mutation)?
        }
        RecoveryAction::Flush => {
            // The observation exists only on this invocation's stack. A crash
            // or retry loses it; a new attempt must flush again before cascade.
            let observed_safe_block = l1.flush().await?;
            l1.sync().await.map_err(classify_input_reader)?;
            let mut storage = storage::Storage::open_writer(db_path).map_err(classify_open)?;
            storage
                .recover_post_flush_for_recovery(
                    observed_safe_block,
                    protocol,
                    crate::clock::unix_now_ms(),
                )
                .map_err(classify_mutation)?
        }
    };
    if invalidated.is_empty() {
        tracing::info!("startup recovery completed without invalidation");
    } else {
        tracing::warn!(
            invalidated_count = invalidated.len(),
            batches = ?invalidated,
            "startup recovery invalidated the doomed suffix"
        );
    }

    let facts = inspect_recovery(db_path, protocol)?;
    refuse_local_terminal(facts)?;
    // Observed repair can expose a surviving clock/view refusal. Never treat
    // successful mutation as admission or begin another repair in this boot.
    match facts.danger {
        DangerStatus::Safe => {
            assert!(facts.has_open_tip, "recovery committed without an open Tip");
            Ok(())
        }
        status @ (DangerStatus::ClosedBatchInDanger(_)
        | DangerStatus::TipInDanger(_)
        | DangerStatus::L1ViewStale
        | DangerStatus::EstimatedBatchInDanger(_)) => {
            Err(RecoveryError::retry(RecoveryRetryReason::DangerPersists {
                status,
            }))
        }
        DangerStatus::CanonicalDivergence(_) => {
            unreachable!("terminal facts were checked after repair")
        }
    }
}

/// Finish startup recovery before fallible, task-free runtime preparation.
/// This grants no runtime authority; [`admit_runtime`] checks again afterwards.
pub(crate) async fn run_startup_recovery(
    db_path: &str,
    input_reader: &mut InputReader,
    l1_config: &L1Config,
    protocol: &ProtocolTiming,
) -> Result<(), RecoveryError> {
    recover_startup(
        db_path,
        protocol,
        &mut StartupL1 {
            db_path,
            input_reader,
            l1_config,
            protocol,
        },
    )
    .await
}

/// Check current facts after preparation. The process lock and task-free
/// preparation exclude another writer, but elapsed time can stale the view.
/// Launch consumes the resulting witness without yielding or further fallible
/// preparation.
pub(crate) fn admit_runtime(
    db_path: &str,
    protocol: &ProtocolTiming,
) -> Result<RuntimeAdmission, RecoveryError> {
    match select_recovery(inspect_recovery(db_path, protocol)?)? {
        RecoveryAction::Ready => Ok(RuntimeAdmission { _private: () }),
        action => Err(RecoveryError::retry(
            RecoveryRetryReason::AdmissionChanged {
                decision: action.label(),
            },
        )),
    }
}

fn classify_open(error: StorageOpenError) -> RecoveryError {
    let persistent = storage::is_persistent_storage_open_error(&error);
    let failure = RecoveryFailure::OpenStorage(error);
    if persistent {
        RecoveryError::refuse(failure)
    } else {
        RecoveryError::retry(failure)
    }
}

fn classify_storage(error: rusqlite::Error) -> RecoveryError {
    let persistent = storage::is_persistent_storage_error(&error);
    let failure = RecoveryFailure::Storage(error);
    if persistent {
        RecoveryError::refuse(failure)
    } else {
        RecoveryError::retry(failure)
    }
}

/// The flush's signer-provider errors, classified at birth. The
/// terminal/transient split must agree with `From<VerifiedSignerProviderError>
/// for BootstrapError` in `commands/error.rs` (mismatch and construction are
/// terminal; the chain-id read is transient); this is recovery's own
/// retry/refuse polarity over the same facts, pinned beside the others.
fn classify_signer_provider(
    error: crate::l1::provider::VerifiedSignerProviderError,
) -> RecoveryError {
    use crate::l1::provider::VerifiedSignerProviderError;
    match error {
        VerifiedSignerProviderError::ChainIdMismatch { rpc, expected } => {
            RecoveryError::refuse(RecoveryFailure::ChainIdMismatch { rpc, expected })
        }
        VerifiedSignerProviderError::ChainIdRpc(message) => {
            RecoveryError::retry(RecoveryFailure::ProviderUnreachable(message))
        }
        VerifiedSignerProviderError::Create(message) => {
            RecoveryError::refuse(RecoveryFailure::SignerMisconfig(message))
        }
    }
}

/// Input-reader errors met during the startup phases (`InitialSync` and
/// `PostFlushSync`, both through `sync_to_current_safe_head`), classified
/// at birth. `ChainIdMismatch` and `StorageTaskPanicked` are refused here
/// and terminal in the live worker alike. Two variants are phase-dependent
/// by design and differ from `InputReaderError::is_terminal_invariant`:
///
/// - `Bootstrap` here can only be the sync's `create_provider` failing on
///   the configured RPC URL (parse, the plaintext-remote rule, client
///   build), refused because re-running the same boot re-fails
///   identically. The discovery-time facts (wrong contract, pre-v3
///   InputBox) never reach this function: they arise in `InputReader::new`,
///   which only `setup` calls and projects as a worker exit (register
///   finding 32). In the live loop the same URL was already proven by this
///   boot's initial sync, so a live `Bootstrap` restarts unclassified
///   rather than poisoning the data directory.
/// - `Join` (a non-panic loss of a storage task) is shutdown-path
///   cancellation in the live loop. During startup the runtime that would
///   cancel it is the one driving this boot, so an unexplained loss is
///   refused rather than retried blind.
///
/// Both halves are pinned.
fn classify_input_reader(error: InputReaderError) -> RecoveryError {
    match error {
        error @ (InputReaderError::Provider(_) | InputReaderError::InconsistentL1Response(_)) => {
            RecoveryError::retry(RecoveryFailure::InputReader(error))
        }
        InputReaderError::OpenStorage(source) => classify_open(source),
        InputReaderError::Storage(source) => classify_storage(source),
        error @ (InputReaderError::ChainIdMismatch { .. }
        | InputReaderError::Bootstrap(_)
        | InputReaderError::StorageTaskPanicked { .. }
        | InputReaderError::Join(_)) => RecoveryError::refuse(RecoveryFailure::InputReader(error)),
    }
}

fn classify_flush(error: FlushError) -> RecoveryError {
    let terminal = error.is_terminal_invariant();
    let failure = RecoveryFailure::Flush(error);
    if terminal {
        RecoveryError::refuse(failure)
    } else {
        RecoveryError::retry(failure)
    }
}

fn classify_mutation(error: RecoveryMutationError) -> RecoveryError {
    match error {
        RecoveryMutationError::Storage(source) => classify_storage(source),
        RecoveryMutationError::CanonicalDivergence { nonce } => {
            RecoveryError::refuse(RecoveryRefusalReason::CanonicalDivergence { nonce })
        }
        RecoveryMutationError::MissingFinalizedSnapshot => {
            RecoveryError::refuse(RecoveryRefusalReason::MissingFinalizedSnapshot)
        }
        RecoveryMutationError::MissingSafeHead => {
            RecoveryError::refuse(RecoveryRefusalReason::MissingSafeHead)
        }
        RecoveryMutationError::ResyncBehindFlushView {
            resynced_safe_block,
            flush_observed_safe_block,
        } => RecoveryError::retry(RecoveryRetryReason::ResyncBehindFlushView {
            resynced_safe_block,
            flush_observed_safe_block,
        }),
        RecoveryMutationError::StaleDecision { actual, .. } => {
            RecoveryError::retry(RecoveryRetryReason::StaleDecision { status: actual })
        }
        RecoveryMutationError::TipAlreadyOpen => {
            RecoveryError::retry(RecoveryRetryReason::TipAlreadyOpen)
        }
        RecoveryMutationError::TipMissingAfterOpen => {
            RecoveryError::refuse(RecoveryRefusalReason::TipMissingAfterOpen)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn sqlite_failure(code: rusqlite::ffi::ErrorCode, extended_code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code,
            },
            None,
        )
    }

    fn assert_retry(error: RecoveryError) {
        assert!(
            matches!(error, RecoveryError::Retry(_)),
            "expected retry, got {error:?}"
        );
    }

    fn assert_refuse(error: RecoveryError) {
        assert!(
            matches!(error, RecoveryError::Refuse(_)),
            "expected refusal, got {error:?}"
        );
    }

    #[test]
    fn error_classifiers_pin_retry_and_refuse_polarity() {
        use crate::l1::watermark::WalletNonceWatermarkError;

        let busy = || sqlite_failure(rusqlite::ffi::ErrorCode::DatabaseBusy, 5);
        let corrupt = || sqlite_failure(rusqlite::ffi::ErrorCode::NotADatabase, 26);

        assert_retry(classify_open(StorageOpenError::Sqlite(busy())));
        assert_refuse(classify_open(StorageOpenError::Sqlite(corrupt())));
        assert_retry(classify_storage(busy()));
        assert_refuse(classify_storage(rusqlite::Error::QueryReturnedNoRows));

        assert_retry(classify_input_reader(InputReaderError::Provider(
            "offline".into(),
        )));
        assert_retry(classify_input_reader(
            InputReaderError::InconsistentL1Response("gap".into()),
        ));
        assert_refuse(classify_input_reader(InputReaderError::ChainIdMismatch {
            rpc: 1,
            expected: 2,
        }));
        // Phase-dependent: refused at startup, non-terminal in the live
        // worker (`InputReaderError::is_terminal_invariant`).
        assert_refuse(classify_input_reader(InputReaderError::Bootstrap(
            "plaintext rpc url".into(),
        )));
        assert_refuse(classify_input_reader(InputReaderError::Join(
            "sync task lost".into(),
        )));
        assert!(!InputReaderError::Bootstrap("x".into()).is_terminal_invariant());
        assert!(!InputReaderError::Join("x".into()).is_terminal_invariant());

        {
            use crate::commands::error::BootstrapError;
            use crate::l1::provider::VerifiedSignerProviderError as Signer;
            // The split's payloads are load-bearing, not only its polarity:
            // swapping the two provider variants must go red.
            assert!(matches!(
                classify_signer_provider(Signer::ChainIdRpc("timeout".into())),
                RecoveryError::Retry(f) if matches!(*f, RecoveryFailure::ProviderUnreachable(_))
            ));
            assert!(matches!(
                classify_signer_provider(Signer::Create("bad url".into())),
                RecoveryError::Refuse(f) if matches!(*f, RecoveryFailure::SignerMisconfig(_))
            ));
            assert!(matches!(
                classify_signer_provider(Signer::ChainIdMismatch {
                    rpc: 1,
                    expected: 2,
                }),
                RecoveryError::Refuse(f) if matches!(*f, RecoveryFailure::ChainIdMismatch { .. })
            ));
            // The doc's "must agree with the `BootstrapError` projection":
            // recovery retries exactly the arm that projection calls
            // transient.
            let mint = || {
                [
                    Signer::ChainIdRpc("timeout".into()),
                    Signer::Create("bad url".into()),
                    Signer::ChainIdMismatch {
                        rpc: 1,
                        expected: 2,
                    },
                ]
            };
            for (recovery_side, projection_side) in mint().into_iter().zip(mint()) {
                let retried = matches!(
                    classify_signer_provider(recovery_side),
                    RecoveryError::Retry(_)
                );
                let transient = matches!(
                    BootstrapError::from(projection_side),
                    BootstrapError::ChainIdRpc { .. }
                );
                assert_eq!(retried, transient);
            }
        }

        assert_retry(classify_flush(FlushError::Provider("offline".into())));
        assert_refuse(classify_flush(FlushError::Watermark(
            WalletNonceWatermarkError::Storage(rusqlite::Error::QueryReturnedNoRows),
        )));

        assert_retry(classify_mutation(RecoveryMutationError::TipAlreadyOpen));
        assert_refuse(classify_mutation(
            RecoveryMutationError::TipMissingAfterOpen,
        ));
        assert_retry(classify_mutation(RecoveryMutationError::StaleDecision {
            expected: DangerStatus::Safe,
            actual: DangerStatus::L1ViewStale,
        }));
        assert_retry(classify_mutation(
            RecoveryMutationError::ResyncBehindFlushView {
                resynced_safe_block: 10,
                flush_observed_safe_block: 11,
            },
        ));
        assert_refuse(classify_mutation(
            RecoveryMutationError::CanonicalDivergence { nonce: 7 },
        ));
        assert_refuse(classify_mutation(
            RecoveryMutationError::MissingFinalizedSnapshot,
        ));
        assert_refuse(classify_mutation(RecoveryMutationError::MissingSafeHead));
    }

    fn admission_fixture(
        name: &str,
        has_finalized_snapshot: bool,
        has_open_tip: bool,
    ) -> (crate::storage::test_helpers::TestDb, ProtocolTiming) {
        use crate::storage::test_helpers::{SENDER_A, default_protocol_timing, temp_db};

        let db = temp_db(name);
        let protocol = default_protocol_timing();
        let now_ms = crate::clock::unix_now_ms();
        let mut storage =
            storage::Storage::initialize_for_command(&db.path, storage::LifecycleCommand::Setup)
                .expect("initialize setup");
        crate::storage::test_helpers::pin_test_deployment_identity(&mut storage, SENDER_A);
        storage
            .append_safe_inputs_with_timestamp(
                0,
                now_ms / 1_000,
                &[],
                SENDER_A,
                &protocol,
                storage::FrontierMode::Populate,
            )
            .expect("seed fresh safe head");
        let prefix = db._dir.path().join("finalized");
        storage
            .insert_initial_finalized_dump(&prefix, 0, 0, 0, 0)
            .expect("seed finalized snapshot");
        if has_open_tip {
            storage
                .initialize_open_state(0, storage::SafeInputRange::empty_at(0))
                .expect("seed open Tip");
        }
        storage.complete_setup().expect("complete setup");
        if !has_finalized_snapshot {
            storage
                .write(|tx| {
                    tx.execute("DELETE FROM finalized_snapshot", [])?;
                    Ok(())
                })
                .expect("simulate post-setup snapshot loss");
        }
        drop(storage);
        (db, protocol)
    }

    enum SyncStep {
        Keep,
        Fail(InputReaderError),
        Observe {
            block: u64,
            inputs: Vec<storage::StoredSafeInput>,
        },
    }

    struct TestL1<'a> {
        db_path: &'a str,
        protocol: &'a ProtocolTiming,
        syncs: VecDeque<SyncStep>,
        flush_block: u64,
        calls: Vec<&'static str>,
    }

    impl<'a> TestL1<'a> {
        fn new(db_path: &'a str, protocol: &'a ProtocolTiming, syncs: Vec<SyncStep>) -> Self {
            Self {
                db_path,
                protocol,
                syncs: syncs.into(),
                flush_block: protocol.danger_threshold(),
                calls: Vec::new(),
            }
        }
    }

    impl RecoveryL1 for TestL1<'_> {
        async fn sync(&mut self) -> Result<(), InputReaderError> {
            self.calls.push("sync");
            match self.syncs.pop_front().expect("unexpected L1 sync") {
                SyncStep::Keep => Ok(()),
                SyncStep::Fail(error) => Err(error),
                SyncStep::Observe { block, inputs } => storage::Storage::open_writer(self.db_path)
                    .expect("open fixture storage")
                    .append_safe_inputs_with_timestamp(
                        block,
                        crate::clock::unix_now_ms() / 1_000,
                        &inputs,
                        crate::storage::test_helpers::SENDER_A,
                        self.protocol,
                        storage::FrontierMode::Populate,
                    )
                    .map_err(InputReaderError::Storage),
            }
        }

        async fn flush(&mut self) -> Result<u64, RecoveryError> {
            self.calls.push("flush");
            Ok(self.flush_block)
        }
    }

    fn danger_fixture(
        name: &str,
        closed: bool,
    ) -> (crate::storage::test_helpers::TestDb, ProtocolTiming) {
        let (db, protocol) = admission_fixture(name, true, true);
        let mut storage = storage::Storage::open_writer(&db.path).expect("open writer");
        if closed {
            let mut head = storage.open_state().unwrap().unwrap();
            storage.close_frame_and_batch(&mut head, 0).unwrap();
        }
        storage
            .append_safe_inputs_with_timestamp(
                protocol.danger_threshold(),
                crate::clock::unix_now_ms() / 1_000,
                &[],
                crate::storage::test_helpers::SENDER_A,
                &protocol,
                storage::FrontierMode::Populate,
            )
            .expect("advance into observed danger");
        (db, protocol)
    }

    fn invalidated_batches(db_path: &str) -> Vec<u64> {
        let mut storage = storage::Storage::open_writer(db_path).unwrap();
        storage
            .read(|tx| {
                tx.prepare("SELECT batch_index FROM batches WHERE invalidated_at_ms IS NOT NULL ORDER BY batch_index")?
                    .query_map([], |row| {
                        Ok(u64::try_from(row.get::<_, i64>(0)?).expect("nonnegative batch index"))
                    })?
                    .collect()
            })
            .unwrap()
    }

    #[tokio::test]
    async fn initial_provider_failure_uses_only_a_fresh_persisted_view() {
        let (db, protocol) = admission_fixture("recovery-warm-provider-failure", true, true);
        let offline = || SyncStep::Fail(InputReaderError::Provider("offline".into()));
        let mut l1 = TestL1::new(&db.path, &protocol, vec![offline()]);
        recover_startup(&db.path, &protocol, &mut l1).await.unwrap();
        assert_eq!(l1.calls, ["sync"]);

        storage::Storage::open_writer(&db.path)
            .unwrap()
            .write(|tx| tx.execute("UPDATE l1_safe_head SET block_timestamp = 0", []))
            .unwrap();
        let mut l1 = TestL1::new(&db.path, &protocol, vec![offline()]);
        assert_retry(
            recover_startup(&db.path, &protocol, &mut l1)
                .await
                .unwrap_err(),
        );
        assert_eq!(l1.calls, ["sync"]);
    }

    #[tokio::test]
    async fn local_terminal_facts_refuse_before_l1() {
        for diverged in [false, true] {
            let (db, protocol) = admission_fixture("recovery-local-terminal", diverged, true);
            if diverged {
                let mut storage = storage::Storage::open_writer(&db.path).unwrap();
                crate::storage::test_helpers::record_canonical_divergence(&mut storage, 7, 0);
            }
            let mut l1 = TestL1::new(&db.path, &protocol, vec![]);
            assert_refuse(
                recover_startup(&db.path, &protocol, &mut l1)
                    .await
                    .unwrap_err(),
            );
            assert!(l1.calls.is_empty());
        }
    }

    #[tokio::test]
    async fn fresh_start_opens_tip_and_tip_repair_never_flushes() {
        let (db, protocol) = admission_fixture("recovery-ensure-tip", true, false);
        let mut l1 = TestL1::new(&db.path, &protocol, vec![SyncStep::Keep]);
        recover_startup(&db.path, &protocol, &mut l1).await.unwrap();
        assert!(inspect_recovery(&db.path, &protocol).unwrap().has_open_tip);
        assert_eq!(l1.calls, ["sync"]);

        let (db, protocol) = danger_fixture("recovery-tip", false);
        let mut l1 = TestL1::new(&db.path, &protocol, vec![SyncStep::Keep]);
        recover_startup(&db.path, &protocol, &mut l1).await.unwrap();
        assert_eq!(invalidated_batches(&db.path), [0]);
        assert_eq!(l1.calls, ["sync"]);
        assert_eq!(
            inspect_recovery(&db.path, &protocol).unwrap().danger,
            DangerStatus::Safe
        );
    }

    #[tokio::test]
    async fn post_flush_cascade_runs_even_when_the_refreshed_view_is_safe() {
        use crate::storage::test_helpers::{SENDER_A, local_batch_payload};

        let (db, protocol) = admission_fixture("recovery-unconditional-cascade", true, true);
        let block = protocol.danger_threshold();
        let mut storage = storage::Storage::open_writer(&db.path).unwrap();
        let mut head = storage.open_state().unwrap().unwrap();
        storage.close_frame_and_batch(&mut head, block).unwrap();
        storage.close_frame_and_batch(&mut head, block).unwrap();
        storage
            .append_safe_inputs_with_timestamp(
                block,
                crate::clock::unix_now_ms() / 1_000,
                &[],
                SENDER_A,
                &protocol,
                storage::FrontierMode::Populate,
            )
            .unwrap();
        let landed = storage::StoredSafeInput {
            sender: SENDER_A,
            payload: local_batch_payload(&mut storage, 0),
            block_number: block,
        };
        drop(storage);
        let mut l1 = TestL1::new(
            &db.path,
            &protocol,
            vec![
                SyncStep::Keep,
                SyncStep::Observe {
                    block: block + 1,
                    inputs: vec![landed],
                },
            ],
        );
        recover_startup(&db.path, &protocol, &mut l1).await.unwrap();
        assert_eq!(l1.calls, ["sync", "flush", "sync"]);
        // Batch 0 became gold; the young unresolved batch 1 still belongs to
        // the flushed suffix even though it no longer trips observed danger.
        assert_eq!(invalidated_batches(&db.path), [1, 2]);
    }

    #[tokio::test]
    async fn sync_discovered_divergence_refuses_before_cascade() {
        use crate::storage::test_helpers::SENDER_A;
        let (db, protocol) = danger_fixture("recovery-sync-divergence", true);
        let block = protocol.danger_threshold();
        let foreign = storage::StoredSafeInput {
            sender: SENDER_A,
            payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                nonce: 0,
                frames: vec![],
            }),
            block_number: block + 1,
        };
        let mut l1 = TestL1::new(
            &db.path,
            &protocol,
            vec![
                SyncStep::Keep,
                SyncStep::Observe {
                    block: block + 1,
                    inputs: vec![foreign],
                },
            ],
        );
        assert_refuse(
            recover_startup(&db.path, &protocol, &mut l1)
                .await
                .unwrap_err(),
        );
        assert_eq!(l1.calls, ["sync", "flush", "sync"]);
        assert!(invalidated_batches(&db.path).is_empty());
    }

    #[tokio::test]
    async fn failed_post_flush_sync_cannot_use_the_initial_sync_fallback() {
        let (db, protocol) = danger_fixture("recovery-post-flush-offline", true);
        let mut l1 = TestL1::new(
            &db.path,
            &protocol,
            vec![
                SyncStep::Keep,
                SyncStep::Fail(InputReaderError::Provider("offline".into())),
            ],
        );
        assert_retry(
            recover_startup(&db.path, &protocol, &mut l1)
                .await
                .unwrap_err(),
        );
        assert_eq!(l1.calls, ["sync", "flush", "sync"]);
        assert!(invalidated_batches(&db.path).is_empty());
    }

    #[tokio::test]
    async fn retry_discards_the_flush_observation_and_flushes_again() {
        let (db, protocol) = danger_fixture("recovery-flush-floor", true);
        let mut interrupted =
            TestL1::new(&db.path, &protocol, vec![SyncStep::Keep, SyncStep::Keep]);
        interrupted.flush_block += 1;
        let error = recover_startup(&db.path, &protocol, &mut interrupted)
            .await
            .unwrap_err();
        assert!(matches!(error, RecoveryError::Retry(failure)
            if matches!(*failure, RecoveryFailure::PolicyRetry(RecoveryRetryReason::ResyncBehindFlushView { .. }))));
        assert_eq!(interrupted.calls, ["sync", "flush", "sync"]);
        assert!(invalidated_batches(&db.path).is_empty());
        drop(interrupted);

        let mut restarted = TestL1::new(&db.path, &protocol, vec![SyncStep::Keep, SyncStep::Keep]);
        recover_startup(&db.path, &protocol, &mut restarted)
            .await
            .unwrap();
        assert_eq!(restarted.calls, ["sync", "flush", "sync"]);
        assert_eq!(invalidated_batches(&db.path), [0, 1]);
    }

    #[tokio::test]
    async fn observed_tip_repair_still_refuses_a_surviving_clock_fault() {
        let (db, protocol) = danger_fixture("recovery-tip-clock", false);
        let ahead = crate::clock::unix_now_ms() + protocol.seconds_per_block * 2_000;
        storage::Storage::open_writer(&db.path)
            .unwrap()
            .write(|tx| {
                tx.execute(
                    "UPDATE l1_safe_head SET synced_at_ms = ?1",
                    [i64::try_from(ahead).unwrap()],
                )
            })
            .unwrap();
        let mut l1 = TestL1::new(&db.path, &protocol, vec![SyncStep::Keep]);
        let error = recover_startup(&db.path, &protocol, &mut l1)
            .await
            .unwrap_err();
        assert!(matches!(error, RecoveryError::Retry(failure)
            if matches!(*failure, RecoveryFailure::PolicyRetry(RecoveryRetryReason::DangerPersists { status: DangerStatus::L1ViewStale }))));
        assert_eq!(invalidated_batches(&db.path), [0]);
        assert_eq!(l1.calls, ["sync"]);
    }

    #[test]
    fn final_admission_rechecks_view_freshness_after_preparation() {
        let (db, protocol) = admission_fixture("admit-stale-view", true, true);
        let _admission = admit_runtime(&db.path, &protocol).unwrap();
        storage::Storage::open_writer(&db.path)
            .unwrap()
            .write(|tx| tx.execute("UPDATE l1_safe_head SET block_timestamp = 0", []))
            .unwrap();
        assert_retry(admit_runtime(&db.path, &protocol).unwrap_err());
    }

    #[test]
    fn final_admission_refuses_new_divergence() {
        let (db, protocol) = admission_fixture("admit-divergence", true, true);
        let mut storage = storage::Storage::open_writer(&db.path).expect("open writer");
        crate::storage::test_helpers::record_canonical_divergence(&mut storage, 7, 0);
        drop(storage);

        let error =
            admit_runtime(&db.path, &protocol).expect_err("divergence must refuse final admission");
        assert!(matches!(
            error,
            RecoveryError::Refuse(failure)
                if matches!(
                    *failure,
                    RecoveryFailure::PolicyRefusal(
                        RecoveryRefusalReason::CanonicalDivergence { nonce: 7 }
                    )
                )
        ));
    }

    #[test]
    fn final_admission_retries_when_tip_disappeared() {
        let (db, protocol) = admission_fixture("admit-no-tip", true, false);

        let error = admit_runtime(&db.path, &protocol)
            .expect_err("a missing Tip requires a fresh recovery attempt");
        assert!(matches!(
            error,
            RecoveryError::Retry(failure)
                if matches!(
                    *failure,
                    RecoveryFailure::PolicyRetry(
                        RecoveryRetryReason::AdmissionChanged {
                            decision: "ensure_open_tip"
                        }
                    )
                )
        ));
    }

    #[test]
    fn final_admission_refuses_missing_snapshot() {
        let (db, protocol) = admission_fixture("admit-no-snapshot", false, true);

        let error = admit_runtime(&db.path, &protocol)
            .expect_err("a missing finalized snapshot must refuse final admission");
        assert!(matches!(
            error,
            RecoveryError::Refuse(failure)
                if matches!(
                    *failure,
                    RecoveryFailure::PolicyRefusal(
                        RecoveryRefusalReason::MissingFinalizedSnapshot
                    )
                )
        ));
    }
}
