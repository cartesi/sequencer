// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Inclusion-lane writer: opens the initial batch/frame, appends user-op chunks,
//! and rotates frame/batch boundaries on the hot path.
//!
//! The lane also reads `safe_inputs` (executed by the application) and the open
//! state (resumed on startup) — those reads live here too because they're driven
//! by the lane's flow, not by an L1 ingress event.

use alloy_primitives::Address;
use rusqlite::{Result, Transaction, TransactionBehavior, params};

use super::internals::{
    from_unix_ms, i64_to_u64, insert_new_batch, insert_open_frame, load_current_write_head,
    now_unix_ms, persist_frame_direct_sequence, query_batch_policy, seal_batch, to_unix_ms,
    u64_to_i64,
};
use super::{
    BatchPolicy, SafeInputFrontier, SafeInputRange, Storage, StoredSafeInput, WriteHead,
    batch_size_target_bytes,
};
use crate::ingress::inclusion_lane::PendingUserOp;

impl Storage {
    /// Cursor for the next safe input to drain into a frame. Reads the highest
    /// already-drained `safe_input_index` from the valid (non-invalidated)
    /// `sequenced_l2_txs` rows and returns `MAX + 1` (or 0 if none).
    ///
    /// Using `MAX + 1` instead of `COUNT(*)` makes this robust against gaps:
    /// when a batch is invalidated, those rows drop out of the view and the
    /// cursor naturally rewinds, allowing the recovery batch to re-drain.
    pub fn load_next_undrained_safe_input_index(&mut self) -> Result<u64> {
        const SQL: &str = "
            SELECT COALESCE(MAX(safe_input_index) + 1, 0)
            FROM valid_sequenced_l2_txs
            WHERE safe_input_index IS NOT NULL
        ";
        let value: i64 = self.conn.query_row(SQL, [], |row| row.get(0))?;
        Ok(i64_to_u64(value))
    }

    /// Resume the lane on startup. Returns `None` if storage is empty (caller
    /// should follow up with [`Storage::initialize_open_state`]).
    pub fn load_open_state(&mut self) -> Result<Option<WriteHead>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let head = load_current_write_head(&tx)?;
        tx.commit()?;
        Ok(head)
    }

    /// Bootstrap the very first batch + frame. Asserts that no open state
    /// exists; call only when [`Storage::load_open_state`] returns `None`.
    pub fn initialize_open_state(
        &mut self,
        safe_block: u64,
        leading_direct_range: SafeInputRange,
    ) -> Result<WriteHead> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert!(
            load_current_write_head(&tx)?.is_none(),
            "open state already exists"
        );

        let now_ms = now_unix_ms();
        let policy = query_batch_policy(&tx)?;
        // Genesis: explicit batch_index = 0, parent = None, nonce = 0.
        insert_new_batch(&tx, Some(0), None, now_ms)?;
        insert_open_frame(&tx, 0, 0, now_ms, policy.recommended_fee, safe_block)?;
        persist_frame_direct_sequence(&tx, 0, 0, leading_direct_range)?;
        tx.commit()?;

        Ok(WriteHead {
            batch_index: 0,
            batch_created_at: from_unix_ms(now_ms),
            frame_fee: policy.recommended_fee,
            safe_block,
            batch_user_op_count: 0,
            open_frame_user_op_count: 0,
            frame_in_batch: 0,
            max_batch_user_op_bytes: batch_size_target_bytes(policy),
        })
    }

    /// Snapshot the current L1 view: safe block + exclusive safe-input cursor.
    /// The lane uses this to decide whether to advance.
    pub fn load_safe_input_frontier(&mut self) -> Result<SafeInputFrontier> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let safe_block = super::internals::query_current_safe_block(&tx)?;
        let end_exclusive = super::internals::query_latest_safe_input_index_exclusive(&tx)?;
        tx.commit()?;
        Ok(SafeInputFrontier {
            safe_block,
            end_exclusive,
        })
    }

    /// Replace `out`'s contents with the safe-input rows in `range`. Asserts
    /// contiguity — gaps in `safe_input_index` are a bug, not a runtime
    /// condition.
    pub fn fill_safe_inputs(
        &mut self,
        range: SafeInputRange,
        out: &mut Vec<StoredSafeInput>,
    ) -> Result<()> {
        out.clear();
        if range.is_empty() {
            return Ok(());
        }

        const SQL: &str = "
            SELECT safe_input_index, sender, payload, block_number
            FROM safe_inputs
            WHERE safe_input_index >= ?1 AND safe_input_index < ?2
            ORDER BY safe_input_index ASC
        ";
        let mut stmt = self.conn.prepare_cached(SQL)?;
        let rows = stmt.query_map(
            params![u64_to_i64(range.start()), u64_to_i64(range.end())],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;

        let mut fetched_count = 0_u64;
        for (offset, row) in rows.enumerate() {
            let (index_i64, sender, payload, block_number_i64) = row?;
            let index = i64_to_u64(index_i64);
            let expected = range.start().saturating_add(offset as u64);

            assert_eq!(
                index, expected,
                "non-contiguous safe-input index: expected {expected}, found {index}"
            );

            out.push(StoredSafeInput {
                sender: Address::from_slice(sender.as_slice()),
                payload,
                block_number: i64_to_u64(block_number_i64),
            });
            fetched_count = fetched_count.saturating_add(1);
        }

        assert_eq!(
            range.start().saturating_add(fetched_count),
            range.end(),
            "safe-input range {range:?} not fully populated"
        );

        Ok(())
    }

    /// Persist a chunk of user ops into the open frame and bump `head`'s
    /// counters.
    ///
    /// `head` is treated as authoritative: the lane is the only writer of
    /// open-frame state, so a stale `WriteHead` indicates a bug in the lane,
    /// not a runtime condition. The schema's FK + PK constraints catch the
    /// dangerous failure modes (write to a non-existent frame, duplicate
    /// `pos_in_frame`) by failing the INSERT.
    pub fn append_user_ops_chunk(
        &mut self,
        head: &mut WriteHead,
        user_ops: &[PendingUserOp],
    ) -> Result<()> {
        if user_ops.is_empty() {
            return Ok(());
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_user_ops_batch(
            &tx,
            head.batch_index,
            head.frame_in_batch,
            head.open_frame_user_op_count,
            user_ops,
        )?;

        tx.commit()?;
        head.increment_batch_user_op_count(user_ops.len());
        Ok(())
    }

    /// Rotate to the next frame inside the same batch. Used when the safe
    /// block advances but batch policy hasn't triggered a batch close — the
    /// new frame inherits the batch and gets a fresh fee/safe-block.
    pub fn close_frame_only(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        leading_direct_range: SafeInputRange,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = now_unix_ms();
        let policy = query_batch_policy(&tx)?;
        let next_frame_in_batch = head.frame_in_batch.saturating_add(1);
        insert_open_frame(
            &tx,
            head.batch_index,
            next_frame_in_batch,
            now_ms,
            policy.recommended_fee,
            next_safe_block,
        )?;
        persist_frame_direct_sequence(
            &tx,
            head.batch_index,
            next_frame_in_batch,
            leading_direct_range,
        )?;
        tx.commit()?;
        head.advance_frame(policy, next_safe_block);
        Ok(())
    }

    /// Close the current batch and open a fresh one with its first frame.
    /// Used when batch policy (size/deadline) triggers a batch close.
    ///
    /// Atomically: seal the current Tip (sets `sealed_at_ms`), insert the new
    /// Tip with `parent_batch_index = head.batch_index`, open its first frame.
    /// Order matters: sealing first removes the old row from the
    /// `ux_single_valid_tip` partial index, making room for the new Tip.
    pub fn close_frame_and_batch(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_ms = now_unix_ms();
        // Batch policy is sampled here: the derived fee is committed to the newly
        // opened frame, and the batch size target is stored on the write head.
        let policy = query_batch_policy(&tx)?;
        seal_batch(&tx, head.batch_index, now_ms)?;
        let next_batch_index = insert_new_batch(&tx, None, Some(head.batch_index), now_ms)?;
        insert_open_frame(
            &tx,
            next_batch_index,
            0,
            now_ms,
            policy.recommended_fee,
            next_safe_block,
        )?;
        tx.commit()?;
        head.move_to_next_batch(
            next_batch_index,
            from_unix_ms(now_ms),
            policy,
            next_safe_block,
        );
        Ok(())
    }

    pub fn batch_policy(&mut self) -> Result<BatchPolicy> {
        query_batch_policy(&self.conn)
    }
}

/// Insert user ops into `user_ops`. The `trg_sequence_user_op` trigger then
/// appends the matching `sequenced_l2_txs` row for each insert.
fn insert_user_ops_batch(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    frame_pos_start: u32,
    user_ops: &[PendingUserOp],
) -> Result<()> {
    if user_ops.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO user_ops (
            batch_index, frame_in_batch, pos_in_frame,
            sender, nonce, max_fee, data, sig, received_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for (offset, item) in user_ops.iter().enumerate() {
        let pos_in_frame = frame_pos_start.saturating_add(offset as u32);
        let sig = item.signed.signature.as_bytes();
        stmt.execute(params![
            u64_to_i64(batch_index),
            i64::from(frame_in_batch),
            i64::from(pos_in_frame),
            item.signed.sender.as_slice(),
            i64::from(item.signed.user_op.nonce),
            i64::from(item.signed.user_op.max_fee),
            item.signed.user_op.data.as_ref(),
            &sig[..],
            to_unix_ms(item.received_at),
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::storage::{
        SafeInputRange, Storage, StoredSafeInput,
        test_helpers::{default_protocol_config, temp_db},
    };
    use alloy_primitives::Address;
    use sequencer_core::l2_tx::SequencedL2Tx;

    #[test]
    fn open_state_is_idempotent_and_rotation_is_atomic() {
        let db = temp_db("open-state");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        assert!(
            storage
                .load_open_state()
                .expect("load open state")
                .is_none(),
            "fresh storage should not have an open frame yet"
        );

        let head_a = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let head_b = storage
            .load_open_state()
            .expect("load existing open state")
            .expect("open state should now exist");

        assert_eq!(head_a.batch_index, head_b.batch_index);
        assert_eq!(head_a.frame_in_batch, head_b.frame_in_batch);
        assert_eq!(head_a.frame_fee, head_b.frame_fee);
        // Default log_recommended_fee = 0+20+419+621 = 1060
        assert_eq!(head_a.frame_fee, 1060);

        let mut head_c = head_b;
        let next_safe_block = head_c.safe_block;
        storage
            .close_frame_only(&mut head_c, next_safe_block, SafeInputRange::empty_at(0))
            .expect("rotate within same batch");
        assert_eq!(head_c.batch_index, head_b.batch_index);
        assert_eq!(head_c.frame_in_batch, 1);

        let mut head_d = head_c;
        let next_safe_block = head_d.safe_block;
        storage
            .close_frame_and_batch(&mut head_d, next_safe_block)
            .expect("close batch and rotate");
        assert!(head_d.batch_index > head_c.batch_index);
        assert_eq!(head_d.frame_in_batch, 0);
    }

    #[test]
    fn next_frame_fee_comes_from_batch_policy() {
        let db = temp_db("batch-policy-fee");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let policy = storage.batch_policy().expect("default policy");
        // Default: log_gas_price=0, log_recommended_fee = 0+20+419+621 = 1060
        assert_eq!(policy.recommended_fee, 1060);

        storage.set_log_gas_price(100).expect("set log gas price");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let next_safe_block = head.safe_block;
        storage
            .close_frame_and_batch(&mut head, next_safe_block)
            .expect("rotate batch");

        let policy = storage.batch_policy().expect("read policy");
        // log_recommended_fee = 100+20+419+621 = 1160
        assert_eq!(head.frame_fee, 1160);
        assert_eq!(head.frame_fee, policy.recommended_fee);
        assert!(
            head.max_batch_user_op_bytes > 0,
            "batch size target should be set"
        );
    }

    #[test]
    fn frame_fee_is_immutable_for_the_lifetime_of_the_frame() {
        // §3.2.3: once a frame is opened at fee F, a policy update mid-frame
        // must NOT change the open frame's committed fee. Only the *next*
        // frame (after close) sees the new policy. This pins the write-once
        // contract `frames.fee` relies on — users submitting against the open
        // frame know the fee they're paying, regardless of upstream policy
        // drift during their round-trip.
        let db = temp_db("frame-fee-immutable");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let original_batch_index = head.batch_index;
        let original_frame_in_batch = head.frame_in_batch;
        // Default: log_gas_price=0 → log_recommended_fee = 0+20+419+621 = 1060
        assert_eq!(head.frame_fee, 1060);

        // Simulate an operator policy update mid-frame: fee oracle reports a
        // higher gas price. The derived view reflects the new fee immediately.
        storage
            .set_log_gas_price(100)
            .expect("set higher log gas price");
        let new_policy = storage.batch_policy().expect("read updated policy");
        assert_eq!(
            new_policy.recommended_fee, 1160,
            "policy-derived fee should reflect the new gas price",
        );

        // Invariant: the already-open frame's persisted fee stays at 1060.
        let persisted_frame_fee: i64 = storage
            .conn
            .query_row(
                "SELECT fee FROM frames WHERE batch_index = ?1 AND frame_in_batch = ?2",
                rusqlite::params![original_batch_index as i64, original_frame_in_batch as i64,],
                |row| row.get(0),
            )
            .expect("query open frame fee");
        assert_eq!(
            persisted_frame_fee, 1060,
            "open frame's committed fee must not change across policy updates",
        );

        // And the in-memory WriteHead mirror must also be stable — the lane
        // submitting against this head should see a consistent fee.
        assert_eq!(
            head.frame_fee, 1060,
            "WriteHead.frame_fee must stay stable until advance_frame runs",
        );

        // Closing the frame picks up the new policy — the *next* frame opens
        // at 1160. This is the expected policy-flow boundary.
        let next_safe_block = head.safe_block;
        storage
            .close_frame_only(&mut head, next_safe_block, SafeInputRange::empty_at(0))
            .expect("rotate within same batch");
        assert_eq!(
            head.frame_fee, 1160,
            "the next frame must use the updated policy's fee (policy flows in at close)",
        );
    }

    #[test]
    fn next_undrained_safe_input_index_is_derived_from_sequenced_directs() {
        let db = temp_db("safe-cursor");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        assert_eq!(
            storage
                .load_next_undrained_safe_input_index()
                .expect("empty cursor"),
            0
        );

        let head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let drained = vec![
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0x00],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0x02],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, drained.as_slice(), &default_protocol_config())
            .expect("insert direct inputs");
        let mut head = head;
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, drained.len() as u64))
            .expect("close frame with directs");

        assert_eq!(
            storage
                .load_next_undrained_safe_input_index()
                .expect("derived cursor"),
            2
        );
    }

    #[test]
    fn initialize_open_state_creates_first_real_batch_and_frame() {
        let db = temp_db("initialize-open-state");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let head = storage
            .initialize_open_state(12, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        assert_eq!(head.batch_index, 0);
        assert_eq!(head.frame_in_batch, 0);
        assert_eq!(head.safe_block, 12);

        let loaded = storage
            .load_open_state()
            .expect("load open state")
            .expect("open state should exist");
        assert_eq!(loaded.batch_index, 0);
        assert_eq!(loaded.frame_in_batch, 0);
        assert_eq!(loaded.safe_block, 12);
    }

    #[test]
    fn replay_returns_direct_inputs_in_drain_order() {
        let db = temp_db("replay-order");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        let drained = vec![
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, drained.as_slice(), &default_protocol_config())
            .expect("insert direct inputs");
        let mut head = head;
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, drained.len() as u64))
            .expect("close frame with directs");

        let replay = storage
            .load_ordered_l2_txs_page_from(0, 100)
            .expect("load replay");
        assert_eq!(replay.len(), 2);
        match &replay[0].1 {
            SequencedL2Tx::Direct(value) => assert_eq!(value.payload.as_slice(), &[0xaa]),
            _ => panic!("expected direct input at position 0"),
        }
        match &replay[1].1 {
            SequencedL2Tx::Direct(value) => assert_eq!(value.payload.as_slice(), &[0xbb]),
            _ => panic!("expected direct input at position 1"),
        }
    }
}
