// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use rusqlite::{Connection, Result, Row, Transaction, params};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{SafeInputRange, StoredSafeInput};
use crate::inclusion_lane::PendingUserOp;

const SQL_SELECT_SAFE_INPUTS_RANGE: &str = include_str!("queries/select_safe_inputs_range.sql");
const SQL_SELECT_ORDERED_L2_TXS_FROM_OFFSET: &str =
    include_str!("queries/select_ordered_l2_txs_from_offset.sql");
const SQL_SELECT_ORDERED_L2_TXS_PAGE_FROM_OFFSET: &str =
    include_str!("queries/select_ordered_l2_txs_page_from_offset.sql");
const SQL_SELECT_LATEST_BATCH_WITH_USER_OP_COUNT: &str =
    include_str!("queries/select_latest_batch_with_user_op_count.sql");
const SQL_SELECT_LATEST_FRAME_IN_BATCH_FOR_BATCH: &str =
    include_str!("queries/select_latest_frame_in_batch_for_batch.sql");
const SQL_SELECT_USER_OP_COUNT_FOR_FRAME: &str =
    include_str!("queries/select_user_op_count_for_frame.sql");
const SQL_SELECT_ORDERED_L2_TXS_FOR_BATCH: &str =
    include_str!("queries/select_ordered_l2_txs_for_batch.sql");
const SQL_SELECT_LATEST_BATCH_INDEX: &str = "SELECT MAX(batch_index) FROM batches";
const SQL_SELECT_USER_OPS_FOR_FRAME: &str = "SELECT nonce, max_fee, data, sig FROM user_ops WHERE batch_index = ?1 AND frame_in_batch = ?2 ORDER BY pos_in_frame ASC";
const SQL_SELECT_MAX_SAFE_INPUT_INDEX: &str = "SELECT MAX(safe_input_index) FROM safe_inputs";
const SQL_SELECT_ORDERED_L2_TX_COUNT: &str = "SELECT COUNT(*) FROM sequenced_l2_txs";
const SQL_SELECT_BATCH_POLICY: &str = "SELECT log_recommended_fee, log_batch_size_target FROM batch_policy_derived WHERE singleton_id = 0 LIMIT 1";
const SQL_SELECT_SAFE_BLOCK: &str =
    "SELECT block_number FROM l1_safe_head WHERE singleton_id = 0 LIMIT 1";
const SQL_INSERT_SAFE_INPUT: &str = "INSERT INTO safe_inputs (safe_input_index, sender, payload, block_number) VALUES (?1, ?2, ?3, ?4)";
const SQL_INSERT_USER_OP: &str = include_str!("queries/insert_user_op.sql");
const SQL_INSERT_SEQUENCED_DIRECT_INPUT: &str =
    include_str!("queries/insert_sequenced_direct_input.sql");
const SQL_UPDATE_BATCH_POLICY_LOG_GAS_PRICE: &str =
    "UPDATE batch_policy SET log_gas_price = ?1 WHERE singleton_id = 0";
const SQL_UPDATE_BATCH_POLICY_ALPHA: &str =
    "UPDATE batch_policy SET log_alpha = ?1, log_one_plus_alpha = ?2 WHERE singleton_id = 0";
const SQL_UPDATE_SAFE_BLOCK: &str =
    "UPDATE l1_safe_head SET block_number = ?1 WHERE singleton_id = 0";
#[derive(Debug, Clone)]
pub(super) struct OrderedL2TxRow {
    pub kind: i64,
    pub sender: Option<Vec<u8>>,
    pub data: Option<Vec<u8>>,
    pub fee: Option<i64>,
    pub payload: Option<Vec<u8>>,
    pub block_number: Option<i64>,
}

#[derive(Debug, Clone)]
pub(super) struct SafeInputRow {
    pub safe_input_index: i64,
    pub sender: Vec<u8>,
    pub payload: Vec<u8>,
    pub block_number: i64,
}

#[derive(Debug, Clone)]
pub(super) struct FrameHeaderRow {
    pub frame_in_batch: i64,
    pub fee: i64,
    pub safe_block: i64,
}

#[derive(Debug, Clone)]
pub(super) struct FrameUserOpRow {
    pub nonce: i64,
    pub max_fee: i64,
    pub data: Vec<u8>,
    pub sig: Vec<u8>,
}

pub(super) fn sql_select_total_drained_direct_inputs(conn: &Connection) -> Result<i64> {
    const SQL: &str = "SELECT COUNT(*) FROM sequenced_l2_txs WHERE safe_input_index IS NOT NULL";
    conn.query_row(SQL, [], |row| row.get(0))
}

pub(super) fn sql_select_max_safe_input_index(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row(
        SQL_SELECT_MAX_SAFE_INPUT_INDEX,
        [],
        convert_row_to_optional_i64,
    )
}

pub(super) fn sql_select_latest_batch_index(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row(
        SQL_SELECT_LATEST_BATCH_INDEX,
        [],
        convert_row_to_optional_i64,
    )
}

/// Derived batch policy: (log_recommended_fee, log_batch_size_target).
pub(super) fn sql_select_batch_policy(conn: &Connection) -> Result<(i64, i64)> {
    conn.query_row(SQL_SELECT_BATCH_POLICY, [], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })
}

pub(super) fn sql_update_batch_policy_log_gas_price(
    conn: &Connection,
    log_gas_price: i64,
) -> Result<usize> {
    conn.execute(
        SQL_UPDATE_BATCH_POLICY_LOG_GAS_PRICE,
        params![log_gas_price],
    )
}

pub(super) fn sql_update_batch_policy_alpha(
    conn: &Connection,
    log_alpha: i64,
    log_one_plus_alpha: i64,
) -> Result<usize> {
    conn.execute(
        SQL_UPDATE_BATCH_POLICY_ALPHA,
        params![log_alpha, log_one_plus_alpha],
    )
}

pub(super) fn sql_select_safe_block(conn: &Connection) -> Result<i64> {
    conn.query_row(SQL_SELECT_SAFE_BLOCK, [], |row| row.get(0))
}

pub(super) fn sql_update_safe_block(conn: &Connection, safe_block: i64) -> Result<usize> {
    conn.execute(SQL_UPDATE_SAFE_BLOCK, params![safe_block])
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

pub(super) fn sql_select_frames_for_batch(
    conn: &Connection,
    batch_index: i64,
) -> Result<Vec<FrameHeaderRow>> {
    const SQL: &str = "SELECT frame_in_batch, fee, safe_block FROM frames WHERE batch_index = ?1 ORDER BY frame_in_batch ASC";
    let mut stmt = conn.prepare_cached(SQL)?;
    let mapped = stmt.query_map(params![batch_index], convert_row_to_frame_header_row)?;
    mapped.collect()
}

pub(super) fn sql_select_user_ops_for_frame(
    conn: &Connection,
    batch_index: i64,
    frame_in_batch: i64,
) -> Result<Vec<FrameUserOpRow>> {
    let mut stmt = conn.prepare_cached(SQL_SELECT_USER_OPS_FOR_FRAME)?;
    let mapped = stmt.query_map(
        params![batch_index, frame_in_batch],
        convert_row_to_frame_user_op_row,
    )?;
    mapped.collect()
}

pub(super) fn sql_insert_safe_inputs_batch(
    tx: &Transaction<'_>,
    start_index: u64,
    safe_inputs: &[StoredSafeInput],
) -> Result<()> {
    if safe_inputs.is_empty() {
        return Ok(());
    }

    let mut stmt = tx.prepare_cached(SQL_INSERT_SAFE_INPUT)?;
    for (offset, input) in safe_inputs.iter().enumerate() {
        stmt.execute(params![
            u64_to_i64(start_index.saturating_add(offset as u64)),
            input.sender.as_slice(),
            input.payload.as_slice(),
            u64_to_i64(input.block_number)
        ])?;
    }
    Ok(())
}

/// Insert user-ops into the `user_ops` table.
/// The `trg_sequence_user_op` trigger automatically appends a corresponding row
/// to `sequenced_l2_txs` for each inserted user-op.
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

pub(super) fn sql_insert_sequenced_direct_inputs(
    tx: &Transaction<'_>,
    batch_index: i64,
    frame_in_batch: i64,
    direct_range: SafeInputRange,
) -> Result<()> {
    if direct_range.is_empty() {
        return Ok(());
    }

    let mut stmt = tx.prepare_cached(SQL_INSERT_SEQUENCED_DIRECT_INPUT)?;
    for safe_input_index in direct_range.start_inclusive..direct_range.end_exclusive {
        stmt.execute(params![
            batch_index,
            frame_in_batch,
            u64_to_i64(safe_input_index),
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

pub(super) fn sql_select_ordered_l2_txs_for_batch(
    conn: &Connection,
    batch_index: i64,
) -> Result<Vec<OrderedL2TxRow>> {
    let mut stmt = conn.prepare_cached(SQL_SELECT_ORDERED_L2_TXS_FOR_BATCH)?;
    let mapped = stmt.query_map(params![batch_index], convert_row_to_ordered_l2_tx_row)?;
    mapped.collect()
}

pub(super) fn sql_select_ordered_l2_txs_page_from_offset(
    conn: &Connection,
    offset: i64,
    limit: i64,
) -> Result<Vec<OrderedL2TxRow>> {
    let mut stmt = conn.prepare_cached(SQL_SELECT_ORDERED_L2_TXS_PAGE_FROM_OFFSET)?;
    let mapped = stmt.query_map(params![offset, limit], convert_row_to_ordered_l2_tx_row)?;
    mapped.collect()
}

pub(super) fn sql_select_ordered_l2_tx_count(conn: &Connection) -> Result<i64> {
    conn.query_row(SQL_SELECT_ORDERED_L2_TX_COUNT, [], |row| row.get(0))
}

pub(super) fn sql_select_latest_batch_with_user_op_count(
    tx: &Transaction<'_>,
) -> Result<(i64, i64, i64)> {
    tx.query_row(
        SQL_SELECT_LATEST_BATCH_WITH_USER_OP_COUNT,
        [],
        convert_row_to_latest_batch_with_user_op_count,
    )
}

pub(super) fn sql_select_latest_frame_in_batch_for_batch(
    tx: &Transaction<'_>,
    batch_index: i64,
) -> Result<(i64, i64, i64)> {
    tx.query_row(
        SQL_SELECT_LATEST_FRAME_IN_BATCH_FOR_BATCH,
        params![batch_index],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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

pub(super) fn sql_insert_open_batch(tx: &Transaction<'_>, created_at_ms: i64) -> Result<usize> {
    const SQL: &str = "INSERT INTO batches (created_at_ms) VALUES (?1)";
    tx.execute(SQL, params![created_at_ms])
}

pub(super) fn sql_insert_open_batch_with_index(
    tx: &Transaction<'_>,
    batch_index: i64,
    created_at_ms: i64,
) -> Result<usize> {
    const SQL: &str = "INSERT INTO batches (batch_index, created_at_ms) VALUES (?1, ?2)";
    tx.execute(SQL, params![batch_index, created_at_ms])
}

pub(super) fn sql_insert_open_frame(
    tx: &Transaction<'_>,
    batch_index: i64,
    frame_in_batch: i64,
    created_at_ms: i64,
    fee: i64,
    safe_block: i64,
) -> Result<usize> {
    const SQL: &str = "INSERT INTO frames (batch_index, frame_in_batch, created_at_ms, fee, safe_block) VALUES (?1, ?2, ?3, ?4, ?5)";
    tx.execute(
        SQL,
        params![batch_index, frame_in_batch, created_at_ms, fee, safe_block],
    )
}

fn convert_row_to_optional_i64(row: &Row<'_>) -> Result<Option<i64>> {
    row.get(0)
}

fn convert_row_to_safe_input_row(row: &Row<'_>) -> Result<SafeInputRow> {
    Ok(SafeInputRow {
        safe_input_index: row.get(0)?,
        sender: row.get(1)?,
        payload: row.get(2)?,
        block_number: row.get(3)?,
    })
}

fn convert_row_to_frame_header_row(row: &Row<'_>) -> Result<FrameHeaderRow> {
    Ok(FrameHeaderRow {
        frame_in_batch: row.get(0)?,
        fee: row.get(1)?,
        safe_block: row.get(2)?,
    })
}

fn convert_row_to_frame_user_op_row(row: &Row<'_>) -> Result<FrameUserOpRow> {
    Ok(FrameUserOpRow {
        nonce: row.get(0)?,
        max_fee: row.get(1)?,
        data: row.get(2)?,
        sig: row.get(3)?,
    })
}

fn convert_row_to_ordered_l2_tx_row(row: &Row<'_>) -> Result<OrderedL2TxRow> {
    Ok(OrderedL2TxRow {
        kind: row.get(0)?,
        sender: row.get(1)?,
        data: row.get(2)?,
        fee: row.get(3)?,
        payload: row.get(4)?,
        block_number: row.get(5)?,
    })
}

fn convert_row_to_latest_batch_with_user_op_count(row: &Row<'_>) -> Result<(i64, i64, i64)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
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
        FrameHeaderRow, SQL_INSERT_SAFE_INPUT, SQL_INSERT_SEQUENCED_DIRECT_INPUT,
        SQL_INSERT_USER_OP, sql_insert_open_batch, sql_insert_open_batch_with_index,
        sql_insert_open_frame, sql_insert_safe_inputs_batch, sql_insert_sequenced_direct_inputs,
        sql_insert_user_ops_batch, sql_select_batch_policy, sql_select_frames_for_batch,
        sql_select_latest_batch_index, sql_select_latest_batch_with_user_op_count,
        sql_select_max_safe_input_index, sql_select_ordered_l2_tx_count,
        sql_select_ordered_l2_txs_from_offset, sql_select_ordered_l2_txs_page_from_offset,
        sql_select_safe_block, sql_select_safe_inputs_range,
        sql_select_total_drained_direct_inputs, sql_select_user_ops_for_frame,
        sql_update_batch_policy_alpha, sql_update_batch_policy_log_gas_price,
        sql_update_safe_block,
    };
    use crate::inclusion_lane::PendingUserOp;
    use crate::storage::db::Storage;
    use crate::storage::{SafeInputRange, StoredSafeInput};
    use alloy_primitives::{Address, Signature};
    use rusqlite::{Connection, params};
    use sequencer_core::user_op::{SignedUserOp, UserOp};
    use std::time::SystemTime;
    use tokio::sync::oneshot;

    fn setup_conn() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory sqlite");
        Storage::run_migrations(&mut conn).expect("run migrations");
        conn
    }

    fn sample_pending_user_op(seed: u8, nonce: u32, max_fee: u16) -> PendingUserOp {
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
            respond_to,
            received_at: SystemTime::now(),
        }
    }

    fn seed_open_batch0_frame0(conn: &mut Connection) {
        let tx = conn.transaction().expect("start tx");
        sql_insert_open_batch_with_index(&tx, 0, 123).expect("insert batch 0");
        sql_insert_open_frame(&tx, 0, 0, 123, 0, 0).expect("insert frame 0");
        tx.commit().expect("commit tx");
    }

    #[test]
    fn max_index_helpers_work_for_empty_and_non_empty_tables() {
        let mut conn = setup_conn();

        assert_eq!(
            sql_select_total_drained_direct_inputs(&conn).expect("total drained"),
            0
        );
        assert_eq!(
            sql_select_max_safe_input_index(&conn).expect("query max direct input"),
            None
        );

        conn.execute(
            SQL_INSERT_SAFE_INPUT,
            params![0_i64, vec![0x11_u8; 20], vec![0xaa_u8], 10_i64],
        )
        .expect("insert direct input 0");
        conn.execute(
            SQL_INSERT_SAFE_INPUT,
            params![1_i64, vec![0x22_u8; 20], vec![0xbb_u8], 11_i64],
        )
        .expect("insert direct input 1");
        assert_eq!(
            sql_select_max_safe_input_index(&conn).expect("query max direct input"),
            Some(1)
        );

        seed_open_batch0_frame0(&mut conn);
        let tx = conn.transaction().expect("start tx");
        tx.execute(
            SQL_INSERT_SEQUENCED_DIRECT_INPUT,
            params![0_i64, 0_i64, 0_i64],
        )
        .expect("insert sequenced direct input");
        tx.commit().expect("commit tx");

        assert_eq!(
            sql_select_total_drained_direct_inputs(&conn).expect("total drained"),
            1
        );

        let tx = conn.transaction().expect("start tx");
        assert_eq!(
            sql_select_max_safe_input_index(&tx).expect("query max direct input in tx"),
            Some(1)
        );
    }

    #[test]
    fn safe_inputs_range_is_half_open_and_ordered() {
        let conn = setup_conn();

        conn.execute(
            SQL_INSERT_SAFE_INPUT,
            params![0_i64, vec![0x11_u8; 20], vec![0xaa_u8], 10_i64],
        )
        .expect("insert direct input 0");
        conn.execute(
            SQL_INSERT_SAFE_INPUT,
            params![1_i64, vec![0x22_u8; 20], vec![0xbb_u8], 11_i64],
        )
        .expect("insert direct input 1");
        conn.execute(
            SQL_INSERT_SAFE_INPUT,
            params![2_i64, vec![0x33_u8; 20], vec![0xcc_u8], 12_i64],
        )
        .expect("insert direct input 2");

        let empty = sql_select_safe_inputs_range(&conn, 1, 1).expect("query empty interval");
        assert!(empty.is_empty());

        let rows = sql_select_safe_inputs_range(&conn, 0, 2).expect("query non-empty interval");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].safe_input_index, 0);
        assert_eq!(rows[1].safe_input_index, 1);
    }

    #[test]
    fn ordered_l2_query_follows_sequenced_offset_order() {
        let mut conn = setup_conn();
        seed_open_batch0_frame0(&mut conn);

        conn.execute(
            SQL_INSERT_USER_OP,
            params![
                0_i64,
                0_i64,
                0_i64,
                vec![0x20_u8; 20],
                0_i64,
                1_i64,
                vec![0x30_u8],
                vec![0x40_u8; 65],
                0_i64
            ],
        )
        .expect("insert user op");
        // The trg_sequence_user_op trigger automatically inserts the sequenced row.
        conn.execute(
            SQL_INSERT_SAFE_INPUT,
            params![0_i64, vec![0x11_u8; 20], vec![0xaa_u8], 10_i64],
        )
        .expect("insert direct input");
        conn.execute(
            SQL_INSERT_SEQUENCED_DIRECT_INPUT,
            params![0_i64, 0_i64, 0_i64],
        )
        .expect("insert sequenced direct input");

        let rows = sql_select_ordered_l2_txs_from_offset(&conn, 0).expect("query ordered l2");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, 0);
        assert_eq!(rows[0].fee, Some(0));
        assert_eq!(rows[1].kind, 1);
        assert_eq!(rows[1].fee, None);

        let paged = sql_select_ordered_l2_txs_page_from_offset(&conn, 1, 1).expect("query page");
        assert_eq!(paged.len(), 1);
        assert_eq!(paged[0].kind, 1);
        assert_eq!(
            sql_select_ordered_l2_tx_count(&conn).expect("query ordered count"),
            2
        );
    }

    #[test]
    fn batch_and_frame_helpers_start_empty_before_lane_initialization() {
        let mut conn = setup_conn();
        let tx = conn.transaction().expect("start tx");

        let err = sql_select_latest_batch_with_user_op_count(&tx).expect_err("no batch yet");
        assert!(matches!(err, rusqlite::Error::QueryReturnedNoRows));
    }

    #[test]
    fn latest_batch_index_and_frames_for_batch_helpers_work() {
        let mut conn = setup_conn();
        // No batches yet.
        assert_eq!(
            sql_select_latest_batch_index(&conn).expect("query latest batch nonce"),
            None
        );

        // Seed batch 0 / frame 0, then batch 1 / frame 0.
        seed_open_batch0_frame0(&mut conn);
        {
            let tx = conn.transaction().expect("start tx");
            sql_insert_open_batch(&tx, 456).expect("insert batch 1");
            let next_batch = tx.last_insert_rowid();
            sql_insert_open_frame(&tx, next_batch, 0, 456, 3, 5)
                .expect("insert frame 0 for batch 1");
            tx.commit().expect("commit tx");
        }

        let latest = sql_select_latest_batch_index(&conn)
            .expect("query latest batch nonce")
            .expect("latest batch should exist");
        assert_eq!(latest, 1);

        let frames = sql_select_frames_for_batch(&conn, 1).expect("query frames for batch 1");
        assert_eq!(frames.len(), 1);
        let FrameHeaderRow {
            frame_in_batch,
            fee,
            safe_block,
        } = frames[0].clone();
        assert_eq!(frame_in_batch, 0);
        assert_eq!(fee, 3);
        assert_eq!(safe_block, 5);
    }

    #[test]
    fn user_ops_for_frame_helper_returns_ordered_rows() {
        let mut conn = setup_conn();
        seed_open_batch0_frame0(&mut conn);

        // Insert two user-ops with different pos_in_frame values.
        conn.execute(
            SQL_INSERT_USER_OP,
            params![
                0_i64,
                0_i64,
                1_i64,
                vec![0x10_u8; 20],
                0_i64,
                1_i64,
                vec![0x01_u8],
                vec![0x55_u8; 65],
                0_i64
            ],
        )
        .expect("insert first user op");
        conn.execute(
            SQL_INSERT_USER_OP,
            params![
                0_i64,
                0_i64,
                0_i64,
                vec![0x20_u8; 20],
                1_i64,
                2_i64,
                vec![0x02_u8],
                vec![0x66_u8; 65],
                0_i64
            ],
        )
        .expect("insert second user op");

        let rows = sql_select_user_ops_for_frame(&conn, 0, 0).expect("query user ops for frame");
        assert_eq!(rows.len(), 2);
        // Ordered by pos_in_frame ASC: nonce 1 comes from pos 1, then nonce 0 from pos 0.
        assert_eq!(rows[0].nonce, 1);
        assert_eq!(rows[1].nonce, 0);
    }

    #[test]
    fn open_batch_and_frame_insert_helpers_work() {
        let mut conn = setup_conn();
        let tx = conn.transaction().expect("start tx");

        sql_insert_open_batch(&tx, 123).expect("insert open batch");
        let new_batch = tx.last_insert_rowid();
        sql_insert_open_frame(&tx, new_batch, 0, 123, 7, 9).expect("insert open frame");
        tx.commit().expect("commit tx");

        let batch_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM batches", [], |row| row.get(0))
            .expect("count batches");
        let frame_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM frames", [], |row| row.get(0))
            .expect("count frames");
        assert_eq!(batch_count, 1);
        assert_eq!(frame_count, 1);
    }

    #[test]
    fn batch_policy_helpers_read_defaults_and_update_knobs() {
        let conn = setup_conn();
        // Default: log_gas_price=0 → log_recommended_fee=0+20+419+621=1060
        // log_batch_size_target = 1403 - (-229) - 419 = 1213
        let (log_fee, log_target) = sql_select_batch_policy(&conn).expect("read policy");
        assert_eq!(log_fee, 20 + 419 + 621); // 1060
        assert_eq!(log_target, 1403 - (-229) - 419); // 1213

        sql_update_batch_policy_log_gas_price(&conn, 100).expect("update log gas price");
        let (log_fee, _) = sql_select_batch_policy(&conn).expect("read updated policy");
        assert_eq!(log_fee, 100 + 20 + 419 + 621); // 1160

        // Update alpha: num=200, denom=1000 → log_alpha=-207, log_one_plus_alpha=23
        // View derives: log_batch_size_target = 1403 - (-207) - 419 = 1191
        sql_update_batch_policy_alpha(&conn, -207, 23).expect("update alpha");
        let (log_fee, log_target) = sql_select_batch_policy(&conn).expect("read updated target");
        assert_eq!(log_target, 1403 - (-207) - 419); // 1191
        assert_eq!(log_fee, 100 + 23 + 419 + 621); // 1163
    }

    #[test]
    fn batch_policy_check_rejects_unsafe_alpha() {
        let conn = setup_conn();
        // log_alpha=-350 → log_batch_size_target = 1403-(-350)-419 = 1334 >= log_max_batch_bytes=1333
        let err = sql_update_batch_policy_alpha(&conn, -350, 0);
        assert!(
            err.is_err(),
            "CHECK should reject unsafe alpha (log_batch_size_target >= log_max_batch_bytes)"
        );
    }

    #[test]
    fn l1_safe_head_helpers_read_and_update_singleton() {
        let conn = setup_conn();
        assert_eq!(sql_select_safe_block(&conn).expect("read safe block"), 0);
        sql_update_safe_block(&conn, 12).expect("update safe block");
        assert_eq!(sql_select_safe_block(&conn).expect("read updated"), 12);
    }

    #[test]
    fn batch_insert_helpers_insert_multiple_rows() {
        let mut conn = setup_conn();
        seed_open_batch0_frame0(&mut conn);
        let tx = conn.transaction().expect("start tx");

        let safe_inputs = vec![
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa_u8],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb_u8],
                block_number: 11,
            },
        ];
        sql_insert_safe_inputs_batch(&tx, 0, safe_inputs.as_slice())
            .expect("insert direct inputs batch");

        let user_ops = vec![
            sample_pending_user_op(0x20, 0, 1),
            sample_pending_user_op(0x21, 1, 1),
        ];
        sql_insert_user_ops_batch(&tx, 0, 0, 0, user_ops.as_slice())
            .expect("insert user ops + sequenced batch");

        sql_insert_sequenced_direct_inputs(
            &tx,
            0,
            0,
            SafeInputRange::new(0, safe_inputs.len() as u64),
        )
        .expect("insert sequenced direct inputs batch");

        tx.commit().expect("commit tx");

        let direct_inputs_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM safe_inputs", [], |row| row.get(0))
            .expect("count direct inputs");
        let user_ops_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM user_ops", [], |row| row.get(0))
            .expect("count user ops");
        let sequenced_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sequenced_l2_txs", [], |row| {
                row.get(0)
            })
            .expect("count sequenced l2 txs");

        assert_eq!(direct_inputs_count, 2);
        assert_eq!(user_ops_count, 2);
        assert_eq!(sequenced_count, 4);
    }

    #[test]
    fn user_op_uniqueness_is_sender_nonce() {
        let mut conn = setup_conn();
        seed_open_batch0_frame0(&mut conn);

        // Same nonce with different senders should be accepted.
        conn.execute(
            SQL_INSERT_USER_OP,
            params![
                0_i64,
                0_i64,
                0_i64,
                vec![0x11_u8; 20],
                0_i64,
                0_i64,
                vec![0x01_u8],
                vec![0x55_u8; 65],
                0_i64
            ],
        )
        .expect("insert first user op");
        conn.execute(
            SQL_INSERT_USER_OP,
            params![
                0_i64,
                0_i64,
                1_i64,
                vec![0x22_u8; 20],
                0_i64,
                0_i64,
                vec![0x02_u8],
                vec![0x66_u8; 65],
                0_i64
            ],
        )
        .expect("insert second user op with same nonce and different sender");

        // Same sender + nonce should violate uniqueness.
        let duplicate_sender_nonce = conn.execute(
            SQL_INSERT_USER_OP,
            params![
                0_i64,
                0_i64,
                2_i64,
                vec![0x11_u8; 20],
                0_i64,
                0_i64,
                vec![0x03_u8],
                vec![0x77_u8; 65],
                0_i64
            ],
        );
        assert!(
            duplicate_sender_nonce.is_err(),
            "duplicate (sender, nonce) should fail"
        );
    }
}
