// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Run-start recovery authority.
//!
//! A pure reducer selects exactly one phase from one transactionally
//! consistent local inspection. The driver executes at most that phase and
//! always returns to local inspection before another phase or runtime
//! admission. Canonical divergence is therefore an absorbing local fact, not
//! a special check each effect must remember.
//!
//! The flush and post-flush-sync witnesses live only for this boot attempt. A
//! crash loses them and the next boot repeats the idempotent flush; no durable
//! recovery-phase state machine is introduced. See `docs/recovery/README.md`
//! and `docs/recovery/admission.tla`.

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
    #[error("provider: {0}")]
    Provider(String),
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
}

/// Single-use proof that the run's final admission decision — the same pure
/// reducer over one transactionally consistent fact set — selected `Admit`
/// after all fallible preparation completed. Its private field makes
/// construction exclusive to [`admit_runtime`]; runtime code may consume the
/// proof but cannot mint one.
#[must_use = "runtime admission must be consumed by PreparedRuntime::admit"]
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

/// The phase-ordering state machine of one boot attempt. `Flushed` and
/// `PostFlushSynced` carry the flush observation as an ephemeral,
/// memory-only witness: `drive_recovery` is its only writer, phases are its
/// only source, and it never persists — a restarted attempt has no witness
/// and must flush again. Cascade is therefore reachable only through
/// Flush → Sync *in this process* (the ADR's recovery-reducer mechanism).
/// This one enum is both the
/// reducer's input and the driver's completion type; the previous
/// `RecoveryState`/witness-struct/`PhaseCompletion` triple encoded the same
/// five variants three times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryProgress {
    NeedInitialSync,
    Inspecting,
    Flushed { observed_safe_block: u64 },
    PostFlushSynced { required_safe_block: u64 },
    Repaired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryPhase {
    InitialSync,
    EnsureOpenTip,
    RecoverTip { expected_batch_index: u64 },
    Flush,
    PostFlushSync { required_safe_block: u64 },
    Cascade { required_safe_block: u64 },
}

impl RecoveryPhase {
    fn label(self) -> &'static str {
        match self {
            Self::InitialSync => "initial_sync",
            Self::EnsureOpenTip => "ensure_open_tip",
            Self::RecoverTip { .. } => "recover_tip",
            Self::Flush => "flush",
            Self::PostFlushSync { .. } => "post_flush_sync",
            Self::Cascade { .. } => "cascade",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryDecision {
    Admit,
    Act(RecoveryPhase),
    Retry(RecoveryRetryReason),
    Refuse(RecoveryRefusalReason),
}

impl RecoveryDecision {
    fn label(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::Act(phase) => phase.label(),
            Self::Retry(_) => "retry",
            Self::Refuse(_) => "refuse",
        }
    }
}

/// The sole startup policy function. Terminal local facts are ranked before
/// phase progress, so no provider call or mutation can mask divergence or a
/// missing finalized state.
fn reduce_recovery(progress: RecoveryProgress, facts: RecoveryInspection) -> RecoveryDecision {
    if let DangerStatus::CanonicalDivergence(nonce) = facts.danger {
        return RecoveryDecision::Refuse(RecoveryRefusalReason::CanonicalDivergence { nonce });
    }
    if !facts.has_finalized_snapshot {
        return RecoveryDecision::Refuse(RecoveryRefusalReason::MissingFinalizedSnapshot);
    }

    match progress {
        RecoveryProgress::NeedInitialSync => RecoveryDecision::Act(RecoveryPhase::InitialSync),
        RecoveryProgress::Flushed {
            observed_safe_block,
        } => RecoveryDecision::Act(RecoveryPhase::PostFlushSync {
            required_safe_block: observed_safe_block,
        }),
        RecoveryProgress::PostFlushSynced {
            required_safe_block,
        } => match facts.current_safe_block {
            None => RecoveryDecision::Refuse(RecoveryRefusalReason::MissingSafeHead),
            Some(resynced_safe_block) if resynced_safe_block < required_safe_block => {
                RecoveryDecision::Retry(RecoveryRetryReason::ResyncBehindFlushView {
                    resynced_safe_block,
                    flush_observed_safe_block: required_safe_block,
                })
            }
            Some(_) => RecoveryDecision::Act(RecoveryPhase::Cascade {
                required_safe_block,
            }),
        },
        RecoveryProgress::Repaired => match facts.danger {
            DangerStatus::Safe if facts.has_open_tip => RecoveryDecision::Admit,
            // Unreachable in production — every repair phase ends with a
            // valid open tip in its own transaction (the admission model
            // encodes that postcondition). Kept so the `Repaired` and
            // `Inspecting` arms stay structurally parallel and total.
            DangerStatus::Safe => RecoveryDecision::Act(RecoveryPhase::EnsureOpenTip),
            // Named, not a wildcard: a new `DangerStatus` variant must be
            // classified here explicitly instead of silently defaulting to
            // retry — the exact mistake this reducer exists to make
            // compiler-visible.
            status @ (DangerStatus::ClosedBatchInDanger(_)
            | DangerStatus::TipInDanger(_)
            | DangerStatus::L1ViewStale
            | DangerStatus::EstimatedBatchInDanger(_)) => {
                RecoveryDecision::Retry(RecoveryRetryReason::DangerPersists { status })
            }
            DangerStatus::CanonicalDivergence(_) => {
                unreachable!("terminal facts were reduced before progress")
            }
        },
        RecoveryProgress::Inspecting => match facts.danger {
            DangerStatus::Safe if facts.has_open_tip => RecoveryDecision::Admit,
            DangerStatus::Safe => RecoveryDecision::Act(RecoveryPhase::EnsureOpenTip),
            DangerStatus::ClosedBatchInDanger(_) => RecoveryDecision::Act(RecoveryPhase::Flush),
            DangerStatus::TipInDanger(expected_batch_index) => {
                RecoveryDecision::Act(RecoveryPhase::RecoverTip {
                    expected_batch_index,
                })
            }
            DangerStatus::L1ViewStale => RecoveryDecision::Retry(RecoveryRetryReason::L1ViewStale),
            DangerStatus::EstimatedBatchInDanger(batch_index) => {
                RecoveryDecision::Retry(RecoveryRetryReason::EstimatedBatchInDanger { batch_index })
            }
            DangerStatus::CanonicalDivergence(_) => {
                unreachable!("terminal facts were reduced before progress")
            }
        },
    }
}

trait RecoveryDriver {
    fn inspect(&mut self) -> Result<RecoveryInspection, RecoveryError>;

    /// Perform one phase and return the progress it established. The
    /// production driver derives it from the phase itself, so a
    /// wrong-progress return is unrepresentable there.
    async fn perform(&mut self, phase: RecoveryPhase) -> Result<RecoveryProgress, RecoveryError>;

    fn admitted(&mut self) {}
}

/// Drive one phase per inspection. There is intentionally no edge from a
/// completed phase directly to another phase or admission.
async fn drive_recovery(driver: &mut impl RecoveryDriver) -> Result<(), RecoveryError> {
    let mut progress = RecoveryProgress::NeedInitialSync;
    loop {
        let facts = driver.inspect()?;
        let decision = reduce_recovery(progress, facts);
        tracing::info!(
            recovery_progress = ?progress,
            danger_status = facts.danger.label(),
            danger_batch_index = ?facts.danger.batch_index(),
            recovery_decision = decision.label(),
            "startup recovery reducer decision"
        );

        match decision {
            RecoveryDecision::Admit => {
                driver.admitted();
                return Ok(());
            }
            RecoveryDecision::Retry(reason) => return Err(RecoveryError::retry(reason)),
            RecoveryDecision::Refuse(reason) => return Err(RecoveryError::refuse(reason)),
            RecoveryDecision::Act(phase) => {
                progress = driver.perform(phase).await?;
            }
        }
    }
}

fn log_repair(invalidated: &[u64]) {
    if invalidated.is_empty() {
        tracing::info!("startup recovery phase completed without invalidation");
    } else {
        tracing::warn!(
            invalidated_count = invalidated.len(),
            batches = ?invalidated,
            "startup recovery invalidated the doomed suffix"
        );
    }
}

struct ProductionRecoveryDriver<'a> {
    db_path: &'a str,
    input_reader: &'a mut InputReader,
    l1_config: &'a L1Config,
    protocol: &'a ProtocolTiming,
}

impl RecoveryDriver for ProductionRecoveryDriver<'_> {
    fn inspect(&mut self) -> Result<RecoveryInspection, RecoveryError> {
        let mut storage = storage::Storage::open_writer(self.db_path).map_err(classify_open)?;
        storage
            .inspect_recovery(self.protocol, crate::clock::unix_now_ms())
            .map_err(classify_storage)
    }

    async fn perform(&mut self, phase: RecoveryPhase) -> Result<RecoveryProgress, RecoveryError> {
        match phase {
            RecoveryPhase::InitialSync => {
                match self.input_reader.sync_to_current_safe_head().await {
                    Ok(()) => tracing::info!("L1 safe head synced"),
                    // Preserve warm boot: an unreachable provider counts as a
                    // completed refresh attempt, then persisted local facts
                    // decide whether serving is still honest.
                    Err(InputReaderError::Provider(error)) => tracing::warn!(
                        error = %error,
                        "L1 unreachable during initial startup sync; inspecting persisted view"
                    ),
                    Err(error) => return Err(classify_input_reader(error)),
                }
                Ok(RecoveryProgress::Inspecting)
            }
            RecoveryPhase::EnsureOpenTip => {
                let mut storage =
                    storage::Storage::open_writer(self.db_path).map_err(classify_open)?;
                storage
                    .ensure_open_tip_for_recovery(self.protocol, crate::clock::unix_now_ms())
                    .map_err(classify_mutation)?;
                log_repair(&[]);
                Ok(RecoveryProgress::Repaired)
            }
            RecoveryPhase::RecoverTip {
                expected_batch_index,
            } => {
                let mut storage =
                    storage::Storage::open_writer(self.db_path).map_err(classify_open)?;
                let invalidated = storage
                    .recover_aging_tip_for_recovery(
                        expected_batch_index,
                        self.protocol,
                        crate::clock::unix_now_ms(),
                    )
                    .map_err(classify_mutation)?;
                log_repair(&invalidated);
                Ok(RecoveryProgress::Repaired)
            }
            RecoveryPhase::Flush => {
                let observed_safe_block = self.flush().await?;
                Ok(RecoveryProgress::Flushed {
                    observed_safe_block,
                })
            }
            RecoveryPhase::PostFlushSync {
                required_safe_block,
            } => {
                self.input_reader
                    .sync_to_current_safe_head()
                    .await
                    .map_err(classify_input_reader)?;
                Ok(RecoveryProgress::PostFlushSynced {
                    required_safe_block,
                })
            }
            RecoveryPhase::Cascade {
                required_safe_block,
            } => {
                let mut storage =
                    storage::Storage::open_writer(self.db_path).map_err(classify_open)?;
                let invalidated = storage
                    .recover_post_flush_for_recovery(
                        required_safe_block,
                        self.protocol,
                        crate::clock::unix_now_ms(),
                    )
                    .map_err(classify_mutation)?;
                log_repair(&invalidated);
                Ok(RecoveryProgress::Repaired)
            }
        }
    }
}

impl ProductionRecoveryDriver<'_> {
    async fn flush(&mut self) -> Result<u64, RecoveryError> {
        use crate::l1::provider::VerifiedSignerProviderError;

        let provider = crate::l1::provider::create_verified_signer_provider(
            &self.l1_config.eth_rpc_url,
            &self.l1_config.batch_submitter_private_key,
            self.l1_config.chain_id,
            self.l1_config.allow_insecure_rpc,
        )
        .await
        .map_err(|error| match error {
            VerifiedSignerProviderError::ChainIdMismatch { rpc, expected } => {
                RecoveryError::refuse(RecoveryFailure::ChainIdMismatch { rpc, expected })
            }
            VerifiedSignerProviderError::ChainIdRpc(message) => {
                RecoveryError::retry(RecoveryFailure::Provider(message))
            }
            VerifiedSignerProviderError::Create(message) => {
                RecoveryError::refuse(RecoveryFailure::Provider(message))
            }
        })?;

        let watermark = {
            let mut storage = storage::Storage::open_writer(self.db_path).map_err(classify_open)?;
            storage.wallet_nonce_watermark().map_err(classify_storage)?
        };
        MempoolFlusher::flush_to_safe(
            provider,
            self.l1_config.batch_submitter_address,
            self.protocol.seconds_per_block,
            self.db_path,
            watermark,
        )
        .await
        .map_err(classify_flush)
    }
}

/// Run the startup reducer through its first clean `Admit` decision. This
/// grants no runtime capability; fallible runtime preparation follows, then
/// [`admit_runtime`] invokes the same reducer once more over one consistent
/// fact set.
pub(crate) async fn run_startup_recovery(
    db_path: &str,
    input_reader: &mut InputReader,
    l1_config: &L1Config,
    protocol: &ProtocolTiming,
) -> Result<(), RecoveryError> {
    let mut driver = ProductionRecoveryDriver {
        db_path,
        input_reader,
        l1_config,
        protocol,
    };
    drive_recovery(&mut driver).await
}

/// Reinvoke the same reducer after all fallible preparation, over one
/// transactionally consistent fact set. Anything except `Admit` drops the
/// prepared resources and restarts from a fresh boot; workers are never
/// launched from an aged decision.
///
/// This consistent read *is* the linearization of the final admission
/// decision: the process lock excludes every other process, and no worker
/// is launched until after this decision — the launch step itself is
/// non-yielding — so no writer exists that could invalidate the facts
/// between this read and worker launch.
pub(crate) fn admit_runtime(
    db_path: &str,
    protocol: &ProtocolTiming,
) -> Result<RuntimeAdmission, RecoveryError> {
    let mut storage = storage::Storage::open_writer(db_path).map_err(classify_open)?;
    let facts = storage
        .inspect_recovery(protocol, crate::clock::unix_now_ms())
        .map_err(classify_storage)?;
    match reduce_recovery(RecoveryProgress::Inspecting, facts) {
        RecoveryDecision::Admit => Ok(RuntimeAdmission { _private: () }),
        RecoveryDecision::Retry(reason) => Err(RecoveryError::retry(reason)),
        RecoveryDecision::Refuse(reason) => Err(RecoveryError::refuse(reason)),
        RecoveryDecision::Act(phase) => Err(RecoveryError::retry(
            RecoveryRetryReason::AdmissionChanged {
                decision: phase.label(),
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
        assert_refuse(classify_input_reader(InputReaderError::ChainIdMismatch {
            rpc: 1,
            expected: 2,
        }));

        assert_retry(classify_flush(FlushError::Provider("offline".into())));
        assert_refuse(classify_flush(FlushError::Watermark(
            WalletNonceWatermarkError::Storage(rusqlite::Error::QueryReturnedNoRows),
        )));

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

    fn facts(danger: DangerStatus) -> RecoveryInspection {
        RecoveryInspection {
            danger,
            has_finalized_snapshot: true,
            has_open_tip: true,
            current_safe_block: Some(1_200),
        }
    }

    #[test]
    fn divergence_dominates_every_progress_state() {
        let terminal = facts(DangerStatus::CanonicalDivergence(7));
        for progress in [
            RecoveryProgress::NeedInitialSync,
            RecoveryProgress::Inspecting,
            RecoveryProgress::Flushed {
                observed_safe_block: 1_201,
            },
            RecoveryProgress::PostFlushSynced {
                required_safe_block: 1_201,
            },
            RecoveryProgress::Repaired,
        ] {
            assert_eq!(
                reduce_recovery(progress, terminal),
                RecoveryDecision::Refuse(RecoveryRefusalReason::CanonicalDivergence { nonce: 7 })
            );
        }
    }

    #[test]
    fn post_flush_lag_retries_before_cascade() {
        let decision = reduce_recovery(
            RecoveryProgress::PostFlushSynced {
                required_safe_block: 1_201,
            },
            facts(DangerStatus::ClosedBatchInDanger(0)),
        );
        assert_eq!(
            decision,
            RecoveryDecision::Retry(RecoveryRetryReason::ResyncBehindFlushView {
                resynced_safe_block: 1_200,
                flush_observed_safe_block: 1_201,
            })
        );
    }

    #[test]
    fn repaired_tip_with_surviving_clock_refusal_cannot_admit() {
        assert_eq!(
            reduce_recovery(RecoveryProgress::Repaired, facts(DangerStatus::L1ViewStale)),
            RecoveryDecision::Retry(RecoveryRetryReason::DangerPersists {
                status: DangerStatus::L1ViewStale
            })
        );
    }

    struct ScriptedDriver {
        inspections: VecDeque<RecoveryInspection>,
        trace: Vec<&'static str>,
        flush_observed_safe_block: u64,
        inspection_attempts: usize,
        fail_inspection_at: Option<usize>,
    }

    impl ScriptedDriver {
        fn new(inspections: impl IntoIterator<Item = RecoveryInspection>) -> Self {
            Self {
                inspections: inspections.into_iter().collect(),
                trace: Vec::new(),
                flush_observed_safe_block: 1_200,
                inspection_attempts: 0,
                fail_inspection_at: None,
            }
        }

        fn fail_inspection_at(mut self, attempt: usize) -> Self {
            self.fail_inspection_at = Some(attempt);
            self
        }
    }

    impl RecoveryDriver for ScriptedDriver {
        fn inspect(&mut self) -> Result<RecoveryInspection, RecoveryError> {
            self.trace.push("inspect");
            self.inspection_attempts += 1;
            if self.fail_inspection_at == Some(self.inspection_attempts) {
                return Err(RecoveryError::retry(RecoveryRetryReason::L1ViewStale));
            }
            Ok(self
                .inspections
                .pop_front()
                .expect("script provides one fact set per inspection"))
        }

        async fn perform(
            &mut self,
            phase: RecoveryPhase,
        ) -> Result<RecoveryProgress, RecoveryError> {
            Ok(match phase {
                RecoveryPhase::InitialSync => {
                    self.trace.push("initial_sync");
                    RecoveryProgress::Inspecting
                }
                RecoveryPhase::EnsureOpenTip => {
                    self.trace.push("ensure_tip");
                    RecoveryProgress::Repaired
                }
                RecoveryPhase::RecoverTip { .. } => {
                    self.trace.push("recover_tip");
                    RecoveryProgress::Repaired
                }
                RecoveryPhase::Flush => {
                    self.trace.push("flush");
                    RecoveryProgress::Flushed {
                        observed_safe_block: self.flush_observed_safe_block,
                    }
                }
                RecoveryPhase::PostFlushSync {
                    required_safe_block,
                } => {
                    self.trace.push("post_flush_sync");
                    RecoveryProgress::PostFlushSynced {
                        required_safe_block,
                    }
                }
                RecoveryPhase::Cascade { .. } => {
                    self.trace.push("cascade");
                    RecoveryProgress::Repaired
                }
            })
        }

        fn admitted(&mut self) {
            self.trace.push("admit");
        }
    }

    #[tokio::test]
    async fn closed_recovery_runs_exactly_one_phase_per_inspection() {
        let closed = facts(DangerStatus::ClosedBatchInDanger(0));
        let mut driver =
            ScriptedDriver::new([closed, closed, closed, closed, facts(DangerStatus::Safe)]);

        drive_recovery(&mut driver).await.expect("admit");
        assert_eq!(
            driver.trace,
            [
                "inspect",
                "initial_sync",
                "inspect",
                "flush",
                "inspect",
                "post_flush_sync",
                "inspect",
                "cascade",
                "inspect",
                "admit",
            ]
        );
    }

    #[tokio::test]
    async fn local_divergence_refuses_before_every_phase() {
        let mut driver = ScriptedDriver::new([facts(DangerStatus::CanonicalDivergence(9))]);
        let error = drive_recovery(&mut driver)
            .await
            .expect_err("divergence refuses");
        assert!(matches!(error, RecoveryError::Refuse(_)));
        assert_eq!(driver.trace, ["inspect"]);
    }

    #[tokio::test]
    async fn sync_discovered_divergence_stops_before_cascade() {
        let closed = facts(DangerStatus::ClosedBatchInDanger(0));
        let mut driver = ScriptedDriver::new([
            closed,
            closed,
            closed,
            facts(DangerStatus::CanonicalDivergence(0)),
        ]);
        let error = drive_recovery(&mut driver)
            .await
            .expect_err("divergence discovered by sync refuses");
        assert!(matches!(error, RecoveryError::Refuse(_)));
        assert_eq!(
            driver.trace,
            [
                "inspect",
                "initial_sync",
                "inspect",
                "flush",
                "inspect",
                "post_flush_sync",
                "inspect",
            ]
        );
    }

    #[tokio::test]
    async fn tip_repair_reinspects_and_retries_on_surviving_clock_refusal() {
        let tip = facts(DangerStatus::TipInDanger(0));
        let mut driver = ScriptedDriver::new([tip, tip, facts(DangerStatus::L1ViewStale)]);

        let error = drive_recovery(&mut driver)
            .await
            .expect_err("clock refusal must block admission after repair");
        assert!(matches!(error, RecoveryError::Retry(_)));
        assert_eq!(
            driver.trace,
            [
                "inspect",
                "initial_sync",
                "inspect",
                "recover_tip",
                "inspect",
            ]
        );
    }

    #[tokio::test]
    async fn reconstructed_controller_cannot_reuse_a_post_flush_sync_witness() {
        let closed = facts(DangerStatus::ClosedBatchInDanger(0));
        let mut interrupted = ScriptedDriver::new([closed, closed, closed]).fail_inspection_at(4);

        let error = drive_recovery(&mut interrupted)
            .await
            .expect_err("the injected inspection boundary ends this controller");
        assert!(matches!(error, RecoveryError::Retry(_)));
        assert_eq!(
            interrupted.trace,
            [
                "inspect",
                "initial_sync",
                "inspect",
                "flush",
                "inspect",
                "post_flush_sync",
                "inspect",
            ],
            "the first controller reached post-flush Sync before it was lost"
        );

        // Reconstructing the Rust controller is the restart boundary: its
        // non-clone witnesses are gone. Even though durable facts still show
        // the same closed danger, the new attempt must InitialSync and Flush;
        // it cannot jump straight to Cascade using the previous attempt's
        // PostFlushSync witness.
        let mut restarted =
            ScriptedDriver::new([closed, closed, closed, closed, facts(DangerStatus::Safe)]);
        drive_recovery(&mut restarted)
            .await
            .expect("restart admits");
        assert_eq!(
            restarted.trace,
            [
                "inspect",
                "initial_sync",
                "inspect",
                "flush",
                "inspect",
                "post_flush_sync",
                "inspect",
                "cascade",
                "inspect",
                "admit",
            ]
        );
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
