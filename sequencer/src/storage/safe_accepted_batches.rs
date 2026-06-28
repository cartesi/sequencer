// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Materialized view of the scheduler-accepted batches.
//!
//! `safe_accepted_batches` caches the prefix of submitted batches that the
//! on-chain scheduler would accept, based on an off-chain simulation of its
//! acceptance rules (see [`sequencer_core::protocol::ProtocolTiming`]).
//!
//! Maintenance contract: the view is advanced atomically with each
//! [`super::Storage::append_safe_inputs`] write, so any reader that sees
//! `l1_safe_head` at block B also sees every acceptance decision up to B. No
//! caller should populate this view directly.
//!
//! Readers:
//! - batch submitter frontier / danger reads (`submitter_frontier`,
//!   `check_danger`)
//! - recovery cascade (`find_closed_frontier_batch_in_danger`)
//! - wall-clock and stalled-safe-head danger estimates
//!
//! The only writer is [`populate_safe_accepted_batches`], invoked from
//! `append_safe_inputs` inside its transaction.

use alloy_primitives::Address;
use rusqlite::{Connection, OptionalExtension, Result, params};

use super::convert::{i64_to_u64, now_unix_ms, u64_to_i64};
use super::mutations::batch_tree_anchor_in;
use sequencer_core::protocol::{AcceptedBatch, ProtocolTiming, SafeInputView};

/// One row of `safe_accepted_batches`, exposing just the columns the
/// frontier-read code paths need.
#[derive(Debug, Clone, Copy)]
pub(super) struct SafeAcceptedBatchRow {
    pub safe_input_index: i64,
    pub nonce: i64,
}

/// The most recently accepted row, or `None` if the view is empty.
pub(super) fn query_latest_safe_accepted_batch(
    conn: &Connection,
) -> Result<Option<SafeAcceptedBatchRow>> {
    conn.query_row(
        "SELECT safe_input_index, nonce FROM safe_accepted_batches \
         ORDER BY safe_input_index DESC LIMIT 1",
        [],
        |row| {
            Ok(SafeAcceptedBatchRow {
                safe_input_index: row.get(0)?,
                nonce: row.get(1)?,
            })
        },
    )
    .optional()
}

/// Next nonce the scheduler is expected to accept — the gold frontier's
/// "expected next" cursor.
///
/// Returns `latest_accepted.nonce + 1` if any batch has been accepted, else `0`.
/// Equivalently, the nonce that the very next valid closed batch (the cascade
/// pivot, when one exists) will carry, by the contiguity invariant on the
/// valid path (`trg_enforce_nonce_contiguity`).
pub(super) fn frontier_nonce(conn: &Connection) -> Result<u64> {
    match query_latest_safe_accepted_batch(conn)? {
        Some(row) => Ok(i64_to_u64(row.nonce).saturating_add(1)),
        // Empty accepted table ⇒ the frontier sits at the batch-tree anchor: 0
        // for a genesis deployment, N' for a cockroach-recovered one. Reading
        // the anchor (not a hard-coded 0) keeps this in step with
        // `populate_safe_accepted_batches`, which seeds `expected` from the same
        // anchor. Without it, a freshly recovered deployment — whose frontier
        // stays empty until `run`'s first sync lands an accepted row — would
        // have its submitter fold from 0 and re-submit its first post-recovery
        // batch (nonce N') every tick until then. The cascade reader
        // (`first_non_gold_closed_batch`) is unaffected: no valid batch sits
        // below the anchor (I16), so `nonce >= 0` and `nonce >= N'` select the
        // same row. (See I16 / docs/recovery/cockroach.md.)
        None => batch_tree_anchor_in(conn),
    }
}

/// Simulate the scheduler's acceptance logic over new safe inputs and append
/// matches to `safe_accepted_batches`.
///
/// Paginates through `safe_inputs` rows newer than the latest accepted row,
/// pre-filtered at SQL to `batch_submitter` as the sender. For each row,
/// delegates to [`ProtocolTiming::scheduler_accepts`] with the
/// currently-expected nonce — on `Some`, inserts the accepted row and advances
/// expected; on `None`, moves on. The SQL sender filter is an optimization;
/// `scheduler_accepts` re-checks defensively, so the filter is
/// correctness-neutral.
///
/// `batch_submitter` and `timing` arrive separately because they're
/// orthogonal: the address is identity (who the scheduler accepts from);
/// the timing is the scheduler's staleness rules.
///
/// The scan cursor is local to one invocation. Persistently, the only cursor
/// is the latest accepted row in `safe_accepted_batches`, not the latest row
/// scanned. That is intentional for now: a recovery batch may reuse the same
/// scheduler nonce after earlier rejected rows, and a too-eager persistent
/// scan cursor would risk skipping it. The tradeoff is that rejected
/// batch-submitter inputs after the gold frontier can be rescanned on later
/// safe-head syncs until a later batch is accepted and moves the accepted
/// cursor forward.
///
/// Scheduler-mirror co-location: the acceptance *predicate* lives at the
/// library seam ([`ProtocolTiming::scheduler_accepts`]), and the bare
/// nonce-advance fold in `protocol::advance_expected_batch_nonce`.
/// This loop deliberately keeps its own inline `expected` advance rather than
/// reusing that fold: the advance is interleaved with two storage-only side
/// effects that cannot move below the protocol layer — the R2 content-identity
/// check ([`content_identity_violation`]) and the `canonical_divergence` freeze.
/// Sharing a fold here would force a callback contract for those (a refactor,
/// not the no-behavior-change library move).
pub(super) fn populate_safe_accepted_batches(
    conn: &Connection,
    batch_submitter: Address,
    timing: &ProtocolTiming,
) -> Result<()> {
    const PAGE_SIZE: i64 = 256;
    const SELECT_SQL: &str = "SELECT safe_input_index, payload, block_number \
                              FROM safe_inputs \
                              WHERE sender = ?1 AND safe_input_index > ?2 \
                              ORDER BY safe_input_index ASC LIMIT ?3";
    const INSERT_SQL: &str = "INSERT OR IGNORE INTO safe_accepted_batches \
                              (safe_input_index, nonce, first_frame_safe_block, inclusion_block) \
                              VALUES (?1, ?2, ?3, ?4)";

    // A persisted divergence marker freezes the acceptance frontier: the
    // local batch tree is no longer a reliable mirror of canonical state,
    // and advancing it (or promoting on it) would compound the divergence.
    // `check_danger` reports `CanonicalDivergence` ahead of every other arm,
    // so the detector exits / startup refuses; the remedy is cockroach
    // recovery, never standard recovery (review R2).
    if canonical_divergence_in(conn)?.is_some() {
        return Ok(());
    }

    // The frontier begins at the batch-tree anchor: 0 for a genesis
    // deployment (unchanged), or N' for a cockroach-recovered one. Below the
    // anchor the local tree has no batches — that history is folded into the
    // recovered checkpoint `S'`, not kept as tree batches — so those L1
    // landings are *trusted collapsed history*, not foreign. Seeding `expected`
    // at the anchor makes the scan skip them by nonce-mismatch (they never
    // reach the content-identity check), while landings at/above the anchor —
    // the resumed instance's own batches — are accepted and checked normally.
    // (See I16 / docs/recovery: the anchor is where the local tree's authority
    // begins.)
    let anchor = batch_tree_anchor_in(conn)?;
    let latest_accepted = query_latest_safe_accepted_batch(conn)?;
    let mut cursor = latest_accepted
        .map(|row| row.safe_input_index)
        .unwrap_or(-1);
    let mut expected = latest_accepted
        .map(|row| i64_to_u64(row.nonce).saturating_add(1))
        .unwrap_or(anchor);

    loop {
        // Materialize one page before executing any INSERTs. rusqlite's row
        // iterator borrows the prepared statement, so we can't INSERT on the
        // same connection while iterating. Once the page is collected and the
        // statement is dropped, the connection is free for inserts.
        let page: Vec<(i64, Vec<u8>, i64)> = {
            let mut stmt = conn.prepare_cached(SELECT_SQL)?;
            stmt.query_map(
                params![batch_submitter.as_slice(), cursor, PAGE_SIZE,],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
            .collect::<Result<_>>()?
        };

        if page.is_empty() {
            break;
        }
        let page_len = page.len() as i64;

        let mut insert_stmt = conn.prepare_cached(INSERT_SQL)?;
        for (safe_input_index, payload, block_number) in &page {
            cursor = *safe_input_index;
            let input = SafeInputView {
                safe_input_index: i64_to_u64(*safe_input_index),
                sender: batch_submitter,
                payload: payload.as_slice(),
                inclusion_block: i64_to_u64(*block_number),
            };
            let Some(accepted) = timing.scheduler_accepts(batch_submitter, input, expected) else {
                continue;
            };

            // Content-identity check (review R2), gated on full acceptance —
            // exactly here, where the simulated scheduler accepted the
            // landing. Rejected/stale/undecodable copies are scheduler
            // no-ops; their content is irrelevant by construction.
            if let Some(kind) = content_identity_violation(conn, &accepted, payload.as_slice())? {
                record_canonical_divergence_in(conn, &accepted, kind)?;
                tracing::error!(
                    nonce = accepted.nonce,
                    safe_input_index = accepted.safe_input_index,
                    kind = kind.as_str(),
                    "CANONICAL DIVERGENCE: accepted L1 landing does not match \
                     our batch at this nonce; freezing the acceptance frontier \
                     — remedy is cockroach recovery (wipe + rebuild from L1)"
                );
                // Stop scanning; the marker (committed with this sync)
                // freezes the frontier and routes every subsequent boot and
                // detector tick to refusal.
                return Ok(());
            }

            insert_stmt.execute(params![
                u64_to_i64(accepted.safe_input_index),
                u64_to_i64(accepted.nonce),
                u64_to_i64(accepted.first_frame_safe_block),
                u64_to_i64(accepted.inclusion_block),
            ])?;
            expected = expected.saturating_add(1);
        }

        if page_len < PAGE_SIZE {
            break;
        }
    }

    Ok(())
}

/// How an accepted landing failed the content-identity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DivergenceKind {
    /// No valid closed local batch exists at the accepted nonce — we never
    /// (validly) submitted one, so the landing is a zombie or foreign batch
    /// from our key.
    Foreign,
    /// A valid closed local batch exists, but the landed wire bytes hash
    /// differently from the bytes we sealed.
    Mismatch,
}

impl DivergenceKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Foreign => "foreign",
            Self::Mismatch => "mismatch",
        }
    }
}

/// R2 check proper: compare a fully-accepted landing against our valid
/// closed batch at the same nonce. `None` = identical (the normal case).
///
/// Why content (not identity) suffices: accepted content-equal copies are
/// effect-equal — which physical tx landed carries no semantic weight. The
/// hash on our side was stamped at seal by the same encode path the
/// submitter broadcasts, so the comparison survives wire-format upgrades.
fn content_identity_violation(
    conn: &Connection,
    accepted: &AcceptedBatch,
    landed_payload: &[u8],
) -> Result<Option<DivergenceKind>> {
    let ours: Option<Option<Vec<u8>>> = conn
        .query_row(
            "SELECT payload_hash FROM valid_closed_batches WHERE nonce = ?1",
            params![u64_to_i64(accepted.nonce)],
            |row| row.get(0),
        )
        .optional()?;
    match ours {
        None => Ok(Some(DivergenceKind::Foreign)),
        Some(None) => {
            // Sealed batches always carry a hash (stamped in the seal
            // UPDATE); a NULL here is an impossible state — fail loud.
            // (Recovery sentinels carry no hash, but they sit *behind* the
            // acceptance frontier by construction and are never compared.)
            // Surface it as a structured error (rolls back the reader's sync
            // transaction and travels its normal error path) rather than a
            // panic that would crash the reader task mid-transaction.
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
                Some(format!(
                    "valid closed batch at nonce {} has no payload_hash",
                    accepted.nonce
                )),
            ))
        }
        Some(Some(hash)) => {
            let landed = alloy_primitives::keccak256(landed_payload);
            if hash.as_slice() == landed.as_slice() {
                Ok(None)
            } else {
                Ok(Some(DivergenceKind::Mismatch))
            }
        }
    }
}

/// The persisted divergence marker, if any: `(nonce, safe_input_index)`.
pub(super) fn canonical_divergence_in(conn: &Connection) -> Result<Option<(u64, u64)>> {
    conn.query_row(
        "SELECT nonce, safe_input_index FROM canonical_divergence WHERE singleton_id = 0",
        [],
        |row| {
            Ok((
                i64_to_u64(row.get::<_, i64>(0)?),
                i64_to_u64(row.get::<_, i64>(1)?),
            ))
        },
    )
    .optional()
}

/// Persist the poison marker, atomically with the sync that detected it
/// (same transaction). Keep-first by construction: the frontier freezes at
/// the first detection, so this can only ever run once per marker lifetime
/// (the guard at the top of `populate_safe_accepted_batches`); a second
/// INSERT would fail loud on the singleton PK.
fn record_canonical_divergence_in(
    conn: &Connection,
    accepted: &AcceptedBatch,
    kind: DivergenceKind,
) -> Result<()> {
    conn.execute(
        "INSERT INTO canonical_divergence \
         (singleton_id, nonce, safe_input_index, kind, detected_at_ms) \
         VALUES (0, ?1, ?2, ?3, ?4)",
        params![
            u64_to_i64(accepted.nonce),
            u64_to_i64(accepted.safe_input_index),
            kind.as_str(),
            now_unix_ms(),
        ],
    )?;
    Ok(())
}
