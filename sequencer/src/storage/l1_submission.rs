// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The submitter's storage half: frontier lookup, per-batch frames + user
//! ops, the catch-up / per-batch replay reader, the SSZ-encoded pending-batch
//! list the submitter pulls each tick — and the one submission-side write,
//! the wallet-nonce watermark (raised before every broadcast, review R1a).
//!
//! Structural nonces are assigned by the `batches.nonce` trigger at close
//! time (see `ingress`), and `safe_accepted_batches` is maintained by
//! `append_safe_inputs` (see `l1_inputs`). The reads here are shared between
//! the batch submitter (hot-path tick) and the egress replay path (catch-up
//! reader); they live together because they all aggregate at the batch level.

use rusqlite::{Result, params};

use super::Storage;
use super::convert::{i64_to_u16, i64_to_u32, i64_to_u64, u64_to_i64};
use super::mutations::{batch_tree_anchor_in, set_batch_tree_anchor_in};
use super::queries::{current_safe_block_required, decode_l2_tx_row};
use super::safe_accepted_batches::frontier_nonce;
use super::{FrameHeader, PendingBatch, SubmitterFrontier};
use sequencer_core::batch::{Batch, Frame as BatchFrame, WireUserOp};
use sequencer_core::l2_tx::SequencedL2Tx;

impl Storage {
    /// Read-only frontier view used by the submitter each tick to derive the
    /// next batch nonce. `accepted_next_nonce` is the next nonce the scheduler
    /// is expected to accept, derived from `safe_accepted_batches`.
    ///
    /// The scheduler-accepted frontier is maintained by
    /// [`Storage::append_safe_inputs`], so this is a pure read.
    ///
    /// **Precondition:** at least one safe-head observation must have been
    /// recorded (via [`Storage::append_safe_inputs`]). In production this is
    /// always true because `run_preemptive_recovery` either syncs L1 first
    /// or refuses to boot via `L1ViewStale`. Tests must seed an observation
    /// explicitly; calling against a fresh DB returns `QueryReturnedNoRows`.
    pub fn submitter_frontier(&mut self) -> Result<SubmitterFrontier> {
        self.read(|tx| {
            Ok(SubmitterFrontier {
                safe_block: current_safe_block_required(tx)?,
                accepted_next_nonce: frontier_nonce(tx)?,
            })
        })
    }

    /// Set the batch-tree anchor nonce (the nonce the parentless root carries).
    /// Used by `setup --recovery` to root the rebuilt tree at `N'`. Aborts if
    /// `setup_complete` already exists (`trg_batch_tree_anchor_write_once`).
    pub fn set_batch_tree_anchor(&mut self, nonce: u64) -> Result<()> {
        self.write(|tx| set_batch_tree_anchor_in(tx, nonce))
    }

    /// Read the batch-tree anchor nonce (default 0).
    pub fn batch_tree_anchor(&mut self) -> Result<u64> {
        self.read(|tx| batch_tree_anchor_in(tx))
    }

    /// The highest wallet nonce ever broadcast by this deployment's
    /// batch-submitter key, or `None` if nothing was ever broadcast
    /// (review R1a — the durable realization of the TLA+ `walletNonce`).
    /// The flush reads this as its coverage floor; it never resets.
    pub fn wallet_nonce_watermark(&mut self) -> Result<Option<u64>> {
        use rusqlite::OptionalExtension;
        self.read(|tx| {
            tx.query_row(
                "SELECT watermark FROM wallet_nonce_watermark WHERE singleton_id = 0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|v| v.map(i64_to_u64))
        })
    }

    /// Write-before-broadcast (review R1a): durably raise the watermark to
    /// cover `nonce` *before* any tx at a nonce `<= nonce` is sent. The
    /// commit is power-loss durable (`synchronous=FULL`); a crash between
    /// commit and send only over-covers — the flush later no-ops a
    /// never-used slot, which is harmless. Monotonic: never lowers.
    pub fn raise_wallet_nonce_watermark(&mut self, nonce: u64) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO wallet_nonce_watermark (singleton_id, watermark) \
                 VALUES (0, ?1) \
                 ON CONFLICT(singleton_id) \
                 DO UPDATE SET watermark = MAX(watermark, excluded.watermark)",
                params![u64_to_i64(nonce)],
            )?;
            Ok(())
        })
    }

    /// Highest valid (non-invalidated) `batch_index`, or `None` if no valid
    /// batches exist. The open batch is included.
    pub fn latest_batch_index(&mut self) -> Result<Option<u64>> {
        let value: Option<i64> =
            self.conn
                .query_row("SELECT MAX(batch_index) FROM valid_batches", [], |row| {
                    row.get(0)
                })?;
        Ok(value.map(i64_to_u64))
    }

    /// The structural nonce of the current open Tip (the single
    /// `valid_open_batch`), or `None` if no Tip is open. Used by `setup
    /// --recovery` to detect a partially-recovered DB whose root tip carries a
    /// different resume nonce than the current attempt (the at-most-one-valid-
    /// open-tip index makes this at most one row).
    pub fn open_tip_nonce(&mut self) -> Result<Option<u64>> {
        use rusqlite::OptionalExtension;
        let value: Option<i64> = self
            .conn
            .query_row("SELECT nonce FROM valid_open_batch", [], |row| row.get(0))
            .optional()?;
        Ok(value.map(i64_to_u64))
    }

    /// Frame headers for `batch_index` in `frame_in_batch` order. Reads the
    /// raw `frames` table — does NOT filter on validity, since callers only
    /// reach this method after they already know the batch is valid.
    pub fn frames_for_batch(&mut self, batch_index: u64) -> Result<Vec<FrameHeader>> {
        frames_for_batch_in(&self.conn, batch_index)
    }

    /// Materialize all sequenced L2 txs in one batch (used by the catch-up /
    /// per-batch replay paths). Returns `[]` for invalidated batches.
    pub fn ordered_l2_txs_for_batch(&mut self, batch_index: u64) -> Result<Vec<SequencedL2Tx>> {
        const SQL: &str = "
            SELECT
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN 0 ELSE 1 END AS kind,
                CASE
                    WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.sender
                    WHEN s.safe_input_index IS NOT NULL THEN d.sender
                    ELSE NULL
                END AS sender,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.data ELSE NULL END AS data,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN f.fee  ELSE NULL END AS fee,
                CASE WHEN s.safe_input_index   IS NOT NULL THEN d.payload      ELSE NULL END AS payload,
                CASE WHEN s.safe_input_index   IS NOT NULL THEN d.block_number ELSE NULL END AS block_number
            FROM valid_sequenced_l2_txs s
            LEFT JOIN user_ops u
              ON u.batch_index    = s.batch_index
             AND u.frame_in_batch = s.frame_in_batch
             AND u.pos_in_frame   = s.user_op_pos_in_frame
            LEFT JOIN frames f
              ON f.batch_index    = s.batch_index
             AND f.frame_in_batch = s.frame_in_batch
            LEFT JOIN safe_inputs d
              ON d.safe_input_index = s.safe_input_index
            WHERE s.batch_index = ?1
            ORDER BY s.offset ASC
        ";
        let mut stmt = self.conn.prepare_cached(SQL)?;
        let rows = stmt.query_map(params![u64_to_i64(batch_index)], |row| {
            Ok(decode_l2_tx_row(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>>>()
    }

    /// Load all valid closed batches with nonce >= `min_nonce`, in nonce order,
    /// each one fully assembled and SSZ-encoded with its authoritative nonce.
    ///
    /// Authoritative because the nonce stamped into the wire payload is the
    /// one the DB persists on the batch row (via the `parent.nonce + 1`
    /// structural invariant). The caller never sees an unstamped batch —
    /// there is no way to accidentally encode with the wrong nonce.
    pub fn pending_batches(&mut self, min_nonce: u64) -> Result<Vec<PendingBatch>> {
        const SQL: &str = "SELECT batch_index, nonce FROM valid_closed_batches \
                           WHERE nonce >= ?1 ORDER BY nonce ASC";
        let pending_refs: Vec<(u64, u64)> = {
            let mut stmt = self.conn.prepare_cached(SQL)?;
            let rows = stmt.query_map(params![u64_to_i64(min_nonce)], |row| {
                let bi: i64 = row.get(0)?;
                let nonce: i64 = row.get(1)?;
                Ok((i64_to_u64(bi), i64_to_u64(nonce)))
            })?;
            rows.collect::<Result<Vec<_>>>()?
        };

        let mut batches = Vec::with_capacity(pending_refs.len());
        for (batch_index, nonce) in pending_refs {
            let frames = self.load_batch_frames(batch_index)?;
            let batch = Batch { nonce, frames };
            let encoded = ssz::Encode::as_ssz_bytes(&batch);
            batches.push(PendingBatch {
                batch_index,
                nonce,
                encoded,
            });
        }
        Ok(batches)
    }

    /// Load every frame (header + user ops) of `batch_index` in frame order.
    /// Internal helper for [`Self::pending_batches`]; does NOT filter on
    /// validity — callers only reach this after they know the batch is valid.
    fn load_batch_frames(&mut self, batch_index: u64) -> Result<Vec<BatchFrame>> {
        load_batch_frames_in(&self.conn, batch_index)
    }
}

fn frames_for_batch_in(conn: &rusqlite::Connection, batch_index: u64) -> Result<Vec<FrameHeader>> {
    let mut stmt = conn.prepare_cached(
        "SELECT frame_in_batch, fee, safe_block FROM frames \
         WHERE batch_index = ?1 ORDER BY frame_in_batch ASC",
    )?;
    let rows = stmt.query_map(params![u64_to_i64(batch_index)], |row| {
        Ok(FrameHeader {
            frame_in_batch: i64_to_u32(row.get(0)?),
            fee: i64_to_u16(row.get(1)?),
            safe_block: i64_to_u64(row.get(2)?),
        })
    })?;
    rows.collect::<Result<Vec<_>>>()
}

/// Free-function form so the seal path can encode the closing batch inside
/// its own transaction — the content-identity check's hash-at-seal must come
/// from **the same encode path the submitter uses** (review R2); this
/// function being that single path is load-bearing.
pub(super) fn load_batch_frames_in(
    conn: &rusqlite::Connection,
    batch_index: u64,
) -> Result<Vec<BatchFrame>> {
    let frame_headers = frames_for_batch_in(conn, batch_index)?;
    let mut frames = Vec::with_capacity(frame_headers.len());
    for header in frame_headers {
        let mut stmt = conn.prepare_cached(
            "SELECT nonce, max_fee, data, sig FROM user_ops \
             WHERE batch_index = ?1 AND frame_in_batch = ?2 \
             ORDER BY pos_in_frame ASC",
        )?;
        let rows = stmt.query_map(
            params![u64_to_i64(batch_index), i64::from(header.frame_in_batch)],
            |row| {
                Ok(WireUserOp {
                    nonce: i64_to_u32(row.get(0)?),
                    max_fee: i64_to_u16(row.get(1)?),
                    data: row.get(2)?,
                    signature: row.get(3)?,
                })
            },
        )?;
        let user_ops: Vec<WireUserOp> = rows.collect::<Result<_>>()?;
        frames.push(BatchFrame {
            user_ops,
            safe_block: header.safe_block,
            fee_price: header.fee,
        });
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{
        SENDER_A, SENDER_B, local_batch_payload, seed_closed_batches,
        seed_safe_inputs_with_batch_nonces, temp_db,
    };
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput};
    use alloy_primitives::Address;
    use sequencer_core::batch::Batch;
    use sequencer_core::protocol::ProtocolTiming;

    #[test]
    fn pending_batches_stamps_authoritative_nonce_into_wire_bytes() {
        // The landmine we removed: an earlier `batch_for_submission` returned a
        // `Batch { nonce: 0, … }` placeholder, and callers had to remember to
        // stamp the real nonce via `encode_for_scheduler_with_nonce`. The new
        // `pending_batches` reads the DB-authoritative nonce from
        // `valid_closed_batches` and bakes it straight into the SSZ bytes — so
        // decoding the payload must round-trip back to that nonce, and the
        // frame body must match what storage persisted.
        let db = temp_db("pending-batches-nonce-baked-in");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        let mut head = storage
            .initialize_open_state(12, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        // Close batch 0 so it becomes eligible for submission.
        storage
            .close_frame_and_batch(&mut head, 12)
            .expect("close batch 0");

        let pending = storage.pending_batches(0).expect("load pending batches");
        assert_eq!(pending.len(), 1);
        let entry = &pending[0];
        assert_eq!(entry.batch_index, 0);
        assert_eq!(entry.nonce, 0, "genesis batch has nonce 0");

        // The wire bytes must decode back to the authoritative nonce AND the
        // frame body storage persisted.
        let decoded: Batch =
            ssz::Decode::from_ssz_bytes(&entry.encoded).expect("decode pending wire bytes");
        assert_eq!(decoded.nonce, entry.nonce);
        assert_eq!(decoded.frames.len(), 1);
        let frame = &decoded.frames[0];
        assert!(frame.user_ops.is_empty());
        assert_eq!(frame.safe_block, 12);
        // Default log_recommended_fee = 0+296+20+419+621 = 1356.
        assert_eq!(frame.fee_price, 1356);
    }

    #[test]
    fn batch_level_helpers_expose_latest_index_frames_and_txs() {
        let db = temp_db("batch-level-helpers");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        // Before initialization there should be no batches.
        assert!(
            storage
                .latest_batch_index()
                .expect("query latest batch nonce on empty db")
                .is_none()
        );

        // Initialize first batch/frame and append some data.
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        // Close current batch and move to next so batch 0 becomes closed.
        let next_safe_block = head.safe_block;
        storage
            .close_frame_and_batch(&mut head, next_safe_block)
            .expect("close batch and rotate");

        // Latest batch nonce should now be 1 (open), with batch 0 closed.
        let latest = storage
            .latest_batch_index()
            .expect("query latest batch nonce")
            .expect("latest batch should exist");
        assert_eq!(latest, 1);

        // Batch 0 should still have at least one frame header.
        let frames = storage
            .frames_for_batch(0)
            .expect("load frames for batch 0");
        assert!(!frames.is_empty());

        // Ordered L2 txs for batch 0 should be queryable (even if empty).
        let txs = storage
            .ordered_l2_txs_for_batch(0)
            .expect("load l2 txs for batch 0");
        assert!(
            txs.is_empty(),
            "fresh batch should not have sequenced txs yet"
        );
    }

    #[test]
    fn closed_batch_becomes_eligible_for_submission_with_assigned_nonce() {
        // : closing a batch transitions it from "open Tip" to "eligible
        // for L1 submission" — it appears in `valid_closed_batches` with a
        // nonce derived from its parent pointer. Pins the submitter's
        // contract: open batches are NOT pulled into the submission pipeline,
        // and closed batches ARE, at the schema-guaranteed nonce.
        let db = temp_db("closed-batch-eligible");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        // Before close: the open batch must not appear in pending-batches.
        let pending_before = storage
            .pending_batches(0)
            .expect("load pending batches (pre-close)");
        assert!(
            pending_before.is_empty(),
            "open batch must not be eligible for submission: {pending_before:?}",
        );

        // Close batch 0 — this rotates the Tip to batch 1 and seals batch 0.
        let safe_block = head.safe_block;
        storage
            .close_frame_and_batch(&mut head, safe_block)
            .expect("close batch 0");

        // After close: batch 0 is eligible with nonce 0 (genesis, parent
        // NULL → trigger assigns nonce 0).
        let pending_after = storage
            .pending_batches(0)
            .expect("load pending batches (post-close)");
        assert_eq!(
            pending_after.len(),
            1,
            "exactly one batch should be eligible after the first close",
        );
        assert_eq!(pending_after[0].batch_index, 0);
        assert_eq!(
            pending_after[0].nonce, 0,
            "closed batch 0 must carry nonce 0 (genesis, no parent)",
        );
        // The new open Tip (batch 1) must NOT be eligible even though it
        // exists — eligibility requires sealed_at_ms NOT NULL.
        assert!(
            pending_after.iter().all(|b| b.batch_index != 1),
            "open batch 1 (the new Tip) must not be eligible: {pending_after:?}",
        );
    }

    #[test]
    fn wallet_nonce_watermark_is_monotonic_and_absent_at_genesis() {
        let db = temp_db("wallet-nonce-watermark");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        assert_eq!(
            storage.wallet_nonce_watermark().expect("read"),
            None,
            "genesis: nothing ever broadcast"
        );

        storage.raise_wallet_nonce_watermark(5).expect("raise to 5");
        assert_eq!(storage.wallet_nonce_watermark().expect("read"), Some(5));

        // Monotonic: a lower raise never lowers the watermark.
        storage.raise_wallet_nonce_watermark(3).expect("raise to 3");
        assert_eq!(storage.wallet_nonce_watermark().expect("read"), Some(5));

        storage.raise_wallet_nonce_watermark(9).expect("raise to 9");
        assert_eq!(storage.wallet_nonce_watermark().expect("read"), Some(9));
    }

    #[test]
    fn submitter_frontier_returns_zero_when_no_batches_were_accepted() {
        let db = temp_db("submitter-frontier-empty");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .append_safe_inputs(0, &[], SENDER_A, &default_test_protocol())
            .expect("record observed safe head");
        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(frontier.safe_block, 0);
        assert_eq!(frontier.accepted_next_nonce, 0);
    }

    #[test]
    fn submitter_frontier_uses_anchor_when_no_batches_accepted() {
        // A cockroach-recovered deployment boots with an empty
        // safe_accepted_batches and a non-zero batch-tree anchor (N'). The
        // submitter's start frontier must be N', not 0 — otherwise it would
        // re-submit its first post-recovery batch (nonce N') on every tick
        // until the first accepted row lands. Regression test for the
        // frontier_nonce/anchor asymmetry (frontier_nonce defaulted to 0 while
        // populate_safe_accepted_batches seeds `expected` from the anchor).
        let db = temp_db("submitter-frontier-anchor");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage.set_batch_tree_anchor(7).expect("anchor at N'=7");
        storage
            .append_safe_inputs(0, &[], SENDER_A, &default_test_protocol())
            .expect("record observed safe head");
        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(frontier.accepted_next_nonce, 7);
    }

    #[test]
    fn submitter_frontier_tracks_accepted_prefix() {
        let db = temp_db("submitter-frontier-prefix");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        // Local closed batches 0 and 1 — their landings carry the real wire
        // bytes (content-identity check); 3..5 stay synthetic, which is fine
        // because the nonce gap means the fold never accepts them.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close 0");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close 1");
        // seed_safe_inputs_with_batch_nonces already calls append_safe_inputs,
        // which auto-populates safe_accepted_batches.
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 10, &[0, 1, 3, 4, 5]);

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(frontier.safe_block, 10);
        assert_eq!(frontier.accepted_next_nonce, 2);
    }

    fn default_test_protocol() -> ProtocolTiming {
        ProtocolTiming {
            max_wait_blocks: 1200,
            preemptive_margin_blocks: 75,
            l1_read_stale_after_blocks: 900,
            seconds_per_block: 12,
        }
    }

    fn unix_now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    #[test]
    fn check_danger_reports_observed_closed_batch_danger() {
        let db = temp_db("check-danger-observed-closed");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 1");

        let protocol = default_test_protocol();
        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                1135,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 20,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append accepted batch 0");

        let status = storage
            .check_danger(&protocol, unix_now_ms())
            .expect("check_danger");
        assert_eq!(status, crate::storage::DangerStatus::ClosedBatchInDanger(1));
    }

    #[test]
    fn check_danger_reports_estimated_batch_danger_on_wall_clock_drift() {
        // Observed block-based checks wouldn't fire (batch 1 has
        // first_frame_safe_block = 100 and safe_block = 1200, age = 1100 <
        // 1125). But wall-clock says the safe head hasn't advanced in ~25
        // blocks: the effective threshold drops to 1100, so batch 1 reaches
        // danger via the wall-clock correction.
        let db = temp_db("check-danger-estimated");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 1");

        let protocol = default_test_protocol();
        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 200,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append accepted batch 0");

        // Pretend safe-progress was recorded 25 blocks' worth of wall-clock ago.
        let now_ms = unix_now_ms();
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms.saturating_sub(25 * 12 * 1000)).unwrap_or(i64::MAX)],
            )
            .expect("rewind safe-progress timestamp");

        let status = storage
            .check_danger(&protocol, now_ms)
            .expect("check_danger");
        assert_eq!(
            status,
            crate::storage::DangerStatus::EstimatedBatchInDanger(1)
        );
    }

    #[test]
    fn check_danger_prefers_observed_closed_over_estimated_when_both_fire() {
        // If the observed safe head already puts the closed frontier past
        // `danger_threshold`, route to observed closed-batch recovery. The
        // wall-clock arm is a fallback, not a mask over observed danger.
        let db = temp_db("check-danger-observed-closed-wins");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        let protocol = default_test_protocol();
        // Advance safe head so batch 0 is past `danger_threshold` (1125):
        // current=1200 - first_frame=10 = 1190 >= 1125.
        // No safe_input ingested for batch 0, so it stays non-gold.
        storage
            .append_safe_inputs(1200, &[], SENDER_A, &protocol)
            .expect("advance safe head past observed danger threshold");

        // Pretend safe-progress was recorded 25 blocks' worth of wall-clock
        // ago. With the wall-clock correction, the adjusted threshold is
        // 1100 — batch 0's age (1190) also clears that, so wall-clock fires.
        let now_ms = unix_now_ms();
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms.saturating_sub(25 * 12 * 1000)).unwrap_or(i64::MAX)],
            )
            .expect("rewind safe-progress timestamp");

        let status = storage
            .check_danger(&protocol, now_ms)
            .expect("check_danger");
        assert_eq!(
            status,
            crate::storage::DangerStatus::ClosedBatchInDanger(0),
            "observed closed-batch danger must win once safe-state has crossed danger",
        );
    }

    #[test]
    fn check_danger_prefers_observed_tip_over_estimated_when_both_fire() {
        // Same fallback rule for the open Tip: if observed safe-state already
        // puts the Tip in danger, recover the Tip directly instead of refusing
        // under the wall-clock classification.
        let db = temp_db("check-danger-observed-tip-wins");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0; batch 1 is Tip");

        let protocol = default_test_protocol();
        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 20,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append accepted batch 0");

        // The Tip's observed age is 1190 >= 1125, and the wall-clock-adjusted
        // arm would also fire. Observed Tip danger must win.
        let now_ms = unix_now_ms();
        storage
            .conn
            .execute(
                "UPDATE l1_safe_head SET synced_at_ms = ?1 WHERE singleton_id = 0",
                [i64::try_from(now_ms.saturating_sub(25 * 12 * 1000)).unwrap_or(i64::MAX)],
            )
            .expect("rewind safe-progress timestamp");

        let status = storage
            .check_danger(&protocol, now_ms)
            .expect("check_danger");
        assert_eq!(
            status,
            crate::storage::DangerStatus::TipInDanger(1),
            "Tip must win when observed safe-state has already crossed danger",
        );
    }

    #[test]
    fn check_danger_refuses_when_l1_view_is_stale() {
        let db = temp_db("check-danger-l1-view-stale");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        let protocol = default_test_protocol();
        let old_safe_timestamp = 1_000_u64;
        storage
            .append_safe_inputs_with_timestamp(
                1200,
                old_safe_timestamp,
                &[],
                SENDER_A,
                &protocol,
                crate::storage::FrontierMode::Populate,
            )
            .expect("advance safe head with stale L1 timestamp");

        let now_ms =
            (old_safe_timestamp + protocol.l1_read_stale_after_secs()).saturating_mul(1000);
        let status = storage
            .check_danger(&protocol, now_ms)
            .expect("check_danger");
        assert_eq!(
            status,
            crate::storage::DangerStatus::L1ViewStale,
            "global L1 view staleness must refuse before observed batch recovery",
        );
    }

    #[test]
    fn check_danger_safe_when_never_synced() {
        // Fresh DB, no prior safe block timestamp. The L1 view is unusable
        // until the input reader records a real safe-head observation.
        let db = temp_db("check-danger-never-synced");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let status = storage
            .check_danger(&default_test_protocol(), unix_now_ms())
            .expect("check_danger");
        assert_eq!(status, crate::storage::DangerStatus::L1ViewStale);
    }

    #[test]
    fn populate_safe_accepted_batches_resumes_from_latest_row() {
        let db = temp_db("safe-accepted-frontier-resume");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_test_protocol();

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        for nonce in 0..4 {
            storage
                .close_frame_and_batch(&mut head, 10)
                .unwrap_or_else(|e| panic!("close {nonce}: {e}"));
        }
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 10, &[0, 1]);

        // Mixed-sender wave: the SENDER_B row must be ignored, SENDER_A rows
        // must resume from the cursor and advance the frontier.
        let second_wave = vec![
            StoredSafeInput {
                sender: SENDER_B,
                payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                    nonce: 99,
                    frames: Vec::new(),
                }),
                block_number: 11,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: local_batch_payload(&mut storage, 2),
                block_number: 11,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: local_batch_payload(&mut storage, 3),
                block_number: 11,
            },
        ];
        storage
            .append_safe_inputs(11, second_wave.as_slice(), SENDER_A, &protocol)
            .expect("append second wave");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(frontier.safe_block, 11);
        assert_eq!(frontier.accepted_next_nonce, 4);

        let accepted_count: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM safe_accepted_batches", [], |row| {
                row.get(0)
            })
            .expect("count accepted rows");
        assert_eq!(accepted_count, 4);
    }

    #[test]
    fn safe_accepted_frontier_skips_stale_payloads() {
        let db = temp_db("safe-accepted-frontier-skip-stale");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_test_protocol();

        // Local batch 0 (first frame safe_block=100) and batch 1 (first frame
        // safe_block=1900). Their landings carry the real wire bytes so the
        // content-identity check accepts them.
        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 1900)
            .expect("close 0");
        storage
            .close_frame_and_batch(&mut head, 1900)
            .expect("close 1");

        // Non-stale landing of batch 0 (block 200 - safe_block 100 < 1200).
        let non_stale_payload = local_batch_payload(&mut storage, 0);
        // Stale copy at nonce 1 (safe_block=100, block 2000 → age >= 1200):
        // a scheduler no-op, so its content is never compared — synthetic
        // bytes are deliberate here.
        let stale_payload = super::super::test_helpers::make_stale_batch_payload(1, 100);
        // Non-stale landing of batch 1 (block 2000 - safe_block 1900 < 1200).
        let non_stale_payload_2 = local_batch_payload(&mut storage, 1);

        let inputs = vec![
            StoredSafeInput {
                sender: SENDER_A,
                payload: non_stale_payload,
                block_number: 200,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: stale_payload,
                block_number: 2000,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: non_stale_payload_2,
                block_number: 2000,
            },
        ];
        storage
            .append_safe_inputs(2000, inputs.as_slice(), SENDER_A, &protocol)
            .expect("append");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(frontier.accepted_next_nonce, 2);
    }

    #[test]
    fn seal_stamps_payload_hash_of_the_submitter_encode_path() {
        // Hash-at-seal (review R2): the hash stamped on the sealed row must
        // be the keccak256 of exactly the bytes the submitter will broadcast
        // (`pending_batches`'s encoding) — same code path, by construction.
        let db = temp_db("seal-stamps-payload-hash");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        let stamped: Vec<u8> = storage
            .conn
            .query_row(
                "SELECT payload_hash FROM batches WHERE batch_index = 0",
                [],
                |row| row.get(0),
            )
            .expect("sealed batch must carry a payload hash");
        let wire = local_batch_payload(&mut storage, 0);
        assert_eq!(
            stamped.as_slice(),
            alloy_primitives::keccak256(&wire).as_slice(),
            "seal-time hash must match the submitter's wire bytes"
        );
    }

    #[test]
    fn accepted_landing_with_mismatched_content_freezes_frontier() {
        // The F1-zombie / F3-re-seal shape: a landing at the expected nonce
        // whose bytes differ from the batch we sealed. The check records a
        // 'mismatch' marker atomically with the sync and freezes the
        // frontier; later syncs stay frozen.
        let db = temp_db("mismatch-divergence");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_test_protocol();
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        // Same nonce + same first-frame safe block (fresh by inclusion),
        // different content (synthetic fee 0 vs the sealed fee).
        let imposter = super::super::test_helpers::make_stale_batch_payload(0, 10);
        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: imposter,
                    block_number: 20,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("sync commits; the marker rides the same transaction");

        let kind: String = storage
            .conn
            .query_row(
                "SELECT kind FROM canonical_divergence WHERE singleton_id = 0",
                [],
                |row| row.get(0),
            )
            .expect("marker persisted");
        assert_eq!(kind, "mismatch");
        let frontier = storage.submitter_frontier().expect("frontier");
        assert_eq!(frontier.accepted_next_nonce, 0, "frontier frozen");

        // A later sync — even one carrying the *correct* bytes — must not
        // thaw the frontier: the marker is terminal until cockroach recovery.
        let genuine = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                21,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: genuine,
                    block_number: 21,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append after marker");
        let frontier = storage.submitter_frontier().expect("frontier");
        assert_eq!(frontier.accepted_next_nonce, 0, "frontier stays frozen");
    }

    #[test]
    fn sim_accepted_foreign_batch_freezes_frontier_with_divergence_marker() {
        // `scheduler_accepts` deliberately omits the two structural
        // rejections (future safe_block, non-monotonic frames) under
        // self-trust — the sequencer never produces them. Before the
        // content-identity check (review R2), such a foreign batch at the
        // expected nonce would be sim-accepted and silently desync the
        // frontier forever. With the check, it fails the local-batch lookup
        // (kind = foreign), the poison marker persists atomically with the
        // sync, the frontier freezes, and `check_danger` reports
        // `CanonicalDivergence` ahead of every other arm.
        let db = temp_db("frontier-future-safe-block");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        let future_safe_block_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                user_ops: Vec::new(),
                safe_block: 500,
                fee_price: 0,
            }],
        });

        let batch_submitter = Address::repeat_byte(0xCC);
        let protocol = ProtocolTiming {
            max_wait_blocks: u64::MAX,
            preemptive_margin_blocks: 75,
            l1_read_stale_after_blocks: 900,
            seconds_per_block: 12,
        };
        let inputs = vec![StoredSafeInput {
            sender: batch_submitter,
            payload: future_safe_block_payload,
            block_number: 100,
        }];
        storage
            .append_safe_inputs(200, inputs.as_slice(), batch_submitter, &protocol)
            .expect("append commits; the marker rides the same transaction");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(
            frontier.accepted_next_nonce, 0,
            "the frontier must freeze before the diverged landing"
        );
        let status = storage
            .check_danger(&protocol, unix_now_ms())
            .expect("check_danger");
        assert_eq!(
            status,
            crate::storage::DangerStatus::CanonicalDivergence(0),
            "the marker outranks every other danger arm"
        );
    }

    #[test]
    fn populate_accepts_post_recovery_batch_at_anchor() {
        // A cockroach-recovered deployment is anchored at N' > 0; its first
        // post-recovery batch lands at nonce N'. populate must seed `expected`
        // from the anchor and accept it (frontier → N'+1), content-identity
        // passing against the genuine local batch. Every other populate test
        // uses anchor=0, so the N' > 0 acceptance path had no coverage.
        let db = temp_db("populate-at-anchor");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_test_protocol();
        let submitter = SENDER_A;

        // Root the tree at N' = 7, then build + seal one local batch — it carries
        // nonce 7 (compute_next_nonce(None) reads the anchor).
        storage.set_batch_tree_anchor(7).expect("anchor at N'=7");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close the nonce-7 batch");
        assert_eq!(
            storage.batch_nonce(0).expect("nonce"),
            7,
            "the root tip carries the anchor nonce N'"
        );

        // Its genuine wire bytes (looked up by nonce N'=7) land from the
        // submitter at a fresh inclusion block.
        let genuine = local_batch_payload(&mut storage, 7);
        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: submitter,
                    payload: genuine,
                    block_number: 20,
                }],
                submitter,
                &protocol,
            )
            .expect("sync the N' landing");

        assert_eq!(
            storage
                .submitter_frontier()
                .expect("frontier")
                .accepted_next_nonce,
            8,
            "the N'=7 batch is accepted; the frontier advances to 8"
        );
        let divergence: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM canonical_divergence", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(
            divergence, 0,
            "a genuine N' landing must not flag divergence"
        );
    }

    #[test]
    fn populate_skips_below_anchor_collapsed_history_without_divergence() {
        // Below the anchor the local tree has no batches (that history was folded
        // into S'). Old-generation L1 landings at nonces < N' are trusted
        // collapsed history: skipped by nonce-mismatch BEFORE the content-identity
        // check, so they must NOT be flagged Foreign (which would freeze the
        // frontier and force a false recovery). Pins the anchor-aware-frontier
        // (I15) skip arm — the inverse of the foreign-batch freeze test above.
        let db = temp_db("populate-below-anchor");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_test_protocol();
        let submitter = SENDER_A;
        storage.set_batch_tree_anchor(7).expect("anchor at N'=7");

        // Old-generation batches at nonces 0, 1, 2 (< anchor) land from the submitter.
        let mk = |nonce: u64| {
            ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                nonce,
                frames: vec![sequencer_core::batch::Frame {
                    user_ops: Vec::new(),
                    safe_block: 10,
                    fee_price: 0,
                }],
            })
        };
        let landings: Vec<StoredSafeInput> = (0..3)
            .map(|n| StoredSafeInput {
                sender: submitter,
                payload: mk(n),
                block_number: 20 + n,
            })
            .collect();
        storage
            .append_safe_inputs(30, &landings, submitter, &protocol)
            .expect("sync below-anchor landings");

        let divergence: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM canonical_divergence", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(
            divergence, 0,
            "below-anchor history must be skipped by nonce-mismatch, not flagged Foreign"
        );
        assert_eq!(
            storage
                .submitter_frontier()
                .expect("frontier")
                .accepted_next_nonce,
            7,
            "nothing is accepted; the frontier stays at the anchor N'"
        );
    }

    #[test]
    fn pending_batches_skips_invalidated_and_respects_min_nonce() {
        let db = temp_db("load-pending-batches-filter");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        seed_closed_batches(&mut storage, 3);
        storage.insert_invalid_batch(1).expect("invalidate batch 1");

        // From nonce 0: batches 0 and 2 remain valid.
        let from_zero = storage
            .pending_batches(0)
            .expect("load pending batches from 0");
        let nonces: Vec<u64> = from_zero.iter().map(|b| b.nonce).collect();
        assert_eq!(nonces, vec![0, 2], "batch 1 must be filtered out");

        // From nonce 1: only batch 2 remains (batch 0 is below min_nonce).
        let from_one = storage
            .pending_batches(1)
            .expect("load pending batches from 1");
        let nonces: Vec<u64> = from_one.iter().map(|b| b.nonce).collect();
        assert_eq!(nonces, vec![2]);

        // Past the suffix: empty.
        let from_three = storage
            .pending_batches(3)
            .expect("load pending batches from 3");
        assert!(
            from_three.is_empty(),
            "no batch should remain at nonce >= 3"
        );
    }

    #[test]
    fn nonce_is_reused_after_torn_cascade() {
        // After a torn cascade invalidates every batch (including genesis),
        // the recovery batch has no valid ancestor. Its parent is NULL,
        // so its nonce resets to 0 — effectively reusing the nonce of the
        // original genesis. The scheduler's "expected next nonce" also
        // resets to 0, since no accepted batches were ever submitted.
        let db = temp_db("nonce-reuse-after-torn-cascade");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        storage.insert_invalid_batch(0).expect("invalidate batch 0");
        storage.insert_invalid_batch(1).expect("invalidate batch 1");
        storage
            .append_safe_inputs(10, &[], SENDER_A, &default_test_protocol())
            .expect("record observed safe head");
        storage
            .recover_post_flush(1200)
            .expect("open recovery batch after torn invalidation");

        let head = storage
            .open_state()
            .expect("load open state")
            .expect("recovery batch");
        assert_eq!(head.batch_index, 2);

        // Recovery Tip has no valid ancestor → parent NULL → nonce 0.
        let recovery_nonce: i64 = storage
            .conn
            .query_row(
                "SELECT nonce FROM batches WHERE batch_index = 2",
                [],
                |row| row.get(0),
            )
            .expect("query recovery nonce");
        assert_eq!(recovery_nonce, 0, "recovery Tip reuses nonce 0");
    }

    #[test]
    fn populate_safe_accepted_batches_skips_duplicate_nonces() {
        let db = temp_db("populate-dup-nonces");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_test_protocol();

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head, 10).expect("close");

        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                20,
                &[
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: landed.clone(),
                        block_number: 20,
                    },
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: landed,
                        block_number: 20,
                    },
                ],
                SENDER_A,
                &protocol,
            )
            .expect("append");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(
            frontier.accepted_next_nonce, 1,
            "duplicate nonce must be skipped"
        );
    }

    #[test]
    fn populate_safe_accepted_batches_handles_large_nonce_gap() {
        let db = temp_db("populate-nonce-gap");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_test_protocol();

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head, 10).expect("close");

        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: super::super::test_helpers::make_stale_batch_payload(5, 10),
                    block_number: 20,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(frontier.accepted_next_nonce, 0, "gap must stall frontier");
    }

    #[test]
    fn populate_safe_accepted_batches_out_of_order_arrivals_stalls_frontier() {
        let db = temp_db("populate-out-of-order");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let protocol = default_test_protocol();

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head, 10).expect("close");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close 2");

        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: super::super::test_helpers::make_stale_batch_payload(1, 10),
                    block_number: 20,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append");

        let frontier = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(
            frontier.accepted_next_nonce, 0,
            "out of order must stall frontier"
        );

        let landed = local_batch_payload(&mut storage, 0);
        storage
            .append_safe_inputs(
                21,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: landed,
                    block_number: 21,
                }],
                SENDER_A,
                &protocol,
            )
            .expect("append nonce 0");

        let frontier2 = storage.submitter_frontier().expect("submitter frontier");
        assert_eq!(
            frontier2.accepted_next_nonce, 1,
            "frontier must remain stalled"
        );
    }
}
