// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use rusqlite::{Connection, Result, Transaction, TransactionBehavior};
use rusqlite_migration::{M, Migrations};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::sql::{
    sql_count_user_ops_for_frame, sql_insert_direct_inputs_batch, sql_insert_frame_drain,
    sql_insert_open_batch, sql_insert_open_frame, sql_insert_user_ops_batch,
    sql_select_latest_batch_with_user_op_count, sql_select_latest_frame_in_batch_for_batch,
    sql_select_max_direct_input_index, sql_select_ordered_l2_txs_from_offset,
    sql_select_recommended_fee, sql_select_safe_inputs_range,
    sql_select_total_drained_direct_inputs, sql_update_recommended_fee,
};
use super::{IndexedDirectInput, StorageOpenError, WriteHead};
use crate::inclusion_lane::PendingUserOp;
use crate::l2_tx::{DirectInput, SequencedL2Tx, ValidUserOp};
use alloy_primitives::Address;

const MIGRATION_0001_SCHEMA: &str = include_str!("migrations/0001_schema.sql");
const MIGRATION_0002_VIEWS: &str = include_str!("migrations/0002_views.sql");

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: &str, synchronous: &str) -> std::result::Result<Self, StorageOpenError> {
        let conn = Self::open_connection_with_migrations(path, synchronous)?;
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

    pub fn open_connection_with_migrations(
        path: &str,
        synchronous: &str,
    ) -> std::result::Result<Connection, StorageOpenError> {
        let mut conn = Self::open_connection(path, synchronous)?;
        Self::run_migrations(&mut conn)?;
        Ok(conn)
    }

    pub fn run_migrations(conn: &mut Connection) -> std::result::Result<(), StorageOpenError> {
        Migrations::from_slice(&[M::up(MIGRATION_0001_SCHEMA), M::up(MIGRATION_0002_VIEWS)])
            .to_latest(conn)?;
        Ok(())
    }

    pub fn load_next_undrained_direct_input_index(&mut self) -> Result<u64> {
        let value = sql_select_total_drained_direct_inputs(&self.conn)?;
        Ok(i64_to_u64(value))
    }

    pub fn safe_input_end_exclusive(&mut self) -> Result<u64> {
        let value = sql_select_max_direct_input_index(&self.conn)?;
        Ok(match value {
            Some(last_index) => i64_to_u64(last_index).saturating_add(1),
            None => 0,
        })
    }

    pub fn fill_safe_inputs(
        &mut self,
        from_inclusive: u64,
        to_exclusive: u64,
        out: &mut Vec<IndexedDirectInput>,
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
            let index = i64_to_u64(row.direct_input_index);
            let expected = from_inclusive.saturating_add(offset as u64);

            assert_eq!(
                index, expected,
                "non-contiguous safe-input index: expected {expected}, found {index}"
            );

            out.push(IndexedDirectInput {
                index,
                payload: row.payload,
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

    pub fn append_safe_direct_inputs(&mut self, inputs: &[IndexedDirectInput]) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let mut next_expected = query_latest_direct_input_index_exclusive(&tx)?;
        for input in inputs {
            assert_eq!(
                input.index, next_expected,
                "direct input index must be contiguous from storage head"
            );
            next_expected = next_expected.saturating_add(1);
        }

        sql_insert_direct_inputs_batch(&tx, inputs)?;

        tx.commit()?;
        Ok(())
    }

    pub fn load_open_state(&mut self) -> Result<WriteHead> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let head = load_current_write_head(&tx)?;
        tx.commit()?;
        Ok(head)
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

        let frame_user_op_count =
            query_frame_user_op_count(&tx, head.batch_index, head.frame_in_batch)?;

        sql_insert_user_ops_batch(
            &tx,
            u64_to_i64(head.batch_index),
            i64::from(head.frame_in_batch),
            frame_user_op_count,
            user_ops,
        )?;

        tx.commit()?;
        head.increment_batch_user_op_count(user_ops.len());
        Ok(())
    }

    pub fn close_frame_only(
        &mut self,
        head: &mut WriteHead,
        drained_direct_count: usize,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_write_head_matches_open_state(&tx, head)?;
        let now_ms = now_unix_ms();
        persist_frame_drain(&tx, head, drained_direct_count)?;
        let next_frame_in_batch = head.frame_in_batch.saturating_add(1);
        insert_open_frame(&tx, head.batch_index, next_frame_in_batch, now_ms)?;
        tx.commit()?;
        head.advance_frame();
        Ok(())
    }

    pub fn close_frame_and_batch(
        &mut self,
        head: &mut WriteHead,
        drained_direct_count: usize,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        assert_write_head_matches_open_state(&tx, head)?;
        let now_ms = now_unix_ms();
        // Batch fee is committed here: we sample the current recommendation once and
        // assign it to the newly opened batch.
        let next_batch_fee = query_recommended_fee(&tx)?;
        persist_frame_drain(&tx, head, drained_direct_count)?;
        let next_batch_index = insert_open_batch(&tx, now_ms, next_batch_fee)?;
        insert_open_frame(&tx, next_batch_index, 0, now_ms)?;
        tx.commit()?;
        head.move_to_next_batch(next_batch_index, from_unix_ms(now_ms), next_batch_fee);
        Ok(())
    }

    pub fn load_ordered_l2_txs_from(&mut self, offset: u64) -> Result<Vec<SequencedL2Tx>> {
        // Read the persisted total order used by catch-up and downstream broadcasters.
        let rows = sql_select_ordered_l2_txs_from_offset(&self.conn, u64_to_i64(offset))?;
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
                    // Replay uses the persisted batch fee to mirror canonical execution.
                    fee: i64_to_u64(row.fee.expect("ordered replay row: missing fee")),
                    data: row.data.expect("ordered replay row: missing data"),
                };
                out.push(SequencedL2Tx::UserOp(entry));
            } else {
                let direct = DirectInput {
                    payload: row.payload.expect("ordered replay row: missing payload"),
                };
                out.push(SequencedL2Tx::Direct(direct));
            }
        }

        Ok(out)
    }
}

fn load_current_write_head(tx: &Transaction<'_>) -> Result<WriteHead> {
    let (batch_index, batch_created_at, batch_fee, batch_user_op_count) = query_latest_batch(tx)?;
    let frame_in_batch = query_latest_frame_in_batch(tx, batch_index)?;
    Ok(WriteHead {
        batch_index,
        batch_created_at,
        batch_fee,
        batch_user_op_count,
        frame_in_batch,
    })
}

fn assert_write_head_matches_open_state(tx: &Transaction<'_>, expected: &WriteHead) -> Result<()> {
    let actual = load_current_write_head(tx)?;
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
        expected.batch_fee, actual.batch_fee,
        "stale WriteHead: batch_fee mismatch"
    );
    assert_eq!(
        to_unix_ms(expected.batch_created_at),
        to_unix_ms(actual.batch_created_at),
        "stale WriteHead: batch_created_at mismatch"
    );
    Ok(())
}

fn query_latest_batch(tx: &Transaction<'_>) -> Result<(u64, SystemTime, u64, u64)> {
    let (batch_index, batch_created_at_ms, batch_fee, batch_user_op_count) =
        sql_select_latest_batch_with_user_op_count(tx)?;
    Ok((
        i64_to_u64(batch_index),
        from_unix_ms(batch_created_at_ms),
        i64_to_u64(batch_fee),
        i64_to_u64(batch_user_op_count),
    ))
}

fn query_latest_frame_in_batch(tx: &Transaction<'_>, batch_index: u64) -> Result<u32> {
    let value = sql_select_latest_frame_in_batch_for_batch(tx, u64_to_i64(batch_index))?;
    Ok(i64_to_u32(value))
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

fn query_latest_direct_input_index_exclusive(tx: &Connection) -> Result<u64> {
    let value = sql_select_max_direct_input_index(tx)?;
    Ok(match value {
        Some(last_index) => i64_to_u64(last_index).saturating_add(1),
        None => 0,
    })
}

fn query_recommended_fee(tx: &Transaction<'_>) -> Result<u64> {
    let value = sql_select_recommended_fee(tx)?;
    Ok(i64_to_u64(value))
}

fn persist_frame_drain(
    tx: &Transaction<'_>,
    head: &WriteHead,
    drained_direct_count: usize,
) -> Result<()> {
    sql_insert_frame_drain(
        tx,
        u64_to_i64(head.batch_index),
        i64::from(head.frame_in_batch),
        u64_to_i64(drained_direct_count as u64),
    )
}

fn insert_open_batch(tx: &Transaction<'_>, created_at_ms: i64, fee: u64) -> Result<u64> {
    sql_insert_open_batch(tx, created_at_ms, u64_to_i64(fee))?;
    Ok(i64_to_u64(tx.last_insert_rowid()))
}

fn insert_open_frame(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    created_at_ms: i64,
) -> Result<()> {
    sql_insert_open_frame(
        tx,
        u64_to_i64(batch_index),
        i64::from(frame_in_batch),
        created_at_ms,
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

fn i64_to_u64(value: i64) -> u64 {
    value.max(0) as u64
}

fn i64_to_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::Storage;
    use crate::l2_tx::SequencedL2Tx;
    use crate::storage::IndexedDirectInput;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path(name: &str) -> String {
        let mut path = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        path.push(format!("sequencer-{name}-{unique}.sqlite"));
        path_to_string(path)
    }

    fn path_to_string(path: PathBuf) -> String {
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn open_state_is_idempotent_and_rotation_is_atomic() {
        let db_path = temp_db_path("open-state");
        let mut storage = Storage::open(&db_path, "NORMAL").expect("open storage");

        let head_a = storage.load_open_state().expect("load open state");
        let head_b = storage.load_open_state().expect("load existing open state");

        assert_eq!(head_a.batch_index, head_b.batch_index);
        assert_eq!(head_a.frame_in_batch, head_b.frame_in_batch);
        assert_eq!(head_a.batch_fee, head_b.batch_fee);
        assert_eq!(head_a.batch_fee, 1);

        let mut head_c = head_b;
        storage
            .close_frame_only(&mut head_c, 0)
            .expect("rotate within same batch");
        assert_eq!(head_c.batch_index, head_b.batch_index);
        assert_eq!(head_c.frame_in_batch, 1);

        let mut head_d = head_c;
        storage
            .close_frame_and_batch(&mut head_d, 0)
            .expect("close batch and rotate");
        assert!(head_d.batch_index > head_c.batch_index);
        assert_eq!(head_d.frame_in_batch, 0);
    }

    #[test]
    fn next_batch_fee_comes_from_recommended_fee_singleton() {
        let db_path = temp_db_path("recommended-fee");
        let mut storage = Storage::open(&db_path, "NORMAL").expect("open storage");
        assert_eq!(storage.recommended_fee().expect("default recommended"), 1);

        storage.set_recommended_fee(7).expect("set recommended fee");

        let mut head = storage.load_open_state().expect("load open state");
        storage
            .close_frame_and_batch(&mut head, 0)
            .expect("rotate batch");

        assert_eq!(head.batch_fee, 7);
        assert_eq!(storage.recommended_fee().expect("read recommended"), 7);
    }

    #[test]
    fn replay_returns_direct_inputs_in_drain_order() {
        let db_path = temp_db_path("replay-order");
        let mut storage = Storage::open(&db_path, "NORMAL").expect("open storage");
        let head = storage.load_open_state().expect("load open state");

        let drained = vec![
            IndexedDirectInput {
                index: 0,
                payload: vec![0xaa],
            },
            IndexedDirectInput {
                index: 1,
                payload: vec![0xbb],
            },
        ];
        storage
            .append_safe_direct_inputs(drained.as_slice())
            .expect("insert direct inputs");
        let mut head = head;
        storage
            .close_frame_only(&mut head, drained.len())
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
    fn next_undrained_direct_input_index_is_derived_from_frame_drains() {
        let db_path = temp_db_path("safe-cursor");
        let mut storage = Storage::open(&db_path, "NORMAL").expect("open storage");
        assert_eq!(
            storage
                .load_next_undrained_direct_input_index()
                .expect("empty cursor"),
            0
        );

        let head = storage.load_open_state().expect("load open state");
        let drained = vec![
            IndexedDirectInput {
                index: 0,
                payload: vec![0x01],
            },
            IndexedDirectInput {
                index: 1,
                payload: vec![0x02],
            },
        ];
        storage
            .append_safe_direct_inputs(drained.as_slice())
            .expect("insert direct inputs");
        let mut head = head;
        storage
            .close_frame_only(&mut head, drained.len())
            .expect("close frame with directs");

        assert_eq!(
            storage
                .load_next_undrained_direct_input_index()
                .expect("derived cursor"),
            2
        );
    }

    #[test]
    fn safe_input_api_uses_half_open_intervals() {
        let db_path = temp_db_path("safe-input-api");
        let mut storage = Storage::open(&db_path, "NORMAL").expect("open storage");

        assert_eq!(storage.safe_input_end_exclusive().expect("safe head"), 0);
        let mut out = Vec::new();
        storage
            .fill_safe_inputs(0, 0, &mut out)
            .expect("query empty interval");
        assert!(out.is_empty());

        let inserted = vec![
            IndexedDirectInput {
                index: 0,
                payload: vec![0xa0],
            },
            IndexedDirectInput {
                index: 1,
                payload: vec![0xb1],
            },
        ];
        storage
            .append_safe_direct_inputs(inserted.as_slice())
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
}
