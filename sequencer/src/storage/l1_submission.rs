// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The submitter's storage half: frontier lookup, the SSZ-encoded pending-batch
//! list, and the wallet-nonce watermark. `load_batch_frames_in` is also the seal
//! path's encoder, so the hash stamped at seal and the submitted bytes come from
//! one encode path.

use rusqlite::{Result, params};

use super::Storage;
use super::convert::{external_u64_to_i64, i64_to_u16, i64_to_u32, i64_to_u64, u64_to_i64};
use super::queries::current_safe_block_required;
use super::safe_accepted_batches::frontier_nonce;
use super::{FrameHeader, PendingBatch, SubmitterFrontier};
use sequencer_core::batch::{Batch, Frame as BatchFrame, WireUserOp};

impl Storage {
    /// Read-only frontier view used by the submitter each tick to derive the
    /// next batch nonce. `accepted_next_nonce` is the next nonce the scheduler
    /// is expected to accept, derived from `safe_accepted_batches`.
    ///
    /// The scheduler-accepted frontier is maintained by
    /// `Storage::append_ingested_safe_inputs_with_timestamp`, so this is a pure read.
    ///
    /// **Precondition:** at least one safe-head observation must have been
    /// recorded (by `Storage::append_ingested_safe_inputs_with_timestamp`). In production this is
    /// always true because the startup recovery procedure completes InitialSync and
    /// refuses admission unless its persisted view is usable. Tests must seed
    /// an observation explicitly; a fresh DB returns `QueryReturnedNoRows`.
    pub fn submitter_frontier(&mut self) -> Result<SubmitterFrontier> {
        self.read(|tx| {
            Ok(SubmitterFrontier {
                safe_block: current_safe_block_required(tx)?,
                accepted_next_nonce: frontier_nonce(tx)?,
            })
        })
    }

    /// The highest wallet nonce ever broadcast by this deployment's
    /// batch-submitter key, or `None` if nothing was ever broadcast
    /// (the durable realization of the TLA+ `walletNonce`).
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

    /// Write-before-broadcast: durably raise the watermark to cover
    /// `wallet_nonce` *before* any tx at a wallet nonce `<= wallet_nonce` is
    /// sent. The
    /// commit is power-loss durable (`synchronous=FULL`); a crash between
    /// commit and send only over-covers — the flush later no-ops a
    /// never-used slot, which is harmless. Monotonic: never lowers.
    pub fn raise_wallet_nonce_watermark(&mut self, wallet_nonce: u64) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO wallet_nonce_watermark (singleton_id, watermark) \
                 VALUES (0, ?1) \
                 ON CONFLICT(singleton_id) \
                 DO UPDATE SET watermark = MAX(watermark, excluded.watermark)",
                params![external_u64_to_i64(
                    wallet_nonce,
                    "batch-submitter wallet nonce"
                )?],
            )?;
            Ok(())
        })
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
/// from **the same encode path the submitter uses**; this
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
        SENDER_A, default_protocol_timing, seed_closed_batches, seed_safe_inputs_with_batch_nonces,
    };
    use crate::storage::{SafeInputRange, Storage};
    use crate::test_support::temp_db;
    use sequencer_core::batch::Batch;

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
        let frames = super::frames_for_batch_in(&storage.conn, 0).expect("load frames for batch 0");
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

        let err = storage
            .raise_wallet_nonce_watermark(i64::MAX as u64 + 1)
            .expect_err("external wallet nonce outside SQLite range must be refused");
        assert!(matches!(err, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(
            storage.wallet_nonce_watermark().expect("read"),
            Some(9),
            "failed external raise must not alter the durable watermark"
        );
    }

    #[test]
    fn submitter_frontier_returns_zero_when_no_batches_were_accepted() {
        let db = temp_db("submitter-frontier-empty");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
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
            .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
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
            .append_safe_inputs(10, &[], SENDER_A, &default_protocol_timing())
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
}
