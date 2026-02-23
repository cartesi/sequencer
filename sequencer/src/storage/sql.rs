// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use rusqlite::{Connection, Result, Row, Transaction, params};
use std::time::{SystemTime, UNIX_EPOCH};

use super::IndexedDirectInput;
use crate::inclusion_lane::PendingUserOp;

const SQL_SELECT_SAFE_INPUTS_RANGE: &str = include_str!("queries/select_safe_inputs_range.sql");
const SQL_SELECT_ORDERED_L2_TXS_FROM_OFFSET: &str =
    include_str!("queries/select_ordered_l2_txs_from_offset.sql");
const SQL_SELECT_LATEST_BATCH_WITH_USER_OP_COUNT: &str =
    include_str!("queries/select_latest_batch_with_user_op_count.sql");
const SQL_SELECT_LATEST_FRAME_IN_BATCH_FOR_BATCH: &str =
    include_str!("queries/select_latest_frame_in_batch_for_batch.sql");
const SQL_SELECT_USER_OP_COUNT_FOR_FRAME: &str =
    include_str!("queries/select_user_op_count_for_frame.sql");
const SQL_SELECT_MAX_DIRECT_INPUT_INDEX: &str = "SELECT MAX(direct_input_index) FROM direct_inputs";
const SQL_SELECT_RECOMMENDED_FEE: &str =
    "SELECT fee FROM recommended_fees WHERE singleton_id = 0 LIMIT 1";
const SQL_INSERT_DIRECT_INPUT: &str =
    "INSERT INTO direct_inputs (direct_input_index, payload) VALUES (?1, ?2)";
const SQL_INSERT_USER_OP: &str = include_str!("queries/insert_user_op.sql");
const SQL_INSERT_FRAME_DRAIN: &str =
    "INSERT INTO frame_drains (batch_index, frame_in_batch, drain_n) VALUES (?1, ?2, ?3)";
const SQL_UPDATE_RECOMMENDED_FEE: &str =
    "UPDATE recommended_fees SET fee = ?1 WHERE singleton_id = 0";

#[derive(Debug, Clone)]
pub(super) struct OrderedL2TxRow {
    pub kind: i64,
    pub sender: Option<Vec<u8>>,
    pub data: Option<Vec<u8>>,
    pub fee: Option<i64>,
    pub payload: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(super) struct SafeInputRow {
    pub direct_input_index: i64,
    pub payload: Vec<u8>,
}

pub(super) fn sql_select_total_drained_direct_inputs(conn: &Connection) -> Result<i64> {
    const SQL: &str = "SELECT COALESCE(SUM(drain_n), 0) FROM frame_drains";
    conn.query_row(SQL, [], |row| row.get(0))
}

pub(super) fn sql_select_max_direct_input_index(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row(
        SQL_SELECT_MAX_DIRECT_INPUT_INDEX,
        [],
        convert_row_to_optional_i64,
    )
}

pub(super) fn sql_select_recommended_fee(conn: &Connection) -> Result<i64> {
    conn.query_row(SQL_SELECT_RECOMMENDED_FEE, [], |row| row.get(0))
}

pub(super) fn sql_update_recommended_fee(conn: &Connection, fee: i64) -> Result<usize> {
    conn.execute(SQL_UPDATE_RECOMMENDED_FEE, params![fee])
}

pub(super) fn sql_select_safe_inputs_range(
    conn: &Connection,
    from_inclusive: i64,
    to_exclusive: i64,
) -> Result<Vec<SafeInputRow>> {
    let mut stmt = conn.prepare_cached(SQL_SELECT_SAFE_INPUTS_RANGE)?;
    let mapped = stmt.query_map(
        params![from_inclusive, to_exclusive],
        convert_row_to_safe_input_row,
    )?;
    mapped.collect()
}

pub(super) fn sql_insert_direct_inputs_batch(
    tx: &Transaction<'_>,
    direct_inputs: &[IndexedDirectInput],
) -> Result<()> {
    if direct_inputs.is_empty() {
        return Ok(());
    }

    let mut stmt = tx.prepare_cached(SQL_INSERT_DIRECT_INPUT)?;
    for input in direct_inputs {
        stmt.execute(params![u64_to_i64(input.index), input.payload.as_slice()])?;
    }
    Ok(())
}

pub(super) fn sql_insert_user_ops_batch(
    tx: &Transaction<'_>,
    batch_index: i64,
    frame_in_batch: i64,
    frame_pos_start: u32,
    user_ops: &[PendingUserOp],
) -> Result<()> {
    if user_ops.is_empty() {
        return Ok(());
    }

    let mut stmt = tx.prepare_cached(SQL_INSERT_USER_OP)?;
    for (offset, item) in user_ops.iter().enumerate() {
        let pos_in_frame = frame_pos_start.saturating_add(offset as u32);
        let sig = item.signed.signature.as_bytes();
        stmt.execute(params![
            batch_index,
            frame_in_batch,
            i64::from(pos_in_frame),
            item.tx_hash.as_slice(),
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

pub(super) fn sql_select_ordered_l2_txs_from_offset(
    conn: &Connection,
    offset: i64,
) -> Result<Vec<OrderedL2TxRow>> {
    let mut stmt = conn.prepare_cached(SQL_SELECT_ORDERED_L2_TXS_FROM_OFFSET)?;
    let mapped = stmt.query_map(params![offset], convert_row_to_ordered_l2_tx_row)?;
    mapped.collect()
}

pub(super) fn sql_select_latest_batch_with_user_op_count(
    tx: &Transaction<'_>,
) -> Result<(i64, i64, i64, i64)> {
    tx.query_row(
        SQL_SELECT_LATEST_BATCH_WITH_USER_OP_COUNT,
        [],
        convert_row_to_latest_batch_with_user_op_count,
    )
}

pub(super) fn sql_select_latest_frame_in_batch_for_batch(
    tx: &Transaction<'_>,
    batch_index: i64,
) -> Result<i64> {
    tx.query_row(
        SQL_SELECT_LATEST_FRAME_IN_BATCH_FOR_BATCH,
        params![batch_index],
        |row| row.get(0),
    )
}

pub(super) fn sql_count_user_ops_for_frame(
    tx: &Transaction<'_>,
    batch_index: i64,
    frame_in_batch: i64,
) -> Result<i64> {
    tx.query_row(
        SQL_SELECT_USER_OP_COUNT_FOR_FRAME,
        params![batch_index, frame_in_batch],
        |row| row.get(0),
    )
}

pub(super) fn sql_insert_frame_drain(
    tx: &Transaction<'_>,
    batch_index: i64,
    frame_in_batch: i64,
    drain_n: i64,
) -> Result<()> {
    let mut stmt = tx.prepare_cached(SQL_INSERT_FRAME_DRAIN)?;
    stmt.execute(params![batch_index, frame_in_batch, drain_n])?;
    Ok(())
}

pub(super) fn sql_insert_open_batch(
    tx: &Transaction<'_>,
    created_at_ms: i64,
    fee: i64,
) -> Result<usize> {
    const SQL: &str = "INSERT INTO batches (created_at_ms, fee) VALUES (?1, ?2)";
    tx.execute(SQL, params![created_at_ms, fee])
}

pub(super) fn sql_insert_open_frame(
    tx: &Transaction<'_>,
    batch_index: i64,
    frame_in_batch: i64,
    created_at_ms: i64,
) -> Result<usize> {
    const SQL: &str =
        "INSERT INTO frames (batch_index, frame_in_batch, created_at_ms) VALUES (?1, ?2, ?3)";
    tx.execute(SQL, params![batch_index, frame_in_batch, created_at_ms])
}

fn convert_row_to_optional_i64(row: &Row<'_>) -> Result<Option<i64>> {
    row.get(0)
}

fn convert_row_to_safe_input_row(row: &Row<'_>) -> Result<SafeInputRow> {
    Ok(SafeInputRow {
        direct_input_index: row.get(0)?,
        payload: row.get(1)?,
    })
}

fn convert_row_to_ordered_l2_tx_row(row: &Row<'_>) -> Result<OrderedL2TxRow> {
    Ok(OrderedL2TxRow {
        kind: row.get(0)?,
        sender: row.get(1)?,
        data: row.get(2)?,
        fee: row.get(3)?,
        payload: row.get(4)?,
    })
}

fn convert_row_to_latest_batch_with_user_op_count(row: &Row<'_>) -> Result<(i64, i64, i64, i64)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn to_unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        SQL_INSERT_DIRECT_INPUT, SQL_INSERT_FRAME_DRAIN, SQL_INSERT_USER_OP,
        sql_count_user_ops_for_frame, sql_insert_direct_inputs_batch, sql_insert_frame_drain,
        sql_insert_open_batch, sql_insert_open_frame, sql_insert_user_ops_batch,
        sql_select_latest_batch_with_user_op_count, sql_select_latest_frame_in_batch_for_batch,
        sql_select_max_direct_input_index, sql_select_ordered_l2_txs_from_offset,
        sql_select_recommended_fee, sql_select_safe_inputs_range,
        sql_select_total_drained_direct_inputs, sql_update_recommended_fee,
    };
    use crate::inclusion_lane::PendingUserOp;
    use crate::storage::IndexedDirectInput;
    use crate::storage::db::Storage;
    use crate::user_op::{SignedUserOp, UserOp};
    use alloy_primitives::{Address, B256, Signature};
    use rusqlite::{Connection, params};
    use std::time::SystemTime;
    use tokio::sync::oneshot;

    fn setup_conn() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory sqlite");
        Storage::run_migrations(&mut conn).expect("run migrations");
        conn
    }

    fn sample_pending_user_op(seed: u8, nonce: u32, max_fee: u32) -> PendingUserOp {
        let sender = Address::from_slice(&[seed; 20]);
        let signature = Signature::test_signature();
        let (respond_to, _recv) = oneshot::channel();
        PendingUserOp {
            signed: SignedUserOp {
                sender,
                signature,
                user_op: UserOp {
                    nonce,
                    max_fee,
                    data: vec![seed].into(),
                },
            },
            tx_hash: B256::from([seed; 32]),
            respond_to,
            received_at: SystemTime::now(),
        }
    }

    #[test]
    fn max_index_helpers_work_for_empty_and_non_empty_tables() {
        let mut conn = setup_conn();

        assert_eq!(
            sql_select_total_drained_direct_inputs(&conn).expect("total drained"),
            0
        );
        assert_eq!(
            sql_select_max_direct_input_index(&conn).expect("query max direct input"),
            None
        );

        conn.execute(SQL_INSERT_DIRECT_INPUT, params![0_i64, vec![0xaa_u8]])
            .expect("insert direct input 0");
        conn.execute(SQL_INSERT_DIRECT_INPUT, params![1_i64, vec![0xbb_u8]])
            .expect("insert direct input 1");
        assert_eq!(
            sql_select_max_direct_input_index(&conn).expect("query max direct input"),
            Some(1)
        );

        let tx = conn.transaction().expect("start tx");
        tx.execute(SQL_INSERT_FRAME_DRAIN, params![0_i64, 0_i64, 1_i64])
            .expect("insert frame drain");
        tx.commit().expect("commit tx");

        assert_eq!(
            sql_select_total_drained_direct_inputs(&conn).expect("total drained"),
            1
        );

        let tx = conn.transaction().expect("start tx");
        assert_eq!(
            sql_select_max_direct_input_index(&tx).expect("query max direct input in tx"),
            Some(1)
        );
    }

    #[test]
    fn safe_inputs_range_is_half_open_and_ordered() {
        let conn = setup_conn();

        conn.execute(SQL_INSERT_DIRECT_INPUT, params![0_i64, vec![0xaa_u8]])
            .expect("insert direct input 0");
        conn.execute(SQL_INSERT_DIRECT_INPUT, params![1_i64, vec![0xbb_u8]])
            .expect("insert direct input 1");
        conn.execute(SQL_INSERT_DIRECT_INPUT, params![2_i64, vec![0xcc_u8]])
            .expect("insert direct input 2");

        let empty = sql_select_safe_inputs_range(&conn, 1, 1).expect("query empty interval");
        assert!(empty.is_empty());

        let rows = sql_select_safe_inputs_range(&conn, 0, 2).expect("query non-empty interval");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].direct_input_index, 0);
        assert_eq!(rows[1].direct_input_index, 1);
    }

    #[test]
    fn ordered_l2_query_returns_user_ops_before_drained_directs_in_frame() {
        let conn = setup_conn();

        conn.execute(
            SQL_INSERT_USER_OP,
            params![
                0_i64,
                0_i64,
                0_i64,
                vec![0x10_u8; 32],
                vec![0x20_u8; 20],
                0_i64,
                1_i64,
                vec![0x30_u8],
                vec![0x40_u8; 65],
                0_i64
            ],
        )
        .expect("insert user op");
        conn.execute(SQL_INSERT_DIRECT_INPUT, params![0_i64, vec![0xaa_u8]])
            .expect("insert direct input");
        conn.execute(SQL_INSERT_FRAME_DRAIN, params![0_i64, 0_i64, 1_i64])
            .expect("insert frame drain");

        let rows = sql_select_ordered_l2_txs_from_offset(&conn, 0).expect("query ordered l2");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, 0);
        assert_eq!(rows[0].fee, Some(1));
        assert_eq!(rows[1].kind, 1);
        assert_eq!(rows[1].fee, None);
    }

    #[test]
    fn batch_and_frame_helpers_reflect_bootstrapped_open_state() {
        let mut conn = setup_conn();
        let tx = conn.transaction().expect("start tx");

        let (batch_index, _created_at_ms, fee, user_op_count) =
            sql_select_latest_batch_with_user_op_count(&tx).expect("query latest batch");
        assert_eq!(batch_index, 0);
        assert_eq!(fee, 1);
        assert_eq!(user_op_count, 0);

        let frame_in_batch =
            sql_select_latest_frame_in_batch_for_batch(&tx, batch_index).expect("latest frame");
        assert_eq!(frame_in_batch, 0);

        let frame_user_op_count =
            sql_count_user_ops_for_frame(&tx, batch_index, frame_in_batch).expect("count user ops");
        assert_eq!(frame_user_op_count, 0);
    }

    #[test]
    fn open_batch_and_frame_insert_helpers_work() {
        let mut conn = setup_conn();
        let tx = conn.transaction().expect("start tx");

        sql_insert_open_batch(&tx, 123, 7).expect("insert open batch");
        let new_batch = tx.last_insert_rowid();
        sql_insert_open_frame(&tx, new_batch, 0, 123).expect("insert open frame");
        tx.commit().expect("commit tx");

        let batch_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM batches", [], |row| row.get(0))
            .expect("count batches");
        let frame_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM frames", [], |row| row.get(0))
            .expect("count frames");
        assert!(batch_count >= 2);
        assert!(frame_count >= 2);
    }

    #[test]
    fn recommended_fee_helpers_read_and_update_singleton() {
        let conn = setup_conn();
        assert_eq!(
            sql_select_recommended_fee(&conn).expect("read recommended"),
            1
        );
        sql_update_recommended_fee(&conn, 9).expect("update recommended");
        assert_eq!(sql_select_recommended_fee(&conn).expect("read updated"), 9);
    }

    #[test]
    fn batch_insert_helpers_insert_multiple_rows() {
        let mut conn = setup_conn();
        let tx = conn.transaction().expect("start tx");

        let direct_inputs = vec![
            IndexedDirectInput {
                index: 0,
                payload: vec![0xaa_u8],
            },
            IndexedDirectInput {
                index: 1,
                payload: vec![0xbb_u8],
            },
        ];
        sql_insert_direct_inputs_batch(&tx, direct_inputs.as_slice())
            .expect("insert direct inputs batch");

        let user_ops = vec![
            sample_pending_user_op(0x20, 0, 1),
            sample_pending_user_op(0x21, 1, 1),
        ];
        sql_insert_user_ops_batch(&tx, 0, 0, 0, user_ops.as_slice())
            .expect("insert user ops batch");

        sql_insert_frame_drain(&tx, 0, 0, direct_inputs.len() as i64).expect("insert frame drain");

        tx.commit().expect("commit tx");

        let direct_inputs_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM direct_inputs", [], |row| row.get(0))
            .expect("count direct inputs");
        let user_ops_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM user_ops", [], |row| row.get(0))
            .expect("count user ops");
        let frame_drains_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM frame_drains", [], |row| row.get(0))
            .expect("count frame drains");

        assert_eq!(direct_inputs_count, 2);
        assert_eq!(user_ops_count, 2);
        assert_eq!(frame_drains_count, 1);
    }
}
