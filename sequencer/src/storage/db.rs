// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Result, Transaction, TransactionBehavior,
};
use rusqlite_migration::{M, Migrations};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::sql::{
    sql_count_user_ops_for_frame, sql_insert_batch_nonce, sql_insert_invalid_batch,
    sql_insert_open_batch, sql_insert_open_batch_with_index, sql_insert_open_frame,
    sql_insert_safe_accepted_batch, sql_insert_safe_inputs_batch,
    sql_insert_sequenced_direct_inputs, sql_insert_user_ops_batch, sql_select_batch_policy,
    sql_select_first_frame_safe_block, sql_select_frames_for_batch, sql_select_l1_bootstrap_cache,
    sql_select_l1_sync_timestamp, sql_select_latest_batch_index,
    sql_select_latest_batch_with_user_op_count, sql_select_latest_frame_in_batch_for_batch,
    sql_select_max_safe_input_index, sql_select_ordered_l2_tx_count,
    sql_select_ordered_l2_txs_for_batch, sql_select_ordered_l2_txs_from_offset,
    sql_select_ordered_l2_txs_page_from_offset, sql_select_safe_block,
    sql_select_safe_inputs_range, sql_select_total_drained_direct_inputs,
    sql_select_user_ops_for_frame, sql_touch_l1_sync, sql_update_batch_policy_alpha,
    sql_update_batch_policy_log_gas_price, sql_update_safe_block, sql_update_safe_block_bootstrap,
    sql_upsert_l1_bootstrap_cache,
};
use super::{
    BatchPolicy, FrameHeader, PendingBatch, SafeFrontier, SafeInputRange, StorageOpenError,
    StoredSafeInput, WriteHead,
};
use crate::inclusion_lane::PendingUserOp;
use alloy_primitives::Address;
use sequencer_core::batch::{Batch, BatchForSubmission, Frame as BatchFrame, WireUserOp};
use sequencer_core::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};

const MIGRATION_0001_SCHEMA: &str = include_str!("migrations/0001_schema.sql");

/// Sequencer storage backed by a single SQLite database.
///
/// All methods take `&mut self` to enforce exclusive access at the Rust level,
/// matching SQLite's single-writer model. Read-only access uses a separate
/// `Storage` instance opened via [`Storage::open_read_only`].
pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: &str, synchronous: &str) -> std::result::Result<Self, StorageOpenError> {
        let conn = Self::open_connection_with_migrations(path, synchronous)?;
        Ok(Self { conn })
    }

    pub fn open_without_migrations(
        path: &str,
        synchronous: &str,
    ) -> std::result::Result<Self, StorageOpenError> {
        let conn = Self::open_connection(path, synchronous)?;
        Ok(Self { conn })
    }

    pub fn open_read_only(path: &str) -> std::result::Result<Self, StorageOpenError> {
        let conn = Self::open_connection_read_only(path)?;
        Ok(Self { conn })
    }

    pub fn open_connection(
        path: &str,
        synchronous: &str,
    ) -> std::result::Result<Connection, StorageOpenError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", synchronous)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(conn)
    }

    pub fn open_connection_read_only(
        path: &str,
    ) -> std::result::Result<Connection, StorageOpenError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "query_only", "ON")?;
        // Readers should fail fast under write pressure to keep tail latency bounded.
        conn.pragma_update(None, "busy_timeout", 50)?;
        Ok(conn)
    }

    pub fn open_connection_with_migrations(
        path: &str,
        synchronous: &str,
    ) -> std::result::Result<Connection, StorageOpenError> {
        let mut conn = Self::open_connection(path, synchronous)?;
        Self::run_migrations(&mut conn)?;
        Ok(conn)
    }

    pub fn run_migrations(conn: &mut Connection) -> std::result::Result<(), StorageOpenError> {
        Migrations::from_slice(&[M::up(MIGRATION_0001_SCHEMA)]).to_latest(conn)?;
        Ok(())
    }

    pub fn load_next_undrained_safe_input_index(&mut self) -> Result<u64> {
        let value = sql_select_total_drained_direct_inputs(&self.conn)?;
        Ok(i64_to_u64(value))
    }

    pub fn safe_input_end_exclusive(&mut self) -> Result<u64> {
        let value = sql_select_max_safe_input_index(&self.conn)?;
        Ok(match value {
            Some(last_index) => i64_to_u64(last_index).saturating_add(1),
            None => 0,
        })
    }

    pub fn current_safe_block(&mut self) -> Result<u64> {
        let value = sql_select_safe_block(&self.conn)?;
        Ok(i64_to_u64(value))
    }

    pub fn ensure_minimum_safe_block(&mut self, minimum_safe_block: u64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_safe_block = query_current_safe_block(&tx)?;
        if current_safe_block < minimum_safe_block {
            let changed_rows =
                sql_update_safe_block_bootstrap(&tx, u64_to_i64(minimum_safe_block))?;
            if changed_rows != 1 {
                return Err(rusqlite::Error::StatementChangedRows(changed_rows));
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record that L1 was successfully queried at the current wall-clock time.
    pub fn touch_l1_sync(&mut self) -> Result<()> {
        let now_ms = now_unix_ms();
        let changed_rows = sql_touch_l1_sync(&self.conn, now_ms)?;
        if changed_rows != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed_rows));
        }
        Ok(())
    }

    pub fn load_safe_frontier(&mut self) -> Result<SafeFrontier> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let safe_block = query_current_safe_block(&tx)?;
        let end_exclusive = query_latest_safe_input_index_exclusive(&tx)?;
        tx.commit()?;
        Ok(SafeFrontier {
            safe_block,
            end_exclusive,
        })
    }

    /// Load the scheduler-accepted safe frontier persisted in `safe_accepted_batches`.
    ///
    /// Returns `(current_safe_block, next_expected_nonce)`.
    pub fn load_safe_accepted_frontier(&mut self) -> Result<(u64, u64)> {
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

    pub fn fill_safe_inputs(
        &mut self,
        from_inclusive: u64,
        to_exclusive: u64,
        out: &mut Vec<StoredSafeInput>,
    ) -> Result<()> {
        assert!(
            from_inclusive <= to_exclusive,
            "invalid safe-input interval [{from_inclusive}, {to_exclusive})"
        );

        if from_inclusive == to_exclusive {
            return Ok(());
        }

        let rows = sql_select_safe_inputs_range(
            &self.conn,
            u64_to_i64(from_inclusive),
            u64_to_i64(to_exclusive),
        )?;

        let mut fetched_count = 0_u64;
        for (offset, row) in rows.into_iter().enumerate() {
            let index = i64_to_u64(row.safe_input_index);
            let expected = from_inclusive.saturating_add(offset as u64);

            assert_eq!(
                index, expected,
                "non-contiguous safe-input index: expected {expected}, found {index}"
            );

            out.push(StoredSafeInput {
                sender: Address::from_slice(row.sender.as_slice()),
                payload: row.payload,
                block_number: i64_to_u64(row.block_number),
            });
            fetched_count = fetched_count.saturating_add(1);
        }

        assert_eq!(
            from_inclusive.saturating_add(fetched_count),
            to_exclusive,
            "safe-input interval [{from_inclusive}, {to_exclusive}) not fully populated"
        );

        Ok(())
    }

    pub fn append_safe_inputs(
        &mut self,
        safe_block: u64,
        inputs: &[StoredSafeInput],
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let current_safe_block = query_current_safe_block(&tx)?;
        assert!(
            safe_block >= current_safe_block,
            "safe block regressed: current={current_safe_block}, next={safe_block}"
        );
        assert!(
            safe_block > current_safe_block || inputs.is_empty(),
            "safe block must advance when appending new safe inputs"
        );

        let next_expected = query_latest_safe_input_index_exclusive(&tx)?;
        sql_insert_safe_inputs_batch(&tx, next_expected, inputs)?;
        let now_ms = now_unix_ms();
        let changed_rows = sql_update_safe_block(&tx, u64_to_i64(safe_block), now_ms)?;
        if changed_rows != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed_rows));
        }

        tx.commit()?;
        Ok(())
    }

    pub fn load_open_state(&mut self) -> Result<Option<WriteHead>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let head = load_current_write_head(&tx)?;
        tx.commit()?;
        Ok(head)
    }

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
        insert_open_batch_with_index(&tx, 0, now_ms)?;
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
            max_batch_user_op_bytes: super::batch_size_target_bytes(policy),
        })
    }

    pub fn batch_policy(&mut self) -> Result<BatchPolicy> {
        let (log_recommended_fee, log_batch_size_target) = sql_select_batch_policy(&self.conn)?;
        let max_exp = sequencer_core::fee::MAX_EXPONENT;
        Ok(BatchPolicy {
            // Clamp to MAX_EXPONENT to prevent panics in fee_to_linear.
            recommended_fee: i64_to_u16(log_recommended_fee).min(max_exp),
            batch_size_target: i64_to_u16(log_batch_size_target).min(max_exp),
        })
    }

    pub fn set_log_gas_price(&mut self, log_gas_price: u16) -> Result<()> {
        let changed_rows =
            sql_update_batch_policy_log_gas_price(&self.conn, i64::from(log_gas_price))?;
        if changed_rows != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed_rows));
        }
        Ok(())
    }

    pub fn set_alpha(&mut self, num: u64, denom: u64) -> Result<()> {
        use sequencer_core::fee::log_fee_ratio;

        let log_alpha = log_fee_ratio(num, denom);
        let one_plus_alpha_num = num.checked_add(denom).expect(
            "set_alpha: num + denom overflows u64; use smaller values for the alpha fraction",
        );
        let log_one_plus_alpha = log_fee_ratio(one_plus_alpha_num, denom);

        let changed_rows = sql_update_batch_policy_alpha(
            &self.conn,
            i64::from(log_alpha),
            i64::from(log_one_plus_alpha),
        )?;
        if changed_rows != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed_rows));
        }
        Ok(())
    }

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
        // Keep the invariant check inside the write transaction so validation and writes
        // observe the same database snapshot.
        assert_write_head_matches_open_state(&tx, head)?;

        sql_insert_user_ops_batch(
            &tx,
            u64_to_i64(head.batch_index),
            i64::from(head.frame_in_batch),
            head.open_frame_user_op_count,
            user_ops,
        )?;

        tx.commit()?;
        head.increment_batch_user_op_count(user_ops.len());
        Ok(())
    }

    pub fn close_frame_only(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        leading_direct_range: SafeInputRange,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_write_head_matches_open_state(&tx, head)?;
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

    pub fn close_frame_and_batch(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_write_head_matches_open_state(&tx, head)?;
        let now_ms = now_unix_ms();
        // Batch policy is sampled here: the derived fee is committed to the newly
        // opened frame, and the batch size target is stored on the write head.
        let policy = query_batch_policy(&tx)?;
        let next_batch_index = insert_open_batch(&tx, now_ms)?;
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

    /// Unbounded load of all valid sequenced L2 txs from `offset`. **O(N) time and memory.**
    /// Test/debug only — production code uses `load_ordered_l2_txs_page_from` instead.
    pub fn load_ordered_l2_txs_from(&mut self, offset: u64) -> Result<Vec<SequencedL2Tx>> {
        let rows = sql_select_ordered_l2_txs_from_offset(&self.conn, u64_to_i64(offset))?;
        Ok(decode_ordered_l2_txs(rows))
    }

    /// Load a page of ordered L2 transactions starting after the given offset.
    /// Returns `(db_offset, tx)` pairs. Callers should track `db_offset` of the last
    /// item as their cursor, not increment a counter.
    pub fn load_ordered_l2_txs_page_from(
        &mut self,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<(u64, SequencedL2Tx)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let rows = sql_select_ordered_l2_txs_page_from_offset(
            &self.conn,
            u64_to_i64(offset),
            usize_to_i64(limit),
        )?;
        Ok(decode_ordered_l2_txs_with_offset(rows))
    }

    /// Unbounded COUNT of all valid sequenced L2 txs. **O(N) full-table scan.**
    /// Test/debug only — production code uses cursor-based pagination instead.
    pub fn ordered_l2_tx_count(&mut self) -> Result<u64> {
        let value = sql_select_ordered_l2_tx_count(&self.conn)?;
        Ok(i64_to_u64(value))
    }

    /// Returns the maximum offset in `sequenced_l2_txs` (valid rows only), or 0 if empty.
    /// Used as the head cursor for feed subscribers — accounts for offset holes from invalid batches.
    pub fn ordered_l2_tx_head_offset(&mut self) -> Result<u64> {
        const SQL: &str = "SELECT MAX(s.offset) FROM sequenced_l2_txs s \
                           WHERE s.batch_index NOT IN (SELECT batch_index FROM invalid_batches)";
        let value: Option<i64> = self.conn.query_row(SQL, [], |row| row.get(0))?;
        Ok(value.map(i64_to_u64).unwrap_or(0))
    }

    /// Count broadcastable events with offset > `from_offset`.
    ///
    /// Used for catch-up window checks. Excludes:
    /// - events from invalidated batches (offset holes)
    /// - batch-submitter direct inputs (filtered before WS delivery)
    ///
    /// This matches the filtering in `run_subscription` / `should_filter_from_broadcast`
    /// so the catch-up limit reflects what the client will actually receive.
    pub fn count_broadcastable_events_after(
        &mut self,
        from_offset: u64,
        limit: u64,
        batch_submitter_address: Option<Address>,
    ) -> Result<u64> {
        if limit == 0 {
            return Ok(0);
        }

        let value: i64 = match batch_submitter_address {
            Some(addr) => {
                const SQL: &str = "SELECT COUNT(*) FROM ( \
                                       SELECT 1 FROM sequenced_l2_txs s \
                                       WHERE s.offset > ?1 \
                                         AND s.batch_index NOT IN (SELECT batch_index FROM invalid_batches) \
                                         AND NOT (s.safe_input_index IS NOT NULL \
                                             AND EXISTS (SELECT 1 FROM safe_inputs si \
                                                 WHERE si.safe_input_index = s.safe_input_index \
                                                   AND si.sender = ?2)) \
                                       LIMIT ?3 \
                                   )";
                self.conn.query_row(
                    SQL,
                    rusqlite::params![u64_to_i64(from_offset), addr.as_slice(), u64_to_i64(limit)],
                    |row| row.get(0),
                )?
            }
            None => {
                const SQL: &str = "SELECT COUNT(*) FROM ( \
                                       SELECT 1 FROM sequenced_l2_txs s \
                                       WHERE s.offset > ?1 \
                                         AND s.batch_index NOT IN (SELECT batch_index FROM invalid_batches) \
                                       LIMIT ?2 \
                                   )";
                self.conn.query_row(
                    SQL,
                    rusqlite::params![u64_to_i64(from_offset), u64_to_i64(limit)],
                    |row| row.get(0),
                )?
            }
        };
        Ok(i64_to_u64(value))
    }

    pub fn latest_batch_index(&mut self) -> Result<Option<u64>> {
        let value = sql_select_latest_batch_index(&self.conn)?;
        Ok(value.map(i64_to_u64))
    }

    pub fn load_frames_for_batch(&mut self, batch_index: u64) -> Result<Vec<FrameHeader>> {
        let rows = sql_select_frames_for_batch(&self.conn, u64_to_i64(batch_index))?;
        Ok(rows
            .into_iter()
            .map(|row| FrameHeader {
                frame_in_batch: i64_to_u32(row.frame_in_batch),
                fee: i64_to_u16(row.fee),
                safe_block: i64_to_u64(row.safe_block),
            })
            .collect())
    }

    pub fn load_ordered_l2_txs_for_batch(
        &mut self,
        batch_index: u64,
    ) -> Result<Vec<SequencedL2Tx>> {
        let rows = sql_select_ordered_l2_txs_for_batch(&self.conn, u64_to_i64(batch_index))?;
        Ok(decode_ordered_l2_txs(rows))
    }

    pub fn load_batch_for_submission(&mut self, batch_index: u64) -> Result<BatchForSubmission> {
        let created_at_ms: i64 = self.conn.query_row(
            "SELECT created_at_ms FROM batches WHERE batch_index = ?1 LIMIT 1",
            [u64_to_i64(batch_index)],
            |row| row.get(0),
        )?;

        let frame_headers = self.load_frames_for_batch(batch_index)?;
        let mut frames = Vec::with_capacity(frame_headers.len());

        for header in frame_headers {
            let rows = sql_select_user_ops_for_frame(
                &self.conn,
                u64_to_i64(batch_index),
                i64::from(header.frame_in_batch),
            )?;

            let user_ops = rows
                .into_iter()
                .map(|row| WireUserOp {
                    nonce: i64_to_u32(row.nonce),
                    max_fee: i64_to_u16(row.max_fee),
                    data: row.data,
                    signature: row.sig,
                })
                .collect();

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

    pub fn insert_invalid_batch(&mut self, batch_index: u64) -> Result<()> {
        sql_insert_invalid_batch(&self.conn, u64_to_i64(batch_index))?;
        Ok(())
    }

    /// Find the first stale batch using the accepted frontier.
    ///
    /// The accepted frontier tells us how many batches the scheduler has accepted.
    /// The local batch at that nonce (the first unaccepted one) is checked for staleness.
    /// Returns the batch_index if it exists and is stale.
    pub fn find_stale_batch(&mut self, max_wait_blocks: u64) -> Result<Option<u64>> {
        find_stale_batch_from_frontier(&self.conn, max_wait_blocks)
    }

    /// Check if the first unresolved batch (past the accepted frontier) is in the
    /// danger zone (approaching staleness).
    ///
    /// Returns the batch_index of the frontier batch if its age
    /// (`current_safe_block - first_frame_safe_block`) meets or exceeds `danger_threshold`.
    ///
    /// Requires `safe_accepted_batches` and `batch_nonces` to be populated first
    /// (call `populate_safe_accepted_batches` + `assign_batch_nonces` before this).
    pub fn check_danger_zone(&mut self, danger_threshold: u64) -> Result<Option<u64>> {
        check_danger_zone_inner(&self.conn, danger_threshold)
    }

    /// Return the wall-clock timestamp (Unix ms) of the last successful L1 sync.
    /// Returns 0 if no sync has occurred.
    pub fn last_l1_sync_ms(&self) -> Result<u64> {
        Ok(i64_to_u64(sql_select_l1_sync_timestamp(&self.conn)?))
    }

    /// Read cached L1 bootstrap data. Returns None on first startup.
    pub fn load_l1_bootstrap_cache(&self) -> Result<Option<(alloy_primitives::Address, u64, u64)>> {
        let row = sql_select_l1_bootstrap_cache(&self.conn)?;
        Ok(row.map(|(addr_bytes, genesis, chain_id)| {
            let addr = alloy_primitives::Address::from_slice(&addr_bytes);
            (addr, i64_to_u64(genesis), i64_to_u64(chain_id))
        }))
    }

    /// Cache L1 bootstrap data for future startups when L1 might be unreachable.
    pub fn save_l1_bootstrap_cache(
        &mut self,
        input_box_address: alloy_primitives::Address,
        genesis_block: u64,
        chain_id: u64,
    ) -> Result<()> {
        sql_upsert_l1_bootstrap_cache(
            &self.conn,
            input_box_address.as_slice(),
            u64_to_i64(genesis_block),
            u64_to_i64(chain_id),
        )?;
        Ok(())
    }

    pub fn load_first_frame_safe_block(&mut self, batch_index: u64) -> Result<Option<u64>> {
        let value = sql_select_first_frame_safe_block(&self.conn, u64_to_i64(batch_index))?;
        Ok(value.map(i64_to_u64))
    }

    /// Populate the `safe_accepted_batches` table — the derived log of batch
    /// submissions the scheduler would actually execute.
    ///
    /// Simulates the scheduler's acceptance logic: scans safe_inputs from
    /// `batch_submitter_address` in order, maintaining `expected_nonce`.
    /// For each decoded batch:
    /// - if stale (`inclusion_block - first_frame_safe_block >= MAX_WAIT_BLOCKS`), skip
    /// - if `batch.nonce == expected_nonce`, append to table and increment nonce
    /// - otherwise skip (wrong nonce — duplicate, out-of-order, etc.)
    ///
    /// Only processes safe_inputs not yet in `safe_accepted_batches`. The function
    /// resumes from the latest accepted row in `safe_accepted_batches`.
    pub fn populate_safe_accepted_batches(
        &mut self,
        batch_submitter_address: Address,
        max_wait_blocks: u64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        populate_safe_accepted_batches_inner(&tx, batch_submitter_address, max_wait_blocks)?;
        tx.commit()?;
        Ok(())
    }

    /// Detect stale batches and cascade-invalidate, then restore the open-batch invariant.
    ///
    /// Runs detection, cascade invalidation, and recovery-batch opening inside a single
    /// `Immediate` transaction so the operation is crash-safe and atomic.
    ///
    /// Also handles the edge case where a previous boot invalidated the suffix but crashed
    /// before opening the fresh batch: if no new invalidations are found but no valid open
    /// batch exists, a recovery batch is opened.
    ///
    /// Returns the list of newly invalidated batch indices (empty if no stale batches found).
    pub fn detect_and_recover(&mut self, max_wait_blocks: u64) -> Result<Vec<u64>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let to_invalidate = detect_and_recover_inner(&tx, max_wait_blocks)?;
        tx.commit()?;
        Ok(to_invalidate)
    }

    /// Assign nonces to all valid batches that don't yet have a nonce in `batch_nonces`.
    /// Nonces are derived from the latest valid assigned batch in batch order.
    ///
    /// Returns the number of newly assigned nonces.
    pub fn assign_batch_nonces(&mut self) -> Result<u64> {
        assign_batch_nonces_inner(&self.conn)
    }

    /// Run the full startup recovery procedure in a single atomic transaction:
    /// 1. Populate safe_accepted_batches (frontier)
    /// 2. Assign nonces to un-nonced valid batches
    /// 3. Detect stale batches, cascade-invalidate, and open recovery batch
    ///
    /// Returns the list of newly invalidated batch indices (empty if no stale batches found).
    pub fn run_startup_recovery(
        &mut self,
        batch_submitter_address: Address,
        max_wait_blocks: u64,
    ) -> Result<Vec<u64>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        populate_safe_accepted_batches_inner(&tx, batch_submitter_address, max_wait_blocks)?;
        assign_batch_nonces_inner(&tx)?;
        let invalidated = detect_and_recover_inner(&tx, max_wait_blocks)?;
        tx.commit()?;
        Ok(invalidated)
    }

    /// Load the next valid closed batch that needs to be submitted.
    pub fn load_next_batch_to_submit(&mut self, min_nonce: u64) -> Result<Option<PendingBatch>> {
        const SQL: &str = "SELECT bn.batch_index, bn.nonce FROM batch_nonces bn \
                           WHERE bn.nonce >= ?1 \
                             AND bn.batch_index NOT IN (SELECT batch_index FROM invalid_batches) \
                           ORDER BY bn.nonce ASC LIMIT 1";
        let batch_ref: Option<(i64, i64)> = self
            .conn
            .query_row(SQL, rusqlite::params![u64_to_i64(min_nonce)], |row| {
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
    /// Uses a single DB connection for all batches — avoids per-batch connection open/close.
    pub fn load_pending_batches(&mut self, min_nonce: u64) -> Result<Vec<PendingBatch>> {
        let mut batches = Vec::new();
        let mut next = min_nonce;
        while let Some(batch) = self.load_next_batch_to_submit(next)? {
            next = batch.nonce.saturating_add(1);
            batches.push(batch);
        }
        Ok(batches)
    }
}

// ---------------------------------------------------------------------------
// Recovery internals
//
// These free functions implement the recovery subsystem. They operate on bare
// `&Connection` / `&Transaction` so they can be composed into a single atomic
// transaction (see `run_startup_recovery`).
//
// ## Key invariants
//
// 1. **Cascade**: if batch B is stale, ALL batches with batch_index >= B are
//    invalid. The suffix is invalidated atomically.
//
// 2. **Open-batch**: after `detect_and_recover`, a valid (non-invalidated) open
//    batch always exists. If the previous open batch was invalidated, a fresh
//    recovery batch is opened.
//
// 3. **Nonce-space**: nonces are contiguous over valid batches. Invalid batches
//    do not consume nonces — new batches reuse them.
//
// 4. **Re-drain**: direct inputs from invalidated batches are re-drained into
//    the recovery batch's first frame. The UNIQUE constraint on
//    `sequenced_l2_txs(safe_input_index)` was removed to allow this.
//
// 5. **Filtering**: all read queries over batch data exclude `invalid_batches`.
//
// ## Fault model
//
// The recovery logic is robust to submission/outage failures (crashes, network
// errors, mempool drops, extended downtime). It is not designed to harden itself
// against arbitrarily malformed self-submissions: `populate_safe_accepted_batches`
// trusts that on-chain batches from the sequencer's own address are structurally
// valid. This is a deliberate system assumption — the sequencer controls its own
// submissions.
// ---------------------------------------------------------------------------

/// Check if the first unresolved batch (past the accepted frontier) has age >= danger_threshold.
///
/// Uses the same frontier-based approach as [`find_stale_batch_from_frontier`]:
/// computes the accepted frontier from `safe_accepted_batches`, finds the local
/// batch at that nonce, and checks its age against `danger_threshold`.
///
/// Requires `safe_accepted_batches` and `batch_nonces` to be populated first
/// (same precondition as `find_stale_batch_from_frontier`).
fn check_danger_zone_inner(conn: &Connection, danger_threshold: u64) -> Result<Option<u64>> {
    find_frontier_batch_exceeding_threshold(conn, danger_threshold)
}

/// A batch is stale when `reference_block - first_frame_safe_block >= max_wait_blocks`.
///
/// Used in two contexts:
/// - **Inclusion staleness**: `reference_block` is the L1 block the batch was included in.
///   The scheduler uses this to skip stale submissions.
/// - **Current staleness**: `reference_block` is the current safe block. The sequencer
///   uses this to detect batches that will be stale by the time the scheduler sees them.
fn batch_age_is_stale(
    reference_block: u64,
    first_frame_safe_block: u64,
    max_wait_blocks: u64,
) -> bool {
    reference_block.saturating_sub(first_frame_safe_block) >= max_wait_blocks
}

#[derive(Debug, Clone, Copy)]
struct SafeAcceptedBatchRow {
    safe_input_index: i64,
    nonce: i64,
}

fn query_latest_safe_accepted_batch(conn: &Connection) -> Result<Option<SafeAcceptedBatchRow>> {
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

/// Populate `safe_accepted_batches` — the derived log of batch submissions the
/// scheduler would actually execute. Simulates the scheduler's acceptance logic
/// over safe_inputs from `batch_submitter_address`.
///
/// See `Storage::populate_safe_accepted_batches` for full doc.
fn populate_safe_accepted_batches_inner(
    conn: &Connection,
    batch_submitter_address: Address,
    max_wait_blocks: u64,
) -> Result<()> {
    const PAGE_SIZE: i64 = 256;

    let latest_accepted = query_latest_safe_accepted_batch(conn)?;
    let mut cursor = latest_accepted
        .map(|row| row.safe_input_index)
        .unwrap_or(-1);
    let mut expected = latest_accepted
        .map(|row| i64_to_u64(row.nonce).saturating_add(1))
        .unwrap_or(0);

    // Scan new safe_inputs from batch_submitter in order, paginated.
    const SQL: &str = "SELECT si.safe_input_index, si.payload, si.block_number \
                       FROM safe_inputs si \
                       WHERE si.sender = ?1 \
                         AND si.safe_input_index > ?2 \
                       ORDER BY si.safe_input_index ASC LIMIT ?3";
    loop {
        let mut stmt = conn.prepare_cached(SQL)?;
        let mut rows = stmt.query(rusqlite::params![
            batch_submitter_address.as_slice(),
            cursor,
            PAGE_SIZE,
        ])?;
        let mut page_count: i64 = 0;
        let mut to_insert = Vec::new();
        while let Some(row) = rows.next()? {
            page_count += 1;
            let safe_input_index: i64 = row.get(0)?;
            cursor = safe_input_index;
            let payload: Vec<u8> = row.get(1)?;
            let block_number: i64 = row.get(2)?;
            let Ok(batch) = <sequencer_core::batch::Batch as ssz::Decode>::from_ssz_bytes(&payload)
            else {
                continue;
            };

            // Skip stale batches — the scheduler skips them too.
            let first_frame_safe_block = batch.frames.first().map(|f| f.safe_block).unwrap_or(0);
            let inclusion_block = i64_to_u64(block_number);
            if !batch.frames.is_empty()
                && batch_age_is_stale(inclusion_block, first_frame_safe_block, max_wait_blocks)
            {
                continue;
            }

            // Only accept if nonce matches the expected sequence.
            if batch.nonce == expected {
                to_insert.push((
                    safe_input_index,
                    i64::try_from(batch.nonce).unwrap_or(i64::MAX),
                    i64::try_from(first_frame_safe_block).unwrap_or(i64::MAX),
                    block_number,
                ));
                expected = expected.saturating_add(1);
            }
        }
        drop(rows);
        drop(stmt);
        for (si_idx, nonce, first_frame_sb, inc_block) in to_insert {
            sql_insert_safe_accepted_batch(conn, si_idx, nonce, first_frame_sb, inc_block)?;
        }
        if page_count < PAGE_SIZE {
            break;
        }
    }

    Ok(())
}

/// Assign nonces to all valid batches that don't yet have a nonce in `batch_nonces`.
/// See `Storage::assign_batch_nonces` for full doc.
fn assign_batch_nonces_inner(conn: &Connection) -> Result<u64> {
    const SQL_LATEST_VALID_NONCE: &str = "SELECT bn.nonce FROM batch_nonces bn \
                                          WHERE bn.batch_index NOT IN (SELECT batch_index FROM invalid_batches) \
                                          ORDER BY bn.batch_index DESC LIMIT 1";
    let latest_valid_nonce: Option<i64> = conn
        .query_row(SQL_LATEST_VALID_NONCE, [], |row| row.get(0))
        .optional()?;
    let mut next_nonce = latest_valid_nonce
        .map(|nonce| i64_to_u64(nonce).saturating_add(1))
        .unwrap_or(0);

    let open_batch_index: Option<i64> =
        conn.query_row("SELECT MAX(batch_index) FROM batches", [], |row| row.get(0))?;
    let Some(open_batch_index) = open_batch_index else {
        return Ok(0);
    };

    const SQL_UNNONCED: &str = "SELECT batch_index FROM batches \
                                WHERE batch_index NOT IN (SELECT batch_index FROM invalid_batches) \
                                  AND batch_index NOT IN (SELECT batch_index FROM batch_nonces) \
                                  AND batch_index < ?1 \
                                ORDER BY batch_index ASC";
    let mut stmt = conn.prepare(SQL_UNNONCED)?;
    let mut rows = stmt.query(rusqlite::params![open_batch_index])?;
    let mut to_assign = Vec::new();
    while let Some(row) = rows.next()? {
        let bi: i64 = row.get(0)?;
        to_assign.push(i64_to_u64(bi));
    }
    drop(rows);
    drop(stmt);

    let count = to_assign.len() as u64;
    for bi in to_assign {
        sql_insert_batch_nonce(conn, u64_to_i64(bi), u64_to_i64(next_nonce))?;
        next_nonce = next_nonce.saturating_add(1);
    }

    Ok(count)
}

/// Detect stale batches, cascade-invalidate, and restore the open-batch invariant.
/// See `Storage::detect_and_recover` for full doc.
fn detect_and_recover_inner(tx: &Transaction<'_>, max_wait_blocks: u64) -> Result<Vec<u64>> {
    let to_invalidate = detect_stale_and_collect_cascade(tx, max_wait_blocks)?;

    for &bi in &to_invalidate {
        sql_insert_invalid_batch(tx, u64_to_i64(bi))?;
    }

    let needs_recovery_batch = if !to_invalidate.is_empty() {
        true
    } else {
        !has_valid_open_batch(tx)?
    };

    if needs_recovery_batch {
        open_recovery_batch_in_tx(tx)?;
    }

    Ok(to_invalidate)
}

/// Find the first stale batch using the accepted frontier.
///
/// Delegates to [`find_frontier_batch_exceeding_threshold`] with `max_wait_blocks`.
fn find_stale_batch_from_frontier(conn: &Connection, max_wait_blocks: u64) -> Result<Option<u64>> {
    find_frontier_batch_exceeding_threshold(conn, max_wait_blocks)
}

/// Find the first unresolved batch past the accepted frontier whose age exceeds `threshold`.
///
/// The accepted frontier (latest accepted nonce + 1 from `safe_accepted_batches`) tells us
/// how many batches the scheduler has accepted. The local batch with that nonce is the first
/// unaccepted one. If it exists and its `first_frame_safe_block` is old enough
/// (`current_safe_block - first_frame_safe_block >= threshold`), it's returned.
///
/// Used with `threshold = max_wait_blocks` for staleness detection, and with
/// `threshold = danger_threshold` for preemptive danger-zone detection.
///
/// Requires `safe_accepted_batches` and `batch_nonces` to be populated.
fn find_frontier_batch_exceeding_threshold(
    conn: &Connection,
    threshold: u64,
) -> Result<Option<u64>> {
    // Step 1: compute the accepted frontier — the next nonce the scheduler expects.
    let frontier_nonce = query_latest_safe_accepted_batch(conn)?
        .map(|row| i64_to_u64(row.nonce).saturating_add(1))
        .unwrap_or(0);

    // Step 2: find the valid local batch with that nonce (the first unaccepted batch).
    let batch_ref: Option<(i64, i64)> = conn
        .query_row(
            "SELECT batch_index, nonce FROM batch_nonces \
             WHERE nonce >= ?1 \
               AND batch_index NOT IN (SELECT batch_index FROM invalid_batches) \
             ORDER BY nonce ASC LIMIT 1",
            rusqlite::params![u64_to_i64(frontier_nonce)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((batch_index, batch_nonce)) = batch_ref else {
        return Ok(None); // No local batch at this nonce yet
    };
    if i64_to_u64(batch_nonce) != frontier_nonce {
        return Ok(None);
    }

    // Step 3: check if this batch exceeds the threshold.
    let first_frame_safe_block =
        i64_to_u64(sql_select_first_frame_safe_block(conn, batch_index)?.unwrap_or(0));
    let safe_block = query_current_safe_block(conn)?;
    if batch_age_is_stale(safe_block, first_frame_safe_block, threshold) {
        Ok(Some(i64_to_u64(batch_index)))
    } else {
        Ok(None)
    }
}

/// Detect the first stale batch using the accepted frontier and collect the cascade suffix.
fn detect_stale_and_collect_cascade(tx: &Connection, max_wait_blocks: u64) -> Result<Vec<u64>> {
    let stale_batch_index = find_stale_batch_from_frontier(tx, max_wait_blocks)?;
    let stale_batch_index = stale_batch_index.map(u64_to_i64);

    let Some(stale_batch_index) = stale_batch_index else {
        return Ok(Vec::new());
    };

    // Cascade: collect ALL batches with batch_index >= stale_batch_index.
    const SQL_CASCADE: &str = "SELECT batch_index FROM batches \
                               WHERE batch_index >= ?1 \
                                 AND batch_index NOT IN (SELECT batch_index FROM invalid_batches) \
                               ORDER BY batch_index ASC";
    let mut stmt = tx.prepare(SQL_CASCADE)?;
    let mut rows = stmt.query(rusqlite::params![stale_batch_index])?;
    let mut to_invalidate = Vec::new();
    while let Some(row) = rows.next()? {
        let bi: i64 = row.get(0)?;
        to_invalidate.push(i64_to_u64(bi));
    }
    Ok(to_invalidate)
}

/// Check whether the DB has a valid (non-invalidated) open batch.
///
/// The open batch is always the absolute latest batch (MAX batch_index).
/// If the latest batch is in `invalid_batches`, there is no valid open batch.
fn has_valid_open_batch(tx: &Connection) -> Result<bool> {
    let max_bi: Option<i64> =
        tx.query_row("SELECT MAX(batch_index) FROM batches", [], |row| row.get(0))?;
    let Some(max_bi) = max_bi else {
        return Ok(false);
    };
    let is_invalid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM invalid_batches WHERE batch_index = ?1)",
        rusqlite::params![max_bi],
        |row| row.get(0),
    )?;
    Ok(!is_invalid)
}

/// Open a fresh recovery batch inside an existing transaction.
fn open_recovery_batch_in_tx(tx: &Transaction<'_>) -> Result<()> {
    let now_ms = now_unix_ms();
    let safe_block = query_current_safe_block(tx).unwrap_or(0);

    // Next batch_index: absolute max + 1
    let max_bi: Option<i64> =
        tx.query_row("SELECT MAX(batch_index) FROM batches", [], |row| row.get(0))?;
    let next_bi = i64_to_u64(max_bi.map(|b| b.saturating_add(1)).unwrap_or(0));

    let policy = query_batch_policy(tx)?;

    insert_open_batch_with_index(tx, next_bi, now_ms)?;
    insert_open_frame(tx, next_bi, 0, now_ms, policy.recommended_fee, safe_block)?;

    // Drain leading directs into the new batch's first frame.
    // Direct inputs from invalidated batches are re-drained into the recovery batch
    // (the UNIQUE(safe_input_index) constraint was removed to allow this).
    let next_undrained = i64_to_u64(sql_select_total_drained_direct_inputs(tx)?);
    let safe_input_end = query_latest_safe_input_index_exclusive(tx)?;
    let leading_range = super::SafeInputRange {
        start_inclusive: next_undrained,
        end_exclusive: safe_input_end,
    };
    persist_frame_direct_sequence(tx, next_bi, 0, leading_range)?;
    Ok(())
}

/// Decode a single ordered-L2Tx row into a `SequencedL2Tx`.
fn decode_l2_tx_row(
    kind: i64,
    sender: Option<Vec<u8>>,
    data: Option<Vec<u8>>,
    fee: Option<i64>,
    payload: Option<Vec<u8>>,
    block_number: Option<i64>,
) -> SequencedL2Tx {
    let sender_bytes = sender.expect("ordered replay row: missing sender");
    assert_eq!(
        sender_bytes.len(),
        20,
        "ordered replay row: sender must be 20 bytes"
    );
    if kind == 0 {
        SequencedL2Tx::UserOp(ValidUserOp {
            sender: Address::from_slice(sender_bytes.as_slice()),
            // Replay uses the persisted frame fee (log-space exponent) to mirror canonical execution.
            fee: i64_to_u16(fee.expect("ordered replay row: missing fee")),
            data: data.expect("ordered replay row: missing data"),
        })
    } else {
        SequencedL2Tx::Direct(DirectInput {
            sender: Address::from_slice(sender_bytes.as_slice()),
            block_number: i64_to_u64(
                block_number.expect("ordered replay row: missing block_number"),
            ),
            payload: payload.expect("ordered replay row: missing payload"),
        })
    }
}

fn decode_ordered_l2_txs(rows: Vec<super::sql::OrderedL2TxRow>) -> Vec<SequencedL2Tx> {
    rows.into_iter()
        .map(|r| decode_l2_tx_row(r.kind, r.sender, r.data, r.fee, r.payload, r.block_number))
        .collect()
}

fn decode_ordered_l2_txs_with_offset(
    rows: Vec<super::sql::OrderedL2TxRowWithOffset>,
) -> Vec<(u64, SequencedL2Tx)> {
    rows.into_iter()
        .map(|r| {
            let tx = decode_l2_tx_row(r.kind, r.sender, r.data, r.fee, r.payload, r.block_number);
            (i64_to_u64(r.offset), tx)
        })
        .collect()
}

fn load_current_write_head(tx: &Transaction<'_>) -> Result<Option<WriteHead>> {
    let Some((batch_index, batch_created_at, batch_user_op_count)) = query_latest_batch(tx)? else {
        return Ok(None);
    };
    let (frame_in_batch, frame_fee, safe_block) = query_latest_frame_in_batch(tx, batch_index)?;
    let open_frame_user_op_count = query_frame_user_op_count(tx, batch_index, frame_in_batch)?;
    let policy = query_batch_policy(tx)?;
    Ok(Some(WriteHead {
        batch_index,
        batch_created_at,
        frame_fee,
        safe_block,
        batch_user_op_count,
        open_frame_user_op_count,
        frame_in_batch,
        max_batch_user_op_bytes: super::batch_size_target_bytes(policy),
    }))
}

fn assert_write_head_matches_open_state(tx: &Transaction<'_>, expected: &WriteHead) -> Result<()> {
    let actual = load_current_write_head(tx)?.expect("stale WriteHead: storage has no open state");
    assert_eq!(
        expected.batch_index, actual.batch_index,
        "stale WriteHead: batch_index mismatch"
    );
    assert_eq!(
        expected.frame_in_batch, actual.frame_in_batch,
        "stale WriteHead: frame_in_batch mismatch"
    );
    assert_eq!(
        expected.batch_user_op_count, actual.batch_user_op_count,
        "stale WriteHead: batch_user_op_count mismatch"
    );
    assert_eq!(
        expected.open_frame_user_op_count, actual.open_frame_user_op_count,
        "stale WriteHead: open_frame_user_op_count mismatch"
    );
    assert_eq!(
        expected.frame_fee, actual.frame_fee,
        "stale WriteHead: frame_fee mismatch"
    );
    assert_eq!(
        expected.safe_block, actual.safe_block,
        "stale WriteHead: safe_block mismatch"
    );
    assert_eq!(
        to_unix_ms(expected.batch_created_at),
        to_unix_ms(actual.batch_created_at),
        "stale WriteHead: batch_created_at mismatch"
    );
    Ok(())
}

fn query_latest_batch(tx: &Transaction<'_>) -> Result<Option<(u64, SystemTime, u64)>> {
    match sql_select_latest_batch_with_user_op_count(tx) {
        Ok((batch_index, batch_created_at_ms, batch_user_op_count)) => Ok(Some((
            i64_to_u64(batch_index),
            from_unix_ms(batch_created_at_ms),
            i64_to_u64(batch_user_op_count),
        ))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(source) => Err(source),
    }
}

fn query_latest_frame_in_batch(tx: &Transaction<'_>, batch_index: u64) -> Result<(u32, u16, u64)> {
    let (frame_in_batch, frame_fee, safe_block) =
        sql_select_latest_frame_in_batch_for_batch(tx, u64_to_i64(batch_index))?;
    Ok((
        i64_to_u32(frame_in_batch),
        i64_to_u16(frame_fee),
        i64_to_u64(safe_block),
    ))
}

fn query_frame_user_op_count(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
) -> Result<u32> {
    let value =
        sql_count_user_ops_for_frame(tx, u64_to_i64(batch_index), i64::from(frame_in_batch))?;
    Ok(i64_to_u32(value))
}

fn query_latest_safe_input_index_exclusive(tx: &Connection) -> Result<u64> {
    let value = sql_select_max_safe_input_index(tx)?;
    Ok(match value {
        Some(last_index) => i64_to_u64(last_index).saturating_add(1),
        None => 0,
    })
}

fn query_current_safe_block(tx: &Connection) -> Result<u64> {
    let value = sql_select_safe_block(tx)?;
    Ok(i64_to_u64(value))
}

fn query_batch_policy(tx: &Transaction<'_>) -> Result<BatchPolicy> {
    let (log_recommended_fee, log_batch_size_target) = sql_select_batch_policy(tx)?;
    let max_exp = sequencer_core::fee::MAX_EXPONENT;
    Ok(BatchPolicy {
        // Clamp to MAX_EXPONENT to prevent panics in fee_to_linear.
        recommended_fee: i64_to_u16(log_recommended_fee).min(max_exp),
        batch_size_target: i64_to_u16(log_batch_size_target).min(max_exp),
    })
}

fn persist_frame_direct_sequence(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    drained_direct_range: SafeInputRange,
) -> Result<()> {
    sql_insert_sequenced_direct_inputs(
        tx,
        u64_to_i64(batch_index),
        i64::from(frame_in_batch),
        drained_direct_range,
    )
}

fn insert_open_batch(tx: &Transaction<'_>, created_at_ms: i64) -> Result<u64> {
    sql_insert_open_batch(tx, created_at_ms)?;
    Ok(i64_to_u64(tx.last_insert_rowid()))
}

fn insert_open_batch_with_index(
    tx: &Transaction<'_>,
    batch_index: u64,
    created_at_ms: i64,
) -> Result<()> {
    sql_insert_open_batch_with_index(tx, u64_to_i64(batch_index), created_at_ms)?;
    Ok(())
}

fn insert_open_frame(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    created_at_ms: i64,
    frame_fee: u16,
    safe_block: u64,
) -> Result<()> {
    sql_insert_open_frame(
        tx,
        u64_to_i64(batch_index),
        i64::from(frame_in_batch),
        created_at_ms,
        i64::from(frame_fee),
        u64_to_i64(safe_block),
    )?;
    Ok(())
}

fn to_unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn from_unix_ms(ms: i64) -> SystemTime {
    let clamped_ms = ms.max(0) as u64;
    UNIX_EPOCH + Duration::from_millis(clamped_ms)
}

fn now_unix_ms() -> i64 {
    to_unix_ms(SystemTime::now())
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn i64_to_u64(value: i64) -> u64 {
    value.max(0) as u64
}

fn i64_to_u16(value: i64) -> u16 {
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

fn i64_to_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use super::Storage;
    use crate::storage::{SafeInputRange, StoredSafeInput};
    use sequencer_core::l2_tx::SequencedL2Tx;
    use tempfile::TempDir;

    struct TestDb {
        _dir: TempDir,
        path: String,
    }

    fn temp_db(name: &str) -> TestDb {
        let dir = tempfile::Builder::new()
            .prefix(format!("sequencer-{name}-").as_str())
            .tempdir()
            .expect("create temporary test directory");
        let path = dir.path().join("sequencer.sqlite");
        TestDb {
            _dir: dir,
            path: path.to_string_lossy().into_owned(),
        }
    }

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
    fn high_gas_price_clamps_recommended_fee_to_max_exponent() {
        let db = temp_db("clamp-fee");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        // Set gas price high enough that log_recommended_fee > MAX_EXPONENT (17101).
        // Default: log_recommended_fee = gas_price + 20 + 419 + 621.
        // With gas_price = 17000: 17000 + 1060 = 18060 > 17101.
        storage
            .set_log_gas_price(17000)
            .expect("set high gas price");

        let policy = storage.batch_policy().expect("read policy");
        assert_eq!(
            policy.recommended_fee,
            sequencer_core::fee::MAX_EXPONENT,
            "recommended_fee should be clamped to MAX_EXPONENT"
        );

        // fee_to_linear must not panic with the clamped value.
        let _ = sequencer_core::fee::fee_to_linear(policy.recommended_fee);
    }

    #[test]
    #[should_panic(expected = "num + denom overflows u64")]
    fn set_alpha_rejects_overflow() {
        let db = temp_db("alpha-overflow");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        storage.set_alpha(u64::MAX, 1).unwrap();
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
            .append_safe_inputs(10, drained.as_slice())
            .expect("insert direct inputs");
        let mut head = head;
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, drained.len() as u64))
            .expect("close frame with directs");

        let replay = storage.load_ordered_l2_txs_from(0).expect("load replay");
        assert_eq!(replay.len(), 2);
        match &replay[0] {
            SequencedL2Tx::Direct(value) => assert_eq!(value.payload.as_slice(), &[0xaa]),
            _ => panic!("expected direct input at position 0"),
        }
        match &replay[1] {
            SequencedL2Tx::Direct(value) => assert_eq!(value.payload.as_slice(), &[0xbb]),
            _ => panic!("expected direct input at position 1"),
        }
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
            .append_safe_inputs(10, drained.as_slice())
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
    fn safe_input_api_uses_half_open_intervals() {
        let db = temp_db("safe-input-api");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        assert_eq!(storage.safe_input_end_exclusive().expect("safe head"), 0);
        let mut out = Vec::new();
        storage
            .fill_safe_inputs(0, 0, &mut out)
            .expect("query empty interval");
        assert!(out.is_empty());

        let inserted = vec![
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xa0],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xb1],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, inserted.as_slice())
            .expect("insert safe directs");

        assert_eq!(storage.safe_input_end_exclusive().expect("safe head"), 2);

        storage
            .fill_safe_inputs(0, 2, &mut out)
            .expect("query full interval");
        assert_eq!(out, inserted);

        out.clear();
        storage
            .fill_safe_inputs(1, 1, &mut out)
            .expect("query empty half-open interval");
        assert!(out.is_empty());
    }

    #[test]
    fn ensure_minimum_safe_block_only_moves_forward() {
        let db = temp_db("ensure-min-safe-block");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        storage
            .ensure_minimum_safe_block(7)
            .expect("advance bootstrap safe head");
        assert_eq!(storage.current_safe_block().expect("read advanced"), 7);

        storage
            .ensure_minimum_safe_block(3)
            .expect("do not regress bootstrap safe head");
        assert_eq!(storage.current_safe_block().expect("read unchanged"), 7);
    }

    #[test]
    fn ensure_minimum_safe_block_does_not_record_l1_sync() {
        let db = temp_db("ensure-min-safe-block-no-sync");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        storage
            .ensure_minimum_safe_block(7)
            .expect("advance bootstrap safe head");
        assert_eq!(
            storage.last_l1_sync_ms().expect("read sync timestamp"),
            0,
            "bootstrap safe-head initialization must not count as a real L1 sync"
        );

        storage.touch_l1_sync().expect("record real L1 sync");
        let recorded_sync = storage.last_l1_sync_ms().expect("read sync timestamp");
        assert!(
            recorded_sync > 0,
            "touch_l1_sync should record wall-clock time"
        );

        storage
            .ensure_minimum_safe_block(9)
            .expect("advance bootstrap safe head again");
        assert_eq!(
            storage.last_l1_sync_ms().expect("read sync timestamp"),
            recorded_sync,
            "bootstrap safe-head updates must preserve the last real L1 sync timestamp"
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

    /// Helper: insert safe inputs whose payloads are SSZ-encoded batches with
    /// the given nonces, all attributed to `sender`.
    fn seed_safe_inputs_with_batch_nonces(
        storage: &mut Storage,
        sender: Address,
        safe_block: u64,
        nonces: &[u64],
    ) {
        let inputs: Vec<StoredSafeInput> = nonces
            .iter()
            .map(|nonce| StoredSafeInput {
                sender,
                payload: ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                    nonce: *nonce,
                    frames: Vec::new(),
                }),
                block_number: safe_block,
            })
            .collect();
        storage
            .append_safe_inputs(safe_block, inputs.as_slice())
            .expect("append safe inputs");
    }

    const SENDER_A: Address = Address::repeat_byte(0xAA);
    const SENDER_B: Address = Address::repeat_byte(0xBB);

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
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 10, &[0, 1, 3, 4, 5]);
        storage
            .populate_safe_accepted_batches(SENDER_A, u64::MAX)
            .expect("populate safe accepted batches");

        let (safe_block, next) = storage
            .load_safe_accepted_frontier()
            .expect("load safe accepted frontier");
        assert_eq!(safe_block, 10);
        assert_eq!(next, 2);
    }

    #[test]
    fn populate_safe_accepted_batches_resumes_from_latest_row() {
        let db = temp_db("safe-accepted-frontier-resume");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        seed_safe_inputs_with_batch_nonces(&mut storage, SENDER_A, 10, &[0, 1]);
        storage
            .populate_safe_accepted_batches(SENDER_A, u64::MAX)
            .expect("populate first page");

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
            .append_safe_inputs(11, second_wave.as_slice())
            .expect("append second wave");
        storage
            .populate_safe_accepted_batches(SENDER_A, u64::MAX)
            .expect("populate second wave");

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
            .append_safe_inputs(2000, inputs.as_slice())
            .expect("append");

        storage
            .populate_safe_accepted_batches(SENDER_A, 1200)
            .expect("populate safe accepted batches");

        // With max_wait_blocks=1200, the stale batch (nonce 1, safe_block 100, block 2000) is skipped.
        // So we see: nonce 0 (counted), stale nonce 1 (skipped), non-stale nonce 1 (counted).
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

        // Batch with safe_block=500 but inclusion block_number=100 (future safe_block).
        // The scheduler would reject this, but our frontier simulation accepts it.
        let future_safe_block_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                user_ops: Vec::new(),
                safe_block: 500,
                fee_price: 0,
            }],
        });
        // Batch with non-monotonic safe_blocks across frames.
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
                    safe_block: 100, // backwards
                    fee_price: 0,
                },
            ],
        });

        let batch_submitter = Address::repeat_byte(0xCC);
        let inputs = vec![
            StoredSafeInput {
                sender: batch_submitter,
                payload: future_safe_block_payload,
                block_number: 100, // safe_block 500 > inclusion 100
            },
            StoredSafeInput {
                sender: batch_submitter,
                payload: non_monotonic_payload,
                block_number: 200,
            },
        ];
        storage
            .append_safe_inputs(200, inputs.as_slice())
            .expect("append");

        // populate_safe_accepted_batches accepts both.
        storage
            .populate_safe_accepted_batches(batch_submitter, u64::MAX)
            .expect("populate");
        let (_, next) = storage
            .load_safe_accepted_frontier()
            .expect("load safe accepted frontier");
        assert_eq!(next, 2, "both batches should be in accepted frontier");
    }

    // -- invalid_batches tests --

    /// Helper: create N closed batches (batch indices 0..N-1) plus one open batch (index N).
    fn seed_closed_batches(storage: &mut Storage, count: u64) {
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        for _ in 0..count {
            let safe_block = head.safe_block;
            storage
                .close_frame_and_batch(&mut head, safe_block)
                .expect("close batch");
        }
    }

    #[test]
    fn invalid_batches_excluded_from_latest_batch_index() {
        let db = temp_db("invalid-latest-batch");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        // Batches 0,1,2 closed; 3 open.
        seed_closed_batches(&mut storage, 3);
        assert_eq!(
            storage.latest_batch_index().expect("latest").unwrap(),
            3,
            "open batch should be 3"
        );

        // Mark batch 3 (open) as invalid — latest_batch_index should return 2.
        storage.insert_invalid_batch(3).expect("mark invalid");
        assert_eq!(storage.latest_batch_index().expect("latest").unwrap(), 2,);

        // Mark batch 2 as invalid — latest should be 1.
        storage.insert_invalid_batch(2).expect("mark invalid");
        assert_eq!(storage.latest_batch_index().expect("latest").unwrap(), 1,);
    }

    #[test]
    fn invalid_batches_excluded_from_ordered_l2_txs() {
        let db = temp_db("invalid-ordered-txs");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        // Create two closed batches, each with one direct input.
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize");
        let directs_0 = vec![StoredSafeInput {
            sender: Address::ZERO,
            payload: vec![0xaa],
            block_number: 10,
        }];
        storage
            .append_safe_inputs(10, directs_0.as_slice())
            .expect("append");
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
            .expect("close frame");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        let directs_1 = vec![StoredSafeInput {
            sender: Address::ZERO,
            payload: vec![0xbb],
            block_number: 20,
        }];
        storage
            .append_safe_inputs(20, directs_1.as_slice())
            .expect("append");
        storage
            .close_frame_only(&mut head, 20, SafeInputRange::new(1, 2))
            .expect("close frame");

        // Both directs should be visible before invalidation.
        let all = storage.load_ordered_l2_txs_from(0).expect("load all");
        assert_eq!(all.len(), 2);

        // Invalidate batch 0.
        storage.insert_invalid_batch(0).expect("mark invalid");

        let filtered = storage.load_ordered_l2_txs_from(0).expect("load filtered");
        assert_eq!(filtered.len(), 1);
        match &filtered[0] {
            SequencedL2Tx::Direct(d) => assert_eq!(d.payload.as_slice(), &[0xbb]),
            _ => panic!("expected direct input"),
        }
    }

    #[test]
    fn invalid_batches_excluded_from_ordered_l2_txs_for_batch() {
        let db = temp_db("invalid-ordered-for-batch");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        // Create a closed batch with one direct input.
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize");
        let directs = vec![StoredSafeInput {
            sender: Address::ZERO,
            payload: vec![0xaa],
            block_number: 10,
        }];
        storage
            .append_safe_inputs(10, directs.as_slice())
            .expect("append");
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, 1))
            .expect("close frame");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        // Before invalidation: batch 0 has one tx.
        let txs = storage
            .load_ordered_l2_txs_for_batch(0)
            .expect("load batch 0");
        assert_eq!(txs.len(), 1);

        // After invalidation: batch 0 returns empty.
        storage.insert_invalid_batch(0).expect("mark invalid");
        let txs = storage
            .load_ordered_l2_txs_for_batch(0)
            .expect("load batch 0 after invalidation");
        assert!(txs.is_empty(), "invalid batch should return no txs");
    }

    #[test]
    fn invalid_batches_excluded_from_drained_direct_count() {
        let db = temp_db("invalid-drained-count");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize");
        let directs = vec![
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
            .append_safe_inputs(10, directs.as_slice())
            .expect("append");
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, 2))
            .expect("close frame");
        assert_eq!(
            storage
                .load_next_undrained_safe_input_index()
                .expect("cursor"),
            2
        );

        // Invalidate batch 0 — cursor should rewind to 0, allowing those direct
        // inputs to be re-drained into a recovery batch.
        storage.insert_invalid_batch(0).expect("mark invalid");
        assert_eq!(
            storage
                .load_next_undrained_safe_input_index()
                .expect("cursor after invalidation"),
            0
        );
    }

    #[test]
    fn load_next_batch_to_submit_returns_nonce_ordered_valid_suffix() {
        let db = temp_db("load-next-batch-to-submit");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        seed_closed_batches(&mut storage, 3);
        storage.assign_batch_nonces().expect("assign nonces");
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
    fn assign_batch_nonces_reuses_frontier_nonce_after_invalid_suffix() {
        let db = temp_db("assign-nonces-after-invalid-suffix");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        storage.assign_batch_nonces().expect("assign generation 1");

        storage.insert_invalid_batch(0).expect("invalidate batch 0");
        storage.insert_invalid_batch(1).expect("invalidate batch 1");
        storage
            .detect_and_recover(1200)
            .expect("open recovery batch after torn invalidation");

        let mut head = storage
            .load_open_state()
            .expect("load open state")
            .expect("recovery batch");
        assert_eq!(head.batch_index, 2);
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close recovery batch");

        let assigned = storage.assign_batch_nonces().expect("assign generation 2");
        assert_eq!(assigned, 1);

        let batch_two_nonce: i64 = storage
            .conn
            .query_row(
                "SELECT nonce FROM batch_nonces WHERE batch_index = 2",
                [],
                |row| row.get(0),
            )
            .expect("query reused nonce");
        assert_eq!(batch_two_nonce, 0);
    }

    #[test]
    fn detect_and_recover_cascades_from_stale() {
        let db = temp_db("detect-stale");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        // Create 3 closed batches with safe_block=10.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        for _ in 0..3 {
            storage
                .close_frame_and_batch(&mut head, 10)
                .expect("close batch");
        }

        // Assign nonces to batches.
        storage.assign_batch_nonces().expect("assign nonces");

        // Insert a stale safe_input, then populate safe_accepted_batches (which skips it).
        let batch_submitter = Address::repeat_byte(0xAA);
        let batch_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 10,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: batch_payload,
                    block_number: 1210,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

        // Detection should find nonce 0 is stale and cascade to all batches (0, 1, 2) + open batch (3).
        // Then atomically open a fresh recovery batch.
        let invalidated = storage
            .detect_and_recover(1200)
            .expect("detect and recover");
        assert_eq!(invalidated, vec![0, 1, 2, 3]);

        // A fresh recovery batch should now exist (batch_index 4).
        let head = storage.load_open_state().expect("load open state");
        assert!(head.is_some(), "recovery should have opened a fresh batch");
        assert_eq!(head.unwrap().batch_index, 4);
    }

    #[test]
    fn detect_and_recover_is_idempotent() {
        let db = temp_db("detect-idempotent");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch");

        // Assign nonces and simulate stale submission.
        storage.assign_batch_nonces().expect("assign nonces");
        let batch_submitter = Address::repeat_byte(0xAA);
        let batch_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 10,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: batch_payload,
                    block_number: 1210,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

        let first = storage.detect_and_recover(1200).expect("first detect");
        assert_eq!(first, vec![0, 1]); // batch 0 + open batch 1

        // Second run: already invalid, recovery batch already exists, nothing new.
        let second = storage.detect_and_recover(1200).expect("second detect");
        assert!(second.is_empty());
    }

    #[test]
    fn detect_and_recover_does_not_false_match_after_nonce_reuse() {
        let db = temp_db("detect-nonce-reuse");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        // Generation 1: create batch 0 (closed) + batch 1 (open).
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        // Assign nonce 0 to batch 0.
        storage.assign_batch_nonces().expect("assign nonces gen1");

        // Simulate stale submission of batch 0 with nonce 0.
        let batch_submitter = Address::repeat_byte(0xAA);
        let stale_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 10,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: stale_payload,
                    block_number: 1210,
                }],
            )
            .expect("append stale safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab gen1");

        // First recovery: invalidates batch 0 and 1, opens batch 2.
        let first = storage.detect_and_recover(1200).expect("first recovery");
        assert_eq!(first, vec![0, 1]);

        // Generation 2: close batch 2 (recovery batch) to create batch 3 (new open).
        let mut head = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close recovery batch");

        // Assign nonce to batch 2 — it should get nonce 0 (reused).
        storage.assign_batch_nonces().expect("assign nonces gen2");

        // Second detect_and_recover: the old stale submission was skipped by
        // populate_safe_accepted_batches (it's stale), so the frontier is 0.
        // The valid batch with nonce 0 is batch 2, which is NOT stale (safe_block ≈ 1210).
        let second = storage.detect_and_recover(1200).expect("second recovery");
        assert!(
            second.is_empty(),
            "old stale row must not false-match new-generation batch with reused nonce"
        );
    }

    #[test]
    fn detect_and_recover_detects_stale_reused_nonce_in_new_generation() {
        // Regression test: after gen1 recovery, if gen2's batch (with reused nonce) ALSO
        // becomes stale, it must still be detected — the nonce must not be permanently
        // blacklisted.
        let db = temp_db("detect-reused-stale");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        // Gen1: batch 0 (closed) + batch 1 (open), nonce 0 assigned.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        storage.assign_batch_nonces().expect("assign nonces gen1");

        // Gen1 stale submission.
        let batch_submitter = Address::repeat_byte(0xAA);
        let gen1_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 10,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: gen1_payload,
                    block_number: 1210,
                }],
            )
            .expect("append gen1 stale safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab gen1");

        // Gen1 recovery: invalidates 0,1, opens batch 2.
        let first = storage.detect_and_recover(1200).expect("gen1 recovery");
        assert_eq!(first, vec![0, 1]);

        // Gen2: close batch 2, opens batch 3. Assign nonce 0 (reused) to batch 2.
        let mut head = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close gen2 batch");
        storage.assign_batch_nonces().expect("assign nonces gen2");

        // Gen2 submission is ALSO stale (reuses nonce 0).
        let gen2_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 100,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                2410,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: gen2_payload,
                    block_number: 2410,
                }],
            )
            .expect("append gen2 stale safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab gen2");

        // Gen2 recovery: nonce 0 is stale AGAIN, must cascade batch 2 and 3.
        let second = storage.detect_and_recover(1200).expect("gen2 recovery");
        assert_eq!(
            second,
            vec![2, 3],
            "stale reused nonce in gen2 must still be detected"
        );
    }

    #[test]
    fn detect_and_recover_opens_batch_after_torn_invalidation() {
        // Regression test for P1: if a previous boot invalidated the suffix but crashed
        // before opening a recovery batch, the next boot must still open one.
        let db = temp_db("detect-torn");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        // Create batch 0 (closed) + batch 1 (open).
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        // Simulate torn state: manually invalidate both batches without opening a
        // recovery batch. This is what would happen if the process crashed mid-recovery.
        storage.insert_invalid_batch(0).expect("invalidate 0");
        storage.insert_invalid_batch(1).expect("invalidate 1");

        // detect_and_recover finds no NEW stale batches (no safe_accepted_batches data),
        // but should notice there's no valid open batch and open one.
        let invalidated = storage
            .detect_and_recover(1200)
            .expect("recover from torn state");
        assert!(invalidated.is_empty(), "no new invalidations");

        // A fresh recovery batch should exist.
        let head = storage.load_open_state().expect("load open state");
        assert!(head.is_some(), "recovery should have opened a fresh batch");
        assert_eq!(head.unwrap().batch_index, 2);
    }

    #[test]
    fn recovery_redrains_direct_inputs_and_replay_sees_them_once() {
        // End-to-end regression test: direct inputs drained into an invalidated batch
        // must be re-drained into the recovery batch, and catch-up replay (which
        // filters invalid batches) must see each direct input exactly once.
        let db = temp_db("recovery-redrain-e2e");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        // Create batch 0 (open at safe_block=10) and drain two deposits into it.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        let deposits = vec![
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xd1],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xd2],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, deposits.as_slice())
            .expect("append deposits");
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, 2))
            .expect("close frame with deposits");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");

        // Before invalidation: both deposits visible in replay.
        let before = storage.load_ordered_l2_txs_from(0).expect("replay before");
        assert_eq!(before.len(), 2, "both deposits should be visible");

        // Assign nonce 0 to batch 0, then simulate stale submission.
        storage.assign_batch_nonces().expect("assign nonces");
        let batch_submitter = Address::repeat_byte(0xAA);
        let stale_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 10,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: stale_payload,
                    block_number: 1210,
                }],
            )
            .expect("append stale batch submission");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

        // Recovery: cascade-invalidate batch 0 and open batch 1, opens batch 2.
        let invalidated = storage
            .detect_and_recover(1200)
            .expect("detect and recover");
        assert!(!invalidated.is_empty(), "should have invalidated batches");

        // After recovery: replay should still see exactly 2 deposits (re-drained
        // into the recovery batch, not doubled or lost).
        let after = storage.load_ordered_l2_txs_from(0).expect("replay after");
        let direct_payloads: Vec<&[u8]> = after
            .iter()
            .filter_map(|tx| match tx {
                SequencedL2Tx::Direct(d) if d.sender != batch_submitter => {
                    Some(d.payload.as_slice())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            direct_payloads,
            vec![&[0xd1][..], &[0xd2][..]],
            "deposits must appear exactly once in replay after recovery"
        );

        // Verify the re-drained deposits are in the recovery batch, not the invalid one.
        let recovery_batch = storage.load_open_state().expect("load").unwrap();
        let recovery_txs = storage
            .load_ordered_l2_txs_for_batch(recovery_batch.batch_index)
            .expect("load recovery batch txs");
        let recovery_direct_count = recovery_txs
            .iter()
            .filter(|tx| matches!(tx, SequencedL2Tx::Direct(d) if d.sender != batch_submitter))
            .count();
        assert_eq!(
            recovery_direct_count, 2,
            "both deposits should be in the recovery batch"
        );
    }

    #[test]
    fn check_danger_zone_ignores_old_gold_batches() {
        let db = temp_db("danger-zone-gold");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let batch_submitter = Address::repeat_byte(0xAA);

        // Create a batch at safe_block=10 and close it.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 0");
        storage.assign_batch_nonces().expect("assign nonces");

        // Submit batch 0 to L1 and have it accepted (Gold).
        let batch_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 10,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: batch_payload,
                    block_number: 20,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

        // Advance safe block far past batch 0's safe_block.
        // Batch 0 is now very old (age = 5000 - 10 = 4990), but it's Gold (accepted).
        // The frontier is batch 1 (the open batch), which has safe_block=100 and is young.
        storage
            .append_safe_inputs(5000, &[])
            .expect("advance safe block");

        // Danger zone check with threshold=1125 should NOT trigger,
        // because the frontier (first unresolved batch) is batch 1 at safe_block=100,
        // and its age is 5000-100=4900 which IS past threshold...
        // but batch 1 doesn't have a nonce yet (it's the open batch, not in batch_nonces).
        // The frontier nonce is 1 (next after accepted nonce 0), and there's no local
        // batch with nonce 1 in batch_nonces. So check_danger_zone returns None.
        let result = storage.check_danger_zone(1125).expect("check danger zone");
        assert!(
            result.is_none(),
            "old Gold batches should not trigger danger zone; got batch_index={result:?}"
        );
    }

    #[test]
    fn check_danger_zone_triggers_on_frontier_batch() {
        let db = temp_db("danger-zone-frontier");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let batch_submitter = Address::repeat_byte(0xAA);

        // Create two batches: batch 0 at safe_block=10, batch 1 at safe_block=10.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 1");
        storage.assign_batch_nonces().expect("assign nonces");

        // Batch 0 is accepted (Gold). Batch 1 is the frontier (first unresolved).
        let batch_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 10,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: batch_payload,
                    block_number: 20,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

        // Advance safe block past the danger threshold for batch 1.
        // Batch 1 has safe_block=10. With threshold=1125: stale when safe_block >= 10+1125 = 1135.
        storage
            .append_safe_inputs(1200, &[])
            .expect("advance safe block");

        // Danger zone should trigger on batch 1 (the frontier).
        let result = storage.check_danger_zone(1125).expect("check danger zone");
        assert_eq!(result, Some(1), "frontier batch should trigger danger zone");
    }

    #[test]
    fn check_danger_zone_does_not_trigger_below_threshold() {
        let db = temp_db("danger-zone-below");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let batch_submitter = Address::repeat_byte(0xAA);

        // Create two closed batches.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch 1");
        storage.assign_batch_nonces().expect("assign nonces");

        // Batch 0 accepted.
        let batch_payload = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
            nonce: 0,
            frames: vec![sequencer_core::batch::Frame {
                safe_block: 10,
                fee_price: 0,
                user_ops: vec![],
            }],
        });
        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: batch_submitter,
                    payload: batch_payload,
                    block_number: 20,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(batch_submitter, 1200)
            .expect("populate sab");

        // Advance safe block to just below the danger threshold for batch 1.
        // Batch 1 has safe_block=10. Threshold=1125. Age=1134-10=1124 < 1125.
        storage
            .append_safe_inputs(1134, &[])
            .expect("advance safe block");

        let result = storage.check_danger_zone(1125).expect("check danger zone");
        assert!(
            result.is_none(),
            "should not trigger below threshold; got batch_index={result:?}"
        );
    }

    // ── Tests cherry-picked from remote feature/recovery ──────────

    fn make_stale_batch_payload(nonce: u64, safe_block: u64) -> Vec<u8> {
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
    fn detect_and_recover_boundary_exactly_max_wait_is_stale() {
        let db = temp_db("detect-boundary-exact");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let max_wait: u64 = 1200;

        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch");
        storage.assign_batch_nonces().expect("assign nonces");

        storage
            .append_safe_inputs(
                1300,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 100),
                    block_number: 1300,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate sab");

        let invalidated = storage.detect_and_recover(max_wait).expect("detect");
        assert_eq!(invalidated, vec![0, 1], "exactly at max_wait must be stale");
        assert_eq!(
            storage
                .load_open_state()
                .expect("load")
                .unwrap()
                .batch_index,
            2
        );
    }

    #[test]
    fn detect_and_recover_boundary_one_below_max_wait_is_not_stale() {
        let db = temp_db("detect-boundary-one-below");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let max_wait: u64 = 1200;

        let mut head = storage
            .initialize_open_state(100, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage
            .close_frame_and_batch(&mut head, 100)
            .expect("close batch");
        storage.assign_batch_nonces().expect("assign nonces");

        // inclusion_block - safe_block = 1299 - 100 = 1199 < 1200
        storage
            .append_safe_inputs(
                1299,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 100),
                    block_number: 1299,
                }],
            )
            .expect("append safe input");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate sab");

        let invalidated = storage.detect_and_recover(max_wait).expect("detect");
        assert!(
            invalidated.is_empty(),
            "one below max_wait must not be stale"
        );
    }

    #[test]
    fn detect_and_recover_all_batches_invalidated_frontier_zero() {
        let db = temp_db("detect-frontier-zero");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let max_wait: u64 = 1200;

        // Close 3 batches all at safe_block=10.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        for _ in 0..3 {
            storage.close_frame_and_batch(&mut head, 10).expect("close");
        }
        storage.assign_batch_nonces().expect("assign nonces");

        // Nonce 0 stale at inclusion.
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate");

        let inv = storage.detect_and_recover(max_wait).expect("detect");
        assert_eq!(inv, vec![0, 1, 2, 3]);
        assert!(storage.load_open_state().expect("open").is_some());
    }

    #[test]
    fn detect_and_recover_recovery_batch_itself_becomes_stale() {
        let db = temp_db("detect-recovery-stale");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let max_wait: u64 = 1200;

        // Gen 1: batch 0 at safe_block=10, close it.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        storage.close_frame_and_batch(&mut head, 10).expect("close");
        storage.assign_batch_nonces().expect("nonces gen1");

        // Submit nonce 0 stale.
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append gen1");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate gen1");
        let inv1 = storage.detect_and_recover(max_wait).expect("recover gen1");
        assert_eq!(inv1, vec![0, 1]);

        // Gen 2: close the recovery batch, assign nonce (reuses nonce 0).
        let mut head2 = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head2, 1210)
            .expect("close gen2");
        storage.assign_batch_nonces().expect("nonces gen2");

        // Gen 2 nonce 0 also arrives stale.
        storage
            .append_safe_inputs(
                2410,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 1210),
                    block_number: 2410,
                }],
            )
            .expect("append gen2");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate gen2");
        let inv2 = storage.detect_and_recover(max_wait).expect("recover gen2");
        assert_eq!(inv2, vec![2, 3]);
        assert!(storage.load_open_state().expect("open").is_some());
    }

    #[test]
    fn detect_and_recover_multi_round_gen3_recovery() {
        let db = temp_db("detect-gen3");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let max_wait: u64 = 1200;

        // Gen 1: stale.
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head, 10).expect("close");
        storage.assign_batch_nonces().expect("nonces");
        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate");
        storage.detect_and_recover(max_wait).expect("recover gen1");

        // Gen 2: also stale.
        let mut head2 = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head2, 1210)
            .expect("close gen2");
        storage.assign_batch_nonces().expect("nonces gen2");
        storage
            .append_safe_inputs(
                2410,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 1210),
                    block_number: 2410,
                }],
            )
            .expect("append gen2");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate gen2");
        storage.detect_and_recover(max_wait).expect("recover gen2");

        // Gen 3: healthy.
        let mut head3 = storage.load_open_state().expect("load").unwrap();
        storage
            .close_frame_and_batch(&mut head3, 2410)
            .expect("close gen3");
        storage.assign_batch_nonces().expect("nonces gen3");
        storage
            .append_safe_inputs(
                2420,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 2410),
                    block_number: 2420,
                }],
            )
            .expect("append gen3");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate gen3");
        let inv3 = storage.detect_and_recover(max_wait).expect("recover gen3");
        assert!(inv3.is_empty(), "gen3 should be healthy");
    }

    #[test]
    fn detect_and_recover_large_cascade_50_batches() {
        let db = temp_db("detect-large-cascade");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        let max_wait: u64 = 1200;

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("initialize");
        for _ in 0..50 {
            storage.close_frame_and_batch(&mut head, 10).expect("close");
        }
        storage.assign_batch_nonces().expect("assign nonces");

        storage
            .append_safe_inputs(
                1210,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 1210,
                }],
            )
            .expect("append");
        storage
            .populate_safe_accepted_batches(SENDER_A, max_wait)
            .expect("populate");

        let inv = storage.detect_and_recover(max_wait).expect("detect");
        assert_eq!(inv.len(), 51); // 50 closed + 1 open
    }

    #[test]
    fn populate_safe_accepted_batches_skips_duplicate_nonces() {
        let db = temp_db("populate-dup-nonces");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head, 10).expect("close");
        storage.assign_batch_nonces().expect("nonces");

        // Submit nonce 0 twice (duplicate).
        storage
            .append_safe_inputs(
                20,
                &[
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 20,
                    },
                    StoredSafeInput {
                        sender: SENDER_A,
                        payload: make_stale_batch_payload(0, 10),
                        block_number: 20,
                    },
                ],
            )
            .expect("append");
        storage
            .populate_safe_accepted_batches(SENDER_A, 1200)
            .expect("populate");

        let (_, next) = storage
            .load_safe_accepted_frontier()
            .expect("load frontier");
        assert_eq!(next, 1, "duplicate nonce must be skipped");
    }

    #[test]
    fn populate_safe_accepted_batches_handles_large_nonce_gap() {
        let db = temp_db("populate-nonce-gap");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head, 10).expect("close");
        storage.assign_batch_nonces().expect("nonces");

        // Submit nonce 5 (gap: 0 expected, 5 provided).
        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(5, 10),
                    block_number: 20,
                }],
            )
            .expect("append");
        storage
            .populate_safe_accepted_batches(SENDER_A, 1200)
            .expect("populate");

        let (_, next) = storage
            .load_safe_accepted_frontier()
            .expect("load frontier");
        assert_eq!(next, 0, "gap must stall frontier");
    }

    #[test]
    fn populate_safe_accepted_batches_out_of_order_arrivals_stalls_frontier() {
        let db = temp_db("populate-out-of-order");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");

        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .expect("init");
        storage.close_frame_and_batch(&mut head, 10).expect("close");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close 2");
        storage.assign_batch_nonces().expect("nonces");

        // Submit nonce 1 before nonce 0.
        storage
            .append_safe_inputs(
                20,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(1, 10),
                    block_number: 20,
                }],
            )
            .expect("append");
        storage
            .populate_safe_accepted_batches(SENDER_A, 1200)
            .expect("populate");

        let (_, next) = storage
            .load_safe_accepted_frontier()
            .expect("load frontier");
        assert_eq!(next, 0, "out of order must stall frontier");

        // Now submit nonce 0.
        storage
            .append_safe_inputs(
                21,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: make_stale_batch_payload(0, 10),
                    block_number: 21,
                }],
            )
            .expect("append nonce 0");
        storage
            .populate_safe_accepted_batches(SENDER_A, 1200)
            .expect("populate again");

        let (_, next2) = storage
            .load_safe_accepted_frontier()
            .expect("load frontier again");
        assert_eq!(next2, 1, "frontier must remain stalled");
    }
}
