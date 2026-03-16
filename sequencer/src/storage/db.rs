// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use rusqlite::{Connection, OpenFlags, Result, Transaction, TransactionBehavior};
use rusqlite_migration::{M, Migrations};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::sql::{
    sql_count_user_ops_for_frame, sql_insert_open_batch, sql_insert_open_batch_with_index,
    sql_insert_open_frame, sql_insert_safe_inputs_batch,
    sql_insert_sequenced_direct_inputs_for_frame, sql_insert_user_ops_and_sequenced_batch,
    sql_select_frames_for_batch, sql_select_latest_batch_index,
    sql_select_latest_batch_with_user_op_count, sql_select_latest_frame_in_batch_for_batch,
    sql_select_max_safe_input_index, sql_select_ordered_l2_tx_count,
    sql_select_ordered_l2_txs_for_batch, sql_select_ordered_l2_txs_from_offset,
    sql_select_ordered_l2_txs_page_from_offset, sql_select_recommended_fee, sql_select_safe_block,
    sql_select_safe_input_payloads_for_sender, sql_select_safe_inputs_range,
    sql_select_total_drained_direct_inputs, sql_select_user_ops_for_frame,
    sql_update_recommended_fee, sql_update_safe_block,
};
use super::{
    FrameHeader, SafeFrontier, SafeInputRange, StorageOpenError, StoredSafeInput, WriteHead,
};
use crate::inclusion_lane::PendingUserOp;
use alloy_primitives::Address;
use sequencer_core::batch::{Batch, BatchForSubmission, Frame as BatchFrame, WireUserOp};
use sequencer_core::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};

const MIGRATION_0001_SCHEMA: &str = include_str!("migrations/0001_schema.sql");

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
            let changed_rows = sql_update_safe_block(&tx, u64_to_i64(minimum_safe_block))?;
            if changed_rows != 1 {
                return Err(rusqlite::Error::StatementChangedRows(changed_rows));
            }
        }
        tx.commit()?;
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

    pub fn load_safe_input_payloads_for_sender(
        &mut self,
        sender: Address,
    ) -> Result<(u64, Vec<Vec<u8>>)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let safe_block = query_current_safe_block(&tx)?;
        let payloads = sql_select_safe_input_payloads_for_sender(&tx, sender.as_slice())?;
        tx.commit()?;
        Ok((safe_block, payloads))
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
        let changed_rows = sql_update_safe_block(&tx, u64_to_i64(safe_block))?;
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
        let frame_fee = query_recommended_fee(&tx)?;
        insert_open_batch_with_index(&tx, 0, now_ms)?;
        insert_open_frame(&tx, 0, 0, now_ms, frame_fee, safe_block)?;
        persist_frame_direct_sequence(&tx, 0, 0, leading_direct_range)?;
        tx.commit()?;

        Ok(WriteHead {
            batch_index: 0,
            batch_created_at: from_unix_ms(now_ms),
            frame_fee,
            safe_block,
            batch_user_op_count: 0,
            open_frame_user_op_count: 0,
            frame_in_batch: 0,
        })
    }

    pub fn recommended_fee(&mut self) -> Result<u64> {
        let value = sql_select_recommended_fee(&self.conn)?;
        Ok(i64_to_u64(value))
    }

    pub fn set_recommended_fee(&mut self, fee: u64) -> Result<()> {
        let changed_rows = sql_update_recommended_fee(&self.conn, u64_to_i64(fee))?;
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

        sql_insert_user_ops_and_sequenced_batch(
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
        let next_frame_fee = query_recommended_fee(&tx)?;
        let next_frame_in_batch = head.frame_in_batch.saturating_add(1);
        insert_open_frame(
            &tx,
            head.batch_index,
            next_frame_in_batch,
            now_ms,
            next_frame_fee,
            next_safe_block,
        )?;
        persist_frame_direct_sequence(
            &tx,
            head.batch_index,
            next_frame_in_batch,
            leading_direct_range,
        )?;
        tx.commit()?;
        head.advance_frame(next_frame_fee, next_safe_block);
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
        // Frame fee is committed here: we sample the current recommendation once and
        // assign it to the newly opened frame.
        let next_frame_fee = query_recommended_fee(&tx)?;
        let next_batch_index = insert_open_batch(&tx, now_ms)?;
        insert_open_frame(
            &tx,
            next_batch_index,
            0,
            now_ms,
            next_frame_fee,
            next_safe_block,
        )?;
        tx.commit()?;
        head.move_to_next_batch(
            next_batch_index,
            from_unix_ms(now_ms),
            next_frame_fee,
            next_safe_block,
        );
        Ok(())
    }

    pub fn load_ordered_l2_txs_from(&mut self, offset: u64) -> Result<Vec<SequencedL2Tx>> {
        // Read the persisted total order used by catch-up and downstream feed readers.
        let rows = sql_select_ordered_l2_txs_from_offset(&self.conn, u64_to_i64(offset))?;
        Ok(decode_ordered_l2_txs(rows))
    }

    pub fn load_ordered_l2_txs_page_from(
        &mut self,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<SequencedL2Tx>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let rows = sql_select_ordered_l2_txs_page_from_offset(
            &self.conn,
            u64_to_i64(offset),
            usize_to_i64(limit),
        )?;
        Ok(decode_ordered_l2_txs(rows))
    }

    pub fn ordered_l2_tx_count(&mut self) -> Result<u64> {
        let value = sql_select_ordered_l2_tx_count(&self.conn)?;
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
                fee: i64_to_u64(row.fee),
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
                    max_fee: i64_to_u32(row.max_fee),
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

        let batch = Batch {
            nonce: batch_index,
            frames,
        };
        let created_at_ms_u64 = created_at_ms.max(0) as u64;

        Ok(BatchForSubmission {
            batch_index,
            created_at_ms: created_at_ms_u64,
            batch,
        })
    }
}

fn decode_ordered_l2_txs(rows: Vec<super::sql::OrderedL2TxRow>) -> Vec<SequencedL2Tx> {
    let mut out = Vec::new();

    for row in rows {
        if row.kind == 0 {
            let sender_bytes = row.sender.expect("ordered replay row: missing sender");
            assert_eq!(
                sender_bytes.len(),
                20,
                "ordered replay row: sender must be 20 bytes"
            );

            let entry = ValidUserOp {
                sender: Address::from_slice(sender_bytes.as_slice()),
                // Replay uses the persisted frame fee to mirror canonical execution.
                fee: i64_to_u64(row.fee.expect("ordered replay row: missing fee")),
                data: row.data.expect("ordered replay row: missing data"),
            };
            out.push(SequencedL2Tx::UserOp(entry));
        } else {
            let direct = DirectInput {
                sender: Address::from_slice(
                    row.sender
                        .expect("ordered replay row: missing sender")
                        .as_slice(),
                ),
                block_number: i64_to_u64(
                    row.block_number
                        .expect("ordered replay row: missing block_number"),
                ),
                payload: row.payload.expect("ordered replay row: missing payload"),
            };
            out.push(SequencedL2Tx::Direct(direct));
        }
    }

    out
}

fn load_current_write_head(tx: &Transaction<'_>) -> Result<Option<WriteHead>> {
    let Some((batch_index, batch_created_at, batch_user_op_count)) = query_latest_batch(tx)? else {
        return Ok(None);
    };
    let (frame_in_batch, frame_fee, safe_block) = query_latest_frame_in_batch(tx, batch_index)?;
    let open_frame_user_op_count = query_frame_user_op_count(tx, batch_index, frame_in_batch)?;
    Ok(Some(WriteHead {
        batch_index,
        batch_created_at,
        frame_fee,
        safe_block,
        batch_user_op_count,
        open_frame_user_op_count,
        frame_in_batch,
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

fn query_latest_frame_in_batch(tx: &Transaction<'_>, batch_index: u64) -> Result<(u32, u64, u64)> {
    let (frame_in_batch, frame_fee, safe_block) =
        sql_select_latest_frame_in_batch_for_batch(tx, u64_to_i64(batch_index))?;
    Ok((
        i64_to_u32(frame_in_batch),
        i64_to_u64(frame_fee),
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

fn query_recommended_fee(tx: &Transaction<'_>) -> Result<u64> {
    let value = sql_select_recommended_fee(tx)?;
    Ok(i64_to_u64(value))
}

fn persist_frame_direct_sequence(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    drained_direct_range: SafeInputRange,
) -> Result<()> {
    sql_insert_sequenced_direct_inputs_for_frame(
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
    frame_fee: u64,
    safe_block: u64,
) -> Result<()> {
    sql_insert_open_frame(
        tx,
        u64_to_i64(batch_index),
        i64::from(frame_in_batch),
        created_at_ms,
        u64_to_i64(frame_fee),
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
        assert_eq!(head_a.frame_fee, 0);

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
    fn next_frame_fee_comes_from_recommended_fee_singleton() {
        let db = temp_db("recommended-fee");
        let mut storage = Storage::open(db.path.as_str(), "NORMAL").expect("open storage");
        assert_eq!(storage.recommended_fee().expect("default recommended"), 0);

        storage.set_recommended_fee(7).expect("set recommended fee");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let next_safe_block = head.safe_block;
        storage
            .close_frame_and_batch(&mut head, next_safe_block)
            .expect("rotate batch");

        assert_eq!(head.frame_fee, 7);
        assert_eq!(storage.recommended_fee().expect("read recommended"), 7);
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
        assert_eq!(frame.fee_price, 0);
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
}
