// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter writer: assigns nonces, populates the scheduler-accepted
//! frontier, and exposes the read-only queries that drive each tick (frontier
//! lookup, danger-zone check, pending-batch loading).
//!
//! Recovery shares all of these — `recovery::run_startup_recovery` calls the
//! same helpers under one transaction. The split is by *frequency*: this file
//! is what runs every tick; recovery is the once-per-startup composer.

use rusqlite::{OptionalExtension, Result, TransactionBehavior, params};

use super::Storage;
use super::internals::{
    decode_l2_tx_row, i64_to_u16, i64_to_u32, i64_to_u64, query_current_safe_block, u64_to_i64,
};
use super::safe_accepted_batches::query_latest_safe_accepted_batch;
use super::{FrameHeader, PendingBatch};
use sequencer_core::batch::{Batch, BatchForSubmission, Frame as BatchFrame, WireUserOp};
use sequencer_core::l2_tx::SequencedL2Tx;

impl Storage {
    /// Read-only frontier view used by the submitter each tick to derive the
    /// next batch nonce. Returns `(current_safe_block, safe_next_expected_nonce)`.
    ///
    /// The scheduler-accepted frontier is maintained by
    /// [`Storage::append_safe_inputs`], so this is a pure read.
    pub fn submitter_frontier_view(&mut self) -> Result<(u64, u64)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let safe_block = query_current_safe_block(&tx)?;
        let next_expected_nonce = query_latest_safe_accepted_batch(&tx)?
            .map(|row| i64_to_u64(row.nonce).saturating_add(1))
            .unwrap_or(0);
        tx.commit()?;
        Ok((safe_block, next_expected_nonce))
    }

    /// Load the scheduler-accepted safe frontier persisted in `safe_accepted_batches`.
    ///
    /// Test-only alias for [`Self::submitter_frontier_view`]. Several tests
    /// were written against this name before the submitter_frontier_view
    /// rename; keep it for continuity.
    #[cfg(test)]
    pub fn load_safe_accepted_frontier(&mut self) -> Result<(u64, u64)> {
        self.submitter_frontier_view()
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

    /// Frame headers for `batch_index` in `frame_in_batch` order. Reads the
    /// raw `frames` table — does NOT filter on validity, since callers only
    /// reach this method after they already know the batch is valid.
    pub fn load_frames_for_batch(&mut self, batch_index: u64) -> Result<Vec<FrameHeader>> {
        let mut stmt = self.conn.prepare_cached(
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

    /// Materialize all sequenced L2 txs in one batch (used by the catch-up /
    /// per-batch replay paths). Returns `[]` for invalidated batches.
    pub fn load_ordered_l2_txs_for_batch(
        &mut self,
        batch_index: u64,
    ) -> Result<Vec<SequencedL2Tx>> {
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

    /// Assemble a batch (header + frames + user ops) for SSZ encoding and L1
    /// submission. The returned [`BatchForSubmission`] carries a placeholder
    /// nonce of 0; callers stamp the real nonce via `encode_for_scheduler_with_nonce`.
    pub fn load_batch_for_submission(&mut self, batch_index: u64) -> Result<BatchForSubmission> {
        let created_at_ms: i64 = self.conn.query_row(
            "SELECT created_at_ms FROM batches WHERE batch_index = ?1 LIMIT 1",
            [u64_to_i64(batch_index)],
            |row| row.get(0),
        )?;

        let frame_headers = self.load_frames_for_batch(batch_index)?;
        let mut frames = Vec::with_capacity(frame_headers.len());

        for header in frame_headers {
            let mut stmt = self.conn.prepare_cached(
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

        // Nonce is a placeholder — callers use encode_for_scheduler_with_nonce() to set the real one.
        let batch = Batch { nonce: 0, frames };
        let created_at_ms_u64 = created_at_ms.max(0) as u64;

        Ok(BatchForSubmission {
            batch_index,
            created_at_ms: created_at_ms_u64,
            batch,
        })
    }

    /// Load the next valid closed batch that needs to be submitted.
    pub fn load_next_batch_to_submit(&mut self, min_nonce: u64) -> Result<Option<PendingBatch>> {
        const SQL: &str = "SELECT batch_index, nonce FROM valid_closed_batches \
                           WHERE nonce >= ?1 ORDER BY nonce ASC LIMIT 1";
        let batch_ref: Option<(i64, i64)> = self
            .conn
            .query_row(SQL, params![u64_to_i64(min_nonce)], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        let Some((batch_index, nonce)) = batch_ref else {
            return Ok(None);
        };

        let batch_index = i64_to_u64(batch_index);
        let nonce = i64_to_u64(nonce);
        let batch = self.load_batch_for_submission(batch_index)?;
        let encoded = batch.encode_for_scheduler_with_nonce(nonce);
        Ok(Some(PendingBatch {
            batch_index,
            nonce,
            encoded,
        }))
    }

    /// Load all valid closed batches with nonce >= `min_nonce`, in nonce order.
    pub fn load_pending_batches(&mut self, min_nonce: u64) -> Result<Vec<PendingBatch>> {
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
            let batch = self.load_batch_for_submission(batch_index)?;
            let encoded = batch.encode_for_scheduler_with_nonce(nonce);
            batches.push(PendingBatch {
                batch_index,
                nonce,
                encoded,
            });
        }
        Ok(batches)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{
        SENDER_A, SENDER_B, scheduler_rules_for, seed_closed_batches,
        seed_safe_inputs_with_batch_nonces, temp_db,
    };
    use crate::storage::{SafeInputRange, SchedulerRules, Storage, StoredSafeInput};
    use alloy_primitives::Address;
    use sequencer_core::batch::{Batch, Frame as BatchFrame};

    #[test]
    fn batch_for_submission_builds_from_storage() {
        let db = temp_db("batch-for-submission");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let head = storage
            .initialize_open_state(12, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        assert_eq!(head.batch_index, 0);

        let batch = storage
            .load_batch_for_submission(0)
            .expect("load batch for submission");

        assert_eq!(batch.batch_index, 0);
        assert_eq!(batch.batch.frames.len(), 1);
        let frame = &batch.batch.frames[0];
        assert!(frame.user_ops.is_empty());
        assert_eq!(frame.safe_block, 12);
        // Default log_recommended_fee = 0+20+419+621 = 1060
        assert_eq!(frame.fee_price, 1060);
        assert!(batch.created_at_ms > 0);
    }

    #[test]
    fn batch_level_helpers_expose_latest_index_frames_and_txs() {
        let db = temp_db("batch-level-helpers");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

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
            .load_frames_for_batch(0)
            .expect("load frames for batch 0");
        assert!(!frames.is_empty());

        // Ordered L2 txs for batch 0 should be queryable (even if empty).
        let txs = storage
            .load_ordered_l2_txs_for_batch(0)
            .expect("load l2 txs for batch 0");
        assert!(
            txs.is_empty(),
            "fresh batch should not have sequenced txs yet"
        );
    }

    #[test]
    fn closed_batch_becomes_eligible_for_submission_with_assigned_nonce() {
        // §3.3.3: closing a batch transitions it from "open Tip" to "eligible
        // for L1 submission" — it appears in `valid_closed_batches` with a
        // nonce derived from its parent pointer. Pins the submitter's
        // contract: open batches are NOT pulled into the submission pipeline,
        // and closed batches ARE, at the schema-guaranteed nonce.
        let db = temp_db("closed-batch-eligible");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        // Before close: the open batch must not appear in pending-batches.
        let pending_before = storage
            .load_pending_batches(0)
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
            .load_pending_batches(0)
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
    fn load_safe_accepted_frontier_returns_zero_when_no_batches_were_accepted() {
        let db = temp_db("safe-accepted-frontier-empty");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let (safe_block, next) = storage
            .load_safe_accepted_frontier()
            .expect("load safe accepted frontier");
        assert_eq!(safe_block, 0);
        assert_eq!(next, 0);
    }

    #[test]
    fn load_safe_accepted_frontier_tracks_accepted_prefix() {
        let db = temp_db("safe-accepted-frontier-prefix");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        // seed_safe_inputs_with_batch_nonces already calls append_safe_inputs,
        // which auto-populates safe_accepted_batches.
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 10, &[0, 1, 3, 4, 5]);

        let (safe_block, next) = storage
            .load_safe_accepted_frontier()
            .expect("load safe accepted frontier");
        assert_eq!(safe_block, 10);
        assert_eq!(next, 2);
    }

    fn default_test_params() -> crate::recovery::RecoveryParams {
        crate::recovery::RecoveryParams {
            max_wait_blocks: 1200,
            danger_threshold: 1125,
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
    fn submitter_frontier_view_tracks_accepted_prefix() {
        let db = temp_db("submitter-frontier-view");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 10, &[0, 1, 3, 4]);

        let (safe_block, safe_next_expected_nonce) = storage
            .submitter_frontier_view()
            .expect("submitter frontier view");

        assert_eq!(safe_block, 10);
        assert_eq!(safe_next_expected_nonce, 2);
    }

    #[test]
    fn check_danger_reports_strict_on_closed_frontier() {
        let db = temp_db("check-danger-strict");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 1");

        let rules = SchedulerRules::new(SENDER_A, 1200);
        storage
            .append_safe_inputs(
                1135,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: ssz::Encode::as_ssz_bytes(&Batch {
                        nonce: 0,
                        frames: vec![BatchFrame {
                            user_ops: vec![],
                            safe_block: 10,
                            fee_price: 0,
                        }],
                    }),
                    block_number: 20,
                }],
                &rules,
            )
            .expect("append accepted batch 0");

        let status = storage
            .check_danger(default_test_params(), unix_now_ms())
            .expect("check_danger");
        assert_eq!(status, crate::storage::DangerStatus::Strict(1));
    }

    #[test]
    fn check_danger_reports_stalled_on_wall_clock_drift() {
        // Strict block-based check wouldn't fire (batch 1 has first_frame_safe_block
        // = 100 and safe_block = 200, age = 100 < 1125). But wall-clock says the
        // safe head hasn't advanced in ~25 blocks — effective threshold drops to
        // 1100, batch 1's age jumps past it via the wall-clock correction.
        let db = temp_db("check-danger-stalled");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 1");

        let rules = SchedulerRules::new(SENDER_A, 1200);
        storage
            .append_safe_inputs(
                1200,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: ssz::Encode::as_ssz_bytes(&Batch {
                        nonce: 0,
                        frames: vec![BatchFrame {
                            user_ops: vec![],
                            safe_block: 100,
                            fee_price: 0,
                        }],
                    }),
                    block_number: 200,
                }],
                &rules,
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
            .check_danger(default_test_params(), now_ms)
            .expect("check_danger");
        assert_eq!(status, crate::storage::DangerStatus::Stalled(1));
    }

    #[test]
    fn check_danger_safe_when_never_synced() {
        // Fresh DB, no prior safe-progress observation. check_danger reports
        // Safe — never-synced is benign at this layer; callers that need
        // "refuse on never-synced" (startup L1-unreachable) check explicitly.
        let db = temp_db("check-danger-never-synced");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let status = storage
            .check_danger(default_test_params(), unix_now_ms())
            .expect("check_danger");
        assert_eq!(status, crate::storage::DangerStatus::Safe);
    }

    #[test]
    fn populate_safe_accepted_batches_resumes_from_latest_row() {
        let db = temp_db("safe-accepted-frontier-resume");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let rules = scheduler_rules_for(SENDER_A);

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
                payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                    nonce: 2,
                    frames: Vec::new(),
                }),
                block_number: 11,
            },
            StoredSafeInput {
                sender: SENDER_A,
                payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                    nonce: 3,
                    frames: Vec::new(),
                }),
                block_number: 11,
            },
        ];
        storage
            .append_safe_inputs(11, second_wave.as_slice(), &rules)
            .expect("append second wave");

        let (safe_block, next) = storage
            .load_safe_accepted_frontier()
            .expect("load safe accepted frontier");
        assert_eq!(safe_block, 11);
        assert_eq!(next, 4);

        let accepted_count: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM safe_accepted_batches", [], |row| {
                row.get(0)
            })
            .expect("count accepted rows");
        assert_eq!(accepted_count, 4);
    }

    #[test]
    fn load_safe_accepted_frontier_skips_stale_payloads() {
        let db = temp_db("safe-accepted-frontier-skip-stale");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let rules = SchedulerRules::new(SENDER_A, 1200);

        // Seed a non-stale batch with nonce 0 (safe_block=100, block_number=200, max_wait=1200 → not stale)
        let non_stale_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                user_ops: Vec::new(),
                safe_block: 100,
                fee_price: 0,
            }],
        });
        // Seed a stale batch with nonce 1 (safe_block=100, block_number=2000, max_wait=1200 → stale)
        let stale_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 1,
            frames: vec![sequencer_core::batch::Frame {
                user_ops: Vec::new(),
                safe_block: 100,
                fee_price: 0,
            }],
        });
        // Seed a non-stale batch with nonce 1 (safe_block=1900, block_number=2000 → not stale)
        let non_stale_payload_2 = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 1,
            frames: vec![sequencer_core::batch::Frame {
                user_ops: Vec::new(),
                safe_block: 1900,
                fee_price: 0,
            }],
        });

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
            .append_safe_inputs(2000, inputs.as_slice(), &rules)
            .expect("append");

        let (_, next) = storage
            .load_safe_accepted_frontier()
            .expect("load safe accepted frontier");
        assert_eq!(next, 2);
    }

    #[test]
    fn frontier_accepts_future_safe_block_batch_by_design() {
        // The scheduler rejects batches where frame safe_block > inclusion_block,
        // but the sequencer trusts its own output and does not re-validate these
        // invariants during recovery. This test documents the intentional design
        // choice: populate_safe_accepted_batches accepts such batches because
        // the sequencer would never produce them.
        let db = temp_db("frontier-future-safe-block");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let future_safe_block_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                user_ops: Vec::new(),
                safe_block: 500,
                fee_price: 0,
            }],
        });
        let non_monotonic_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 1,
            frames: vec![
                sequencer_core::batch::Frame {
                    user_ops: Vec::new(),
                    safe_block: 200,
                    fee_price: 0,
                },
                sequencer_core::batch::Frame {
                    user_ops: Vec::new(),
                    safe_block: 100,
                    fee_price: 0,
                },
            ],
        });

        let batch_submitter = Address::repeat_byte(0xCC);
        let rules = SchedulerRules::new(batch_submitter, u64::MAX);
        let inputs = vec![
            StoredSafeInput {
                sender: batch_submitter,
                payload: future_safe_block_payload,
                block_number: 100,
            },
            StoredSafeInput {
                sender: batch_submitter,
                payload: non_monotonic_payload,
                block_number: 200,
            },
        ];
        storage
            .append_safe_inputs(200, inputs.as_slice(), &rules)
            .expect("append");

        let (_, next) = storage
            .load_safe_accepted_frontier()
            .expect("load safe accepted frontier");
        assert_eq!(next, 2, "both batches should be in accepted frontier");
    }

    #[test]
    fn load_next_batch_to_submit_returns_nonce_ordered_valid_suffix() {
        let db = temp_db("load-next-batch-to-submit");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        seed_closed_batches(&mut storage, 3);
        storage.insert_invalid_batch(1).expect("invalidate batch 1");

        let first = storage
            .load_next_batch_to_submit(0)
            .expect("load first pending batch")
            .expect("batch 0 should be pending");
        assert_eq!(first.batch_index, 0);
        assert_eq!(first.nonce, 0);

        let second = storage
            .load_next_batch_to_submit(1)
            .expect("load next pending batch")
            .expect("batch 2 should be pending");
        assert_eq!(second.batch_index, 2);
        assert_eq!(second.nonce, 2);

        let none = storage
            .load_next_batch_to_submit(3)
            .expect("load after suffix");
        assert!(none.is_none(), "no batch should remain at nonce >= 3");
    }

    #[test]
    fn nonce_is_reused_after_torn_cascade() {
        // After a torn cascade invalidates every batch (including genesis),
        // the recovery batch has no valid ancestor. Its parent is NULL,
        // so its nonce resets to 0 — effectively reusing the nonce of the
        // original genesis. The scheduler's "expected next nonce" also
        // resets to 0, since no accepted batches were ever submitted.
        let db = temp_db("nonce-reuse-after-torn-cascade");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        storage.insert_invalid_batch(0).expect("invalidate batch 0");
        storage.insert_invalid_batch(1).expect("invalidate batch 1");
        storage
            .detect_and_recover(1200)
            .expect("open recovery batch after torn invalidation");

        let head = storage
            .load_open_state()
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
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let rules = SchedulerRules::new(SENDER_A, 1200);

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head, 10).expect("close");

        storage
            .append_safe_inputs(
                20,
                &[
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: super::super::test_helpers::make_stale_batch_payload(0, 10),
                        block_number: 20,
                    },
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: super::super::test_helpers::make_stale_batch_payload(0, 10),
                        block_number: 20,
                    },
                ],
                &rules,
            )
            .expect("append");

        let (_, next) = storage
            .load_safe_accepted_frontier()
            .expect("load frontier");
        assert_eq!(next, 1, "duplicate nonce must be skipped");
    }

    #[test]
    fn populate_safe_accepted_batches_handles_large_nonce_gap() {
        let db = temp_db("populate-nonce-gap");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let rules = SchedulerRules::new(SENDER_A, 1200);

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
                &rules,
            )
            .expect("append");

        let (_, next) = storage
            .load_safe_accepted_frontier()
            .expect("load frontier");
        assert_eq!(next, 0, "gap must stall frontier");
    }

    #[test]
    fn populate_safe_accepted_batches_out_of_order_arrivals_stalls_frontier() {
        let db = temp_db("populate-out-of-order");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let rules = SchedulerRules::new(SENDER_A, 1200);

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
                &rules,
            )
            .expect("append");

        let (_, next) = storage
            .load_safe_accepted_frontier()
            .expect("load frontier");
        assert_eq!(next, 0, "out of order must stall frontier");

        storage
            .append_safe_inputs(
                21,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: super::super::test_helpers::make_stale_batch_payload(0, 10),
                    block_number: 21,
                }],
                &rules,
            )
            .expect("append nonce 0");

        let (_, next2) = storage
            .load_safe_accepted_frontier()
            .expect("load frontier again");
        assert_eq!(next2, 1, "frontier must remain stalled");
    }
}
