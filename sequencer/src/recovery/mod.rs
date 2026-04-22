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

use alloy_primitives::Address;
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
    match input_reader.sync_to_current_safe_head().await {
        Ok(()) => {
            tracing::info!("L1 safe head synced");
        }
        Err(e) => {
            let InputReaderError::Provider(error) = e else {
                return Err(RecoveryError::InputReader(e));
            };
            tracing::error!(error = %error, "L1 unreachable during startup safe-head sync");

            // L1 is down. Estimate whether the frontier batch has crossed the danger
            // threshold since the last successful sync.
            let in_danger = wall_clock_danger_estimate(db_path, batch_submitter_address, params)?;

            if let Some(batch_index) = in_danger {
                tracing::error!(
                    batch_index,
                    "wall-clock estimate indicates danger zone during startup outage"
                );
                return Err(RecoveryError::StartupDangerZoneEstimate);
            }

            tracing::info!(
                "L1 unreachable but wall-clock estimate is before danger zone — \
                 proceeding with stale safe head"
            );
        }
    }

    if let Some(batch_index) =
        stalled_safe_head_danger_estimate(db_path, batch_submitter_address, params)?
    {
        tracing::error!(
            batch_index,
            "safe head has not progressed and the estimated frontier is in danger zone at startup"
        );
        return Err(RecoveryError::StartupDangerZoneEstimate);
    }

    // ── Step 2: Populate frontier + check danger zone ───────────────
    let needs_flush = {
        let mut det_storage = storage::Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
        det_storage.refresh_recovery_metadata(batch_submitter_address, max_wait_blocks)?;
        det_storage.check_danger_zone(danger_threshold)?
    };

    if let Some(batch_index) = needs_flush {
        tracing::error!(
            batch_index,
            danger_threshold,
            max_wait_blocks,
            "danger zone detected — entering preemptive recovery"
        );

        // ── Step 3: Flush mempool ──────────────────────────────────
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
    } else {
        tracing::info!("no danger zone detected — skipping flush");
    }

    // ── Step 4: Atomic recovery ────────────────────────────────────
    tracing::info!("running startup recovery (populate frontier, assign nonces, detect stale)");
    let mut det_storage = storage::Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;
    let invalidated = det_storage.run_startup_recovery(batch_submitter_address, max_wait_blocks)?;

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

/// Estimate whether we're in the danger zone using wall-clock time.
///
/// Reads the persisted safe-progress timestamp from the DB. Estimates how many
/// blocks have elapsed since then using `seconds_per_block`, then adjusts the
/// frontier-based danger check by that many missed blocks. Returns the frontier
/// batch index if it is estimated to have crossed the danger threshold.
///
/// This is the same check the batch submitter uses at runtime. Both ask:
/// "given the frontier age at our last safe-head progress, how much additional
/// age should we attribute to the outage?"
pub(crate) fn wall_clock_danger_estimate(
    db_path: &str,
    batch_submitter_address: Address,
    params: RecoveryParams,
) -> Result<Option<u64>, RecoveryError> {
    let RecoveryParams {
        max_wait_blocks,
        danger_threshold,
        seconds_per_block,
    } = params;
    let mut storage = storage::Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;

    let last_sync_ms = storage.last_safe_progress_ms()?;

    if last_sync_ms == 0 {
        // Never synced — first startup. L1 is required.
        tracing::error!(
            "no previous safe-head observation recorded — L1 is required for first startup"
        );
        return Err(RecoveryError::StartupDangerZoneEstimate);
    }

    let (elapsed_secs, estimated_missed_blocks) =
        estimate_missed_blocks_since(last_sync_ms, seconds_per_block);
    let adjusted_threshold = danger_threshold.saturating_sub(estimated_missed_blocks);

    storage.refresh_recovery_metadata(batch_submitter_address, max_wait_blocks)?;
    // Use the unified check here (not `check_danger_zone`): if L1 is
    // unreachable, we want to refuse to boot whenever *any* unresolved batch
    // may be past the threshold, including the open batch. `check_danger_zone`
    // is narrower (closed batches only, for zombie detection by the live
    // submitter) and would miss an aging open batch.
    let estimated_danger_batch =
        storage.check_any_unresolved_batch_in_danger(adjusted_threshold)?;

    if let Some(batch_index) = estimated_danger_batch {
        tracing::error!(
            batch_index,
            estimated_missed_blocks,
            elapsed_secs,
            danger_threshold,
            adjusted_threshold,
            "wall-clock danger estimate: frontier is estimated to be in danger zone"
        );
        Ok(Some(batch_index))
    } else {
        tracing::info!(
            estimated_missed_blocks,
            danger_threshold,
            adjusted_threshold,
            "wall-clock danger estimate: before danger zone"
        );
        Ok(None)
    }
}

/// Estimate danger when L1 remains reachable but the safe frontier has failed
/// to advance for at least one expected block interval.
pub(crate) fn stalled_safe_head_danger_estimate(
    db_path: &str,
    batch_submitter_address: Address,
    params: RecoveryParams,
) -> Result<Option<u64>, RecoveryError> {
    let RecoveryParams {
        max_wait_blocks,
        danger_threshold,
        seconds_per_block,
    } = params;
    let mut storage = storage::Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA)?;

    let last_sync_ms = storage.last_safe_progress_ms()?;
    if last_sync_ms == 0 {
        return Ok(None);
    }

    let (elapsed_secs, estimated_missed_blocks) =
        estimate_missed_blocks_since(last_sync_ms, seconds_per_block);
    if estimated_missed_blocks == 0 {
        return Ok(None);
    }

    let adjusted_threshold = danger_threshold.saturating_sub(estimated_missed_blocks);
    storage.refresh_recovery_metadata(batch_submitter_address, max_wait_blocks)?;
    let estimated_danger_batch =
        storage.check_any_unresolved_batch_in_danger(adjusted_threshold)?;

    if let Some(batch_index) = estimated_danger_batch {
        tracing::error!(
            batch_index,
            estimated_missed_blocks,
            elapsed_secs,
            danger_threshold,
            adjusted_threshold,
            "safe-head stall estimate: frontier is estimated to be in danger zone"
        );
        Ok(Some(batch_index))
    } else {
        tracing::info!(
            estimated_missed_blocks,
            danger_threshold,
            adjusted_threshold,
            "safe-head stall estimate: before danger zone"
        );
        Ok(None)
    }
}

fn estimate_missed_blocks_since(last_sync_ms: u64, seconds_per_block: u64) -> (u64, u64) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let elapsed_secs = now_ms.saturating_sub(last_sync_ms) / 1000;
    let estimated_missed_blocks = elapsed_secs / seconds_per_block;
    (elapsed_secs, estimated_missed_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::temp_db;
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput};

    const SQLITE_SYNCHRONOUS_PRAGMA: &str = "NORMAL";
    const BATCH_SUBMITTER: Address = Address::repeat_byte(0xAA);

    fn set_last_safe_progress_ms(db_path: &str, synced_at_ms: u64) {
        let conn = Storage::open_connection(db_path, SQLITE_SYNCHRONOUS_PRAGMA)
            .expect("open raw sqlite connection");
        conn.execute(
            "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
            [i64::try_from(synced_at_ms).unwrap_or(i64::MAX)],
        )
        .expect("update sync timestamp");
    }

    fn batch_payload(nonce: u64, safe_block: u64) -> Vec<u8> {
        ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce,
            frames: vec![sequencer_core::batch::Frame {
                safe_block,
                fee_price: 0,
                user_ops: vec![],
            }],
        })
    }

    #[test]
    fn wall_clock_danger_estimate_requires_previous_real_sync() {
        let db = temp_db("wall-clock-first-startup");

        let err = wall_clock_danger_estimate(
            &db.path,
            BATCH_SUBMITTER,
            RecoveryParams {
                max_wait_blocks: 1200,
                danger_threshold: 1125,
                seconds_per_block: 12,
            },
        )
        .expect_err("first startup without L1 sync should block");
        assert!(matches!(err, RecoveryError::StartupDangerZoneEstimate));
    }

    #[test]
    fn wall_clock_danger_estimate_accounts_for_frontier_age_at_last_sync() {
        let db = temp_db("wall-clock-frontier-age");
        let mut storage = Storage::open(&db.path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");

        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 1");

        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: BATCH_SUBMITTER,
                    payload: batch_payload(0, 100),
                    block_number: 200,
                }],
            )
            .expect("append accepted batch");
        drop(storage);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let missed_blocks = 25_u64;
        set_last_safe_progress_ms(&db.path, now_ms.saturating_sub(missed_blocks * 12 * 1000));

        let batch_index = wall_clock_danger_estimate(
            &db.path,
            BATCH_SUBMITTER,
            RecoveryParams {
                max_wait_blocks: 1200,
                danger_threshold: 1125,
                seconds_per_block: 12,
            },
        )
        .expect("wall clock estimate should succeed");
        assert_eq!(
            batch_index,
            Some(1),
            "frontier already 1100 blocks old should trip after 25 missed blocks"
        );
    }

    #[test]
    fn stalled_safe_head_danger_estimate_requires_elapsed_progress_gap() {
        let db = temp_db("stall-estimate-needs-gap");
        let mut storage = Storage::open(&db.path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
        storage
            .append_safe_inputs(1200, &[])
            .expect("record current safe progress");
        drop(storage);

        let batch_index = stalled_safe_head_danger_estimate(
            &db.path,
            BATCH_SUBMITTER,
            RecoveryParams {
                max_wait_blocks: 1200,
                danger_threshold: 1125,
                seconds_per_block: 12,
            },
        )
        .expect("stalled safe-head estimate should succeed");
        assert_eq!(batch_index, None);
    }

    #[test]
    fn stalled_safe_head_danger_estimate_uses_safe_progress_timestamp() {
        let db = temp_db("stall-estimate-frontier-age");
        let mut storage = Storage::open(&db.path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");

        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 1");

        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: BATCH_SUBMITTER,
                    payload: batch_payload(0, 100),
                    block_number: 200,
                }],
            )
            .expect("append accepted batch");
        drop(storage);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        set_last_safe_progress_ms(&db.path, now_ms.saturating_sub(25 * 12 * 1000));

        let batch_index = stalled_safe_head_danger_estimate(
            &db.path,
            BATCH_SUBMITTER,
            RecoveryParams {
                max_wait_blocks: 1200,
                danger_threshold: 1125,
                seconds_per_block: 12,
            },
        )
        .expect("stalled safe-head estimate should succeed");
        assert_eq!(batch_index, Some(1));
    }
}
