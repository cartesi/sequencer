// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Preemptive recovery: detect danger zone, flush mempool, cascade-invalidate stale batches.
//!
//! At startup the sequencer checks if any batch is approaching the staleness deadline.
//! If so, it flushes the L1 mempool (competing with pending batch transactions using
//! no-op replacements), re-syncs the safe head, and runs the atomic recovery procedure
//! (populate scheduler frontier, assign nonces, detect stale, cascade-invalidate,
//! open recovery batch).
//!
//! At runtime the batch submitter performs the same danger-zone check each tick.
//! If triggered, it returns a `DangerZone` error, which crashes the process.
//! External orchestration restarts the sequencer, and this startup path runs again.
//!
//! ## Fault model
//!
//! Recovery is designed to handle **submission and outage failures**: the sequencer
//! crashes, the L1 provider becomes unreachable, transactions are dropped from the
//! mempool, or the process is offline for an extended period. It is **not** designed
//! to handle arbitrarily malformed self-submissions. The scheduler frontier
//! reconstruction (`populate_safe_accepted_batches`) trusts that on-chain batches
//! from the sequencer's own address are structurally valid. This is a deliberate
//! system assumption, not a gap — the sequencer controls its own submissions.
//!
//! See `docs/recovery/` for the full design, TLA+ specs, and design history.

mod flusher;

use thiserror::Error;

use crate::l1::reader::{InputReader, InputReaderError};
use crate::runtime::config::L1Config;
use crate::storage::{self, StorageOpenError};
pub use flusher::MempoolFlusher;

/// Recovery thresholds and timing parameters. Bundled together so callers don't
/// have to plumb four `u64` arguments through multiple layers.
#[derive(Debug, Clone, Copy)]
pub struct RecoveryParams {
    /// Stale-batch deadline (`MAX_WAIT_BLOCKS`).
    pub max_wait_blocks: u64,
    /// `MAX_WAIT_BLOCKS - MARGIN`. Triggering threshold for preemptive recovery.
    pub danger_threshold: u64,
    /// Wall-clock fallback estimate when L1 is unreachable. Default 12 (Ethereum).
    pub seconds_per_block: u64,
}

const SQLITE_SYNCHRONOUS_PRAGMA: &str = "NORMAL";

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("flush: {0}")]
    Flush(#[from] flusher::FlushError),
    #[error("input reader: {0}")]
    InputReader(#[from] InputReaderError),
    #[error("provider: {0}")]
    Provider(String),
    #[error("startup safe-progress estimate indicates danger zone — cannot proceed safely")]
    StartupDangerZoneEstimate,
}

/// Run the full preemptive recovery procedure at startup.
///
/// 1. Try to sync the safe head from L1. If L1 is unreachable, or if the safe
///    head is reachable but appears stalled for too long, use wall-clock
///    estimation to decide whether it's safe to proceed (before danger zone)
///    or we must block (in or past danger zone).
/// 2. Check if any batch is in the danger zone (approaching staleness).
/// 3. If so, flush the mempool and re-sync the safe head.
/// 4. Run the atomic recovery transaction (populate frontier, assign nonces,
///    detect stale, cascade-invalidate, open recovery batch).
///
/// Returns the list of invalidated batch indices (empty if no stale batches).
pub async fn run_preemptive_recovery(
    db_path: &str,
    input_reader: &mut InputReader,
    l1_config: &L1Config,
    params: RecoveryParams,
) -> Result<Vec<u64>, RecoveryError> {
    let RecoveryParams {
        max_wait_blocks,
        danger_threshold,
        seconds_per_block,
    } = params;
    let batch_submitter_address = l1_config.batch_submitter_address;

    // ── Step 1: Sync safe head (tolerate L1 failure) ───────────────
    //
    // `sync_to_current_safe_head` goes through `append_safe_inputs`, which
    // maintains `safe_accepted_batches` atomically with each advance. After
    // a successful sync, the scheduler-frontier view is consistent with
    // l1_safe_head for every downstream reader.
    let l1_reachable = match input_reader.sync_to_current_safe_head().await {
        Ok(()) => {
            tracing::info!("L1 safe head synced");
            true
        }
        Err(e) => {
            let InputReaderError::Provider(error) = e else {
                return Err(RecoveryError::InputReader(e));
            };
            tracing::error!(error = %error, "L1 unreachable during startup safe-head sync");
            false
        }
    };

    // ── Step 2: Danger check ───────────────────────────────────────
    //
    // `Storage::check_danger` runs strict (block-based) + wall-clock-adjusted
    // checks in one read transaction and returns a `DangerStatus`. The
    // response depends on the check result AND on whether L1 is reachable:
    //
    //                    | L1 reachable             | L1 unreachable
    // -------------------|--------------------------|-----------------------
    // Safe               | proceed to recovery tx   | proceed with stale DB
    // Strict(idx)        | flush + resync, then tx  | refuse boot
    // Stalled(idx)       | refuse boot (*)          | refuse boot
    // never synced (**)  | (not possible)           | refuse boot
    //
    // (*) A stalled-safe-head means `flush_and_wait` would spin waiting for
    // a safe head that isn't advancing. We can't act safely — refuse boot.
    // (**) Checked explicitly before `check_danger` under L1-unreachable,
    // because `check_danger` reports never-synced as `Safe` (no baseline to
    // estimate from). Under L1-unreachable that's still a refuse-boot condition.
    let danger = {
        let mut storage = storage::Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;

        if !l1_reachable && storage.last_safe_progress_ms()? == 0 {
            tracing::error!(
                "no previous safe-head observation recorded — L1 is required for first startup"
            );
            return Err(RecoveryError::StartupDangerZoneEstimate);
        }

        storage.check_danger(params, unix_now_ms())?
    };

    match (danger, l1_reachable) {
        (storage::DangerStatus::Safe, _) => {
            tracing::info!("no danger zone detected — skipping flush");
        }
        (storage::DangerStatus::Strict(batch_index), true) => {
            tracing::error!(
                batch_index,
                danger_threshold,
                max_wait_blocks,
                "danger zone detected — entering preemptive recovery"
            );

            // ── Step 3: Flush mempool ──────────────────────────────
            let flush_provider = crate::l1::provider::create_signer_provider(
                &l1_config.eth_rpc_url,
                &l1_config.batch_submitter_private_key,
            )
            .map_err(|e| RecoveryError::Provider(e.to_string()))?;
            let flusher =
                MempoolFlusher::new(flush_provider, batch_submitter_address, seconds_per_block);
            flusher.flush_and_wait().await?;

            tracing::info!("re-syncing L1 safe head after flush");
            input_reader.sync_to_current_safe_head().await?;
        }
        (status, _) => {
            tracing::error!(
                ?status,
                reachable = l1_reachable,
                "startup refused: flush cannot run safely"
            );
            return Err(RecoveryError::StartupDangerZoneEstimate);
        }
    }

    // ── Step 4: Atomic recovery ────────────────────────────────────
    //
    // `safe_accepted_batches` is already caught up to `l1_safe_head` (step 1
    // and, if we flushed, step 3 re-synced it). The recovery transaction only
    // needs to cascade + open.
    tracing::info!("running startup recovery (detect stale, cascade-invalidate, open recovery)");
    let mut det_storage = storage::Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
    let invalidated = det_storage.run_startup_recovery(max_wait_blocks)?;

    if invalidated.is_empty() {
        tracing::info!("no stale batches found — continuing normally");
    } else {
        tracing::error!(
            count = invalidated.len(),
            batches = ?invalidated,
            "stale batches invalidated — recovery batch opened"
        );
    }

    Ok(invalidated)
}

/// Current Unix-ms wall-clock time. Shared helper for callers of
/// [`crate::storage::Storage::check_danger`].
pub fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
