// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Storage-domain test fixtures: seeding, the recovery fixture trait and
//! divergence markers. Crate-wide fixtures live in `crate::test_support`.

use std::path::Path;

use alloy_primitives::{Address, B256};
use app_core::application::WALLET_SCHEDULER_LIMITS;
use rusqlite::params;
use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};
use sequencer_core::protocol::ProtocolTiming;

use super::convert::{i64_to_u16, i64_to_u32, i64_to_u64, now_unix_ms, u64_to_i64};
use super::history::next_l2_tx_count_in;
use super::ingress::{
    insert_tip_rows, open_fresh_tip_in_tx, open_successor_frame_in, seal_and_open_next_batch,
};
use super::mutations::persist_frame_direct_sequence_derived;
use super::queries::{load_current_write_head, query_batch_policy};
use super::recovery::{recover_aging_tip_inner, recover_post_flush_inner};
use super::snapshots::{insert_baseline_snapshot_in, insert_batch_snapshot_in};
use super::{
    BatchId, BatchPolicy, IngestedSafeInput, L2TxCount, LifecycleCommand, NewEra, SafeInputRange,
    Storage, StorageOpenError, WriteHead,
};
use crate::test_support::{TestDb, pin_test_deployment_identity, temp_db};

/// One safe input as stored on the L1 InputBox: sender, opaque payload, and
/// the L1 block where it was included. Test fixtures seed inputs with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredSafeInput {
    pub sender: Address,
    pub payload: Vec<u8>,
    /// Chain block number where this input was included (e.g. InputAdded event block).
    pub block_number: u64,
}

impl NewEra {
    /// An era at genesis that rebuilt nothing, with stop block 0 and anchor 0:
    /// what [`set_up_storage`], the crate-test open path and seeded harnesses
    /// publish.
    pub(crate) const EMPTY_GENESIS: Self = Self {
        base: L2TxCount::ZERO,
        rebuilt_end: L2TxCount::ZERO,
        stop_block: 0,
        first_queued_input: 0,
        anchor_batch_nonce: 0,
    };
}

impl BatchId {
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// For tests outside storage that read the database with raw SQL.
    pub(crate) fn to_raw(self) -> u64 {
        self.0
    }
}

impl SafeInputRange {
    pub(crate) fn empty_at(index: u64) -> Self {
        Self::new(index, index)
    }
}

impl WriteHead {
    pub(crate) fn frame_in_batch(&self) -> u32 {
        self.frame_in_batch
    }

    pub(crate) fn open_frame_user_op_count(&self) -> u32 {
        self.open_frame_user_op_count
    }
}

/// A database in the state `setup` leaves it, built through setup's own
/// calls: the schema, the test deployment identity with `batch_submitter`,
/// and the genesis era published with its baseline snapshot (a `baseline`
/// prefix beside the database file). A test that needs no identity or no
/// baseline yet opens through `Storage::open_with_era` instead; one that
/// needs no era, through `Storage::initialize_for_command`.
pub(crate) fn set_up_storage(path: &str, batch_submitter: Address) -> Storage {
    let mut storage =
        Storage::initialize_for_command(path, LifecycleCommand::Setup).expect("create schema");
    pin_test_deployment_identity(&mut storage, batch_submitter);
    storage
        .publish_era(
            &Path::new(path).with_file_name("baseline"),
            &NewEra::EMPTY_GENESIS,
        )
        .expect("publish the genesis era");
    storage
}

/// Batch ids from raw indices, to compare against a cascade's invalidated list.
pub(crate) fn batch_ids(raw: &[u64]) -> Vec<BatchId> {
    raw.iter().copied().map(BatchId::from_raw).collect()
}

pub(crate) const SENDER_A: Address = Address::repeat_byte(0xAA);
pub(crate) const SENDER_B: Address = Address::repeat_byte(0xBB);

/// The direct payload cap every test identity pins: the wallet's, which
/// `StubApp` shares, so fixtures and either test lane drain the same directs.
pub(crate) const TEST_MAX_DIRECT_PAYLOAD_BYTES: u64 =
    WALLET_SCHEDULER_LIMITS.max_direct_payload_bytes;

/// An era with nothing queued and anchor 0; fixtures override fields.
pub(crate) fn era(base: u64, rebuilt_end: u64, stop_block: u64) -> super::NewEra {
    super::NewEra {
        base: L2TxCount::new(base),
        rebuilt_end: L2TxCount::new(rebuilt_end),
        stop_block,
        first_queued_input: 0,
        anchor_batch_nonce: 0,
    }
}

/// Default protocol timing for tests that don't care about specific tuning.
/// Sender-independent (timing carries no address); pair with a submitter
/// address as needed at the call site.
pub(crate) fn default_protocol_timing() -> ProtocolTiming {
    ProtocolTiming::try_new(sequencer_core::MAX_WAIT_BLOCKS, 75, 900, 12)
        .expect("valid test timing")
}

/// Every field of a [`WriteHead`], for the few tests that build one by hand:
/// the lane's pure dequeue tests, the counter-overflow tests and the stale-head
/// tests. Production heads come only from storage's loader.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WriteHeadParts {
    pub batch_index: u64,
    pub batch_nonce: u64,
    pub batch_created_at: std::time::SystemTime,
    pub frame_fee: u16,
    pub safe_block: u64,
    pub batch_user_op_count: u64,
    pub open_frame_user_op_count: u32,
    pub frame_in_batch: u32,
    pub max_batch_user_op_bytes: u64,
}

impl WriteHeadParts {
    /// Batch 0, nonce 0, frame 0, everything zero, a one-byte size target.
    pub(crate) const ZERO: Self = Self {
        batch_index: 0,
        batch_nonce: 0,
        batch_created_at: std::time::UNIX_EPOCH,
        frame_fee: 0,
        safe_block: 0,
        batch_user_op_count: 0,
        open_frame_user_op_count: 0,
        frame_in_batch: 0,
        max_batch_user_op_bytes: 1,
    };

    pub(crate) fn of(head: &WriteHead) -> Self {
        Self {
            batch_index: head.batch_index,
            batch_nonce: head.batch_nonce,
            batch_created_at: head.batch_created_at,
            frame_fee: head.frame_fee,
            safe_block: head.safe_block,
            batch_user_op_count: head.batch_user_op_count,
            open_frame_user_op_count: head.open_frame_user_op_count,
            frame_in_batch: head.frame_in_batch,
            max_batch_user_op_bytes: head.max_batch_user_op_bytes,
        }
    }

    pub(crate) fn build(self) -> WriteHead {
        WriteHead {
            batch_index: self.batch_index,
            batch_nonce: self.batch_nonce,
            batch_created_at: self.batch_created_at,
            frame_fee: self.frame_fee,
            safe_block: self.safe_block,
            batch_user_op_count: self.batch_user_op_count,
            open_frame_user_op_count: self.open_frame_user_op_count,
            frame_in_batch: self.frame_in_batch,
            max_batch_user_op_bytes: self.max_batch_user_op_bytes,
        }
    }
}

/// Lane fixtures. Production opens the Tip through
/// `ensure_open_tip_for_recovery` and closes frames and batches with
/// attributed executions and a snapshot (I7).
impl Storage {
    /// Open the genesis Tip with an explicit leading range; asserts none exists.
    pub(crate) fn initialize_open_state(
        &mut self,
        safe_block: u64,
        leading_direct_range: SafeInputRange,
    ) -> rusqlite::Result<WriteHead> {
        self.write(|tx| {
            assert!(
                load_current_write_head(tx)?.is_none(),
                "open state already exists"
            );
            let batch_index = insert_tip_rows(tx, Some(0), safe_block)?;
            persist_frame_direct_sequence_derived(tx, batch_index, 0, leading_direct_range)?;
            Ok(load_current_write_head(tx)?.expect("genesis tip just inserted"))
        })
    }

    /// Open the genesis Tip at the observed safe head unless one exists; the
    /// unguarded twin of `ensure_open_tip_for_recovery`.
    pub(crate) fn ensure_open_tip(&mut self) -> rusqlite::Result<()> {
        self.write(|tx| {
            if load_current_write_head(tx)?.is_some() {
                return Ok(());
            }
            open_fresh_tip_in_tx(tx)
        })
    }

    /// Rotate the frame with derived application offsets.
    pub(crate) fn close_frame_only(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        leading_direct_range: SafeInputRange,
    ) -> rusqlite::Result<()> {
        let policy = self
            .write(|tx| close_frame_derived_in(tx, head, next_safe_block, leading_direct_range))?;
        head.advance_frame(policy, next_safe_block);
        Ok(())
    }

    /// Close the batch without registering a snapshot.
    pub(crate) fn close_frame_and_batch(&mut self, head: &mut WriteHead) -> rusqlite::Result<()> {
        let next = self.write(|tx| seal_and_open_next_batch(tx, head))?;
        next.apply_to(head);
        Ok(())
    }

    pub(crate) fn batch_policy(&mut self) -> rusqlite::Result<BatchPolicy> {
        query_batch_policy(&self.conn)
    }

    /// The published era; errors before setup publishes one.
    pub(crate) fn era(&mut self) -> rusqlite::Result<super::history::Era> {
        self.read(|tx| super::history::era_in(tx))
    }

    /// The era id and the recovery generation (the greatest cut).
    pub(crate) fn history_version(
        &mut self,
    ) -> rusqlite::Result<sequencer_core::history::HistoryVersion> {
        self.read(|tx| super::history::history_version_in(tx))
    }
}

/// Reads only tests make. The danger check reads the safe-head columns
/// through the `queries` functions inside its own transaction.
impl Storage {
    /// Unix-ms of the last successful `log_gas_price` write. `0` means never
    /// written (migration default); completed setup guarantees a nonzero value.
    pub(crate) fn log_gas_price_updated_at_ms(&self) -> rusqlite::Result<u64> {
        self.conn.query_row(
            "SELECT log_gas_price_updated_at_ms FROM batch_policy WHERE singleton_id = 0",
            [],
            |row| Ok(i64_to_u64(row.get::<_, i64>(0)?)),
        )
    }

    /// L1 timestamp (Unix seconds) of the observed safe block, or `None`
    /// before the first safe-head observation.
    pub(crate) fn current_safe_block_timestamp(&mut self) -> rusqlite::Result<Option<u64>> {
        super::queries::current_safe_block_timestamp(&self.conn)
    }

    /// Wall-clock timestamp (Unix ms) of the last observed safe-head advance,
    /// or `None` before the first safe-head observation.
    pub(crate) fn last_safe_progress_ms(&self) -> rusqlite::Result<Option<u64>> {
        super::queries::last_safe_progress_ms(&self.conn)
    }
}

/// Seeding and inspection levers. Each bypasses the guard, provenance or
/// wall clock its production counterpart enforces.
impl Storage {
    /// A fresh database with `era`: setup's schema, then the era row through
    /// `publish_era`'s insert, with no identity and no baseline snapshot
    /// (callers that need either add it). For fixtures whose era is not the
    /// genesis one, or that need no identity or no baseline yet.
    pub(crate) fn open_with_era(path: &str, era: &super::NewEra) -> Storage {
        let mut storage =
            Storage::initialize_for_command(path, super::LifecycleCommand::Setup).expect("schema");
        storage
            .write(|tx| super::history::insert_era_in(tx, era))
            .expect("insert era");
        storage
    }

    pub(crate) fn next_l2_tx_count(&mut self) -> rusqlite::Result<L2TxCount> {
        self.read(|tx| next_l2_tx_count_in(tx))
    }

    pub(crate) fn has_rollback_safe_snapshot(&mut self) -> rusqlite::Result<bool> {
        self.read(|tx| super::snapshots::has_rollback_safe_snapshot_in(tx))
    }

    /// [`Storage::read`]'s deferred transaction, for tests outside storage
    /// that read raw SQL.
    pub(crate) fn raw_read<T>(
        &mut self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        self.read(f)
    }

    /// [`Storage::write`]'s immediate transaction, for tests outside storage
    /// that write raw SQL.
    pub(crate) fn raw_write<T>(
        &mut self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        self.write(f)
    }

    /// A raw connection with [`Storage::open`]'s pragmas, for tests that reach
    /// past the typed API (rewinding `synced_at_ms`, installing failure
    /// triggers).
    pub(crate) fn open_connection(path: &str) -> Result<rusqlite::Connection, StorageOpenError> {
        super::open::open_writer_connection(path)
    }

    /// Append safe inputs at the current wall clock, with zero provenance.
    /// See [`Storage::append_safe_inputs_with_timestamp`] for `batch_submitter`.
    pub(crate) fn append_safe_inputs(
        &mut self,
        safe_block: u64,
        inputs: &[StoredSafeInput],
        batch_submitter: Address,
        timing: &ProtocolTiming,
    ) -> rusqlite::Result<()> {
        self.append_safe_inputs_with_timestamp(
            safe_block,
            i64_to_u64(now_unix_ms()) / 1000,
            inputs,
            batch_submitter,
            timing,
        )
    }

    /// [`Storage::append_safe_inputs`] at an explicit L1 timestamp of
    /// `safe_block`. Stands in for setup's identity pin: on an unpinned
    /// database it pins a test identity whose submitter is `batch_submitter`;
    /// on a pinned one it asserts the pinned submitter is `batch_submitter`.
    /// Storage reads the submitter from the pin, never from this argument.
    pub(crate) fn append_safe_inputs_with_timestamp(
        &mut self,
        safe_block: u64,
        safe_block_timestamp: u64,
        inputs: &[StoredSafeInput],
        batch_submitter: Address,
        timing: &ProtocolTiming,
    ) -> rusqlite::Result<()> {
        match self.deployment_identity()? {
            Some(identity) => assert_eq!(
                identity.batch_submitter_address, batch_submitter,
                "the fixture's submitter differs from the pinned identity's"
            ),
            None => pin_test_deployment_identity(self, batch_submitter),
        }
        let rows: Vec<IngestedSafeInput> = inputs.iter().map(synthetic_row).collect();
        self.append_ingested_safe_inputs_with_timestamp(
            safe_block,
            safe_block_timestamp,
            rows.as_slice(),
            timing,
        )
    }

    /// Highest valid `batch_index` (the open batch included), or `None`.
    pub(crate) fn latest_batch_index(&mut self) -> rusqlite::Result<Option<u64>> {
        let value: Option<i64> =
            self.conn
                .query_row("SELECT MAX(batch_index) FROM valid_batches", [], |row| {
                    row.get(0)
                })?;
        Ok(value.map(i64_to_u64))
    }

    /// The sequenced L2 txs of one batch; `[]` for an invalidated batch.
    /// Production replay pages the valid stream by offset, never per batch.
    pub(crate) fn ordered_l2_txs_for_batch(
        &mut self,
        batch_index: u64,
    ) -> rusqlite::Result<Vec<L2Tx>> {
        const SQL: &str = "
            SELECT
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN 0 ELSE 1 END AS kind,
                CASE
                    WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.sender
                    WHEN s.safe_input_index IS NOT NULL THEN d.sender
                    ELSE NULL
                END AS sender,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.data  ELSE NULL END AS data,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN f.fee   ELSE NULL END AS fee,
                CASE WHEN s.user_op_pos_in_frame IS NOT NULL THEN u.nonce ELSE NULL END AS nonce,
                f.safe_block,
                s.safe_input_index,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.payload         ELSE NULL END AS payload,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.block_number    ELSE NULL END AS block_number,
                CASE WHEN s.safe_input_index IS NOT NULL THEN d.block_timestamp ELSE NULL END AS block_timestamp
            FROM application_inputs s
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
            ORDER BY s.l2_tx_count ASC
        ";
        let mut stmt = self.conn.prepare_cached(SQL)?;
        let rows = stmt.query_map(params![u64_to_i64(batch_index)], decode_l2_tx_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    }

    /// Mark one batch invalid; leaves an already-invalid row alone.
    pub(crate) fn insert_invalid_batch(&mut self, batch_index: u64) -> rusqlite::Result<()> {
        let now_ms = now_unix_ms();
        self.conn.execute(
            "UPDATE batches SET invalidated_at_ms = ?1 \
             WHERE batch_index = ?2 AND invalidated_at_ms IS NULL",
            params![now_ms, u64_to_i64(batch_index)],
        )?;
        Ok(())
    }

    /// The unguarded twin of `recover_post_flush_for_recovery`.
    pub(crate) fn recover_post_flush(
        &mut self,
        danger_threshold: u64,
    ) -> rusqlite::Result<Vec<BatchId>> {
        self.write(|tx| recover_post_flush_inner(tx, danger_threshold))
    }

    /// The unguarded twin of `recover_aging_tip_for_recovery`.
    pub(crate) fn recover_aging_tip(
        &mut self,
        danger_threshold: u64,
    ) -> rusqlite::Result<Vec<BatchId>> {
        self.write(|tx| recover_aging_tip_inner(tx, danger_threshold))
    }

    pub(crate) fn insert_batch_snapshot(
        &mut self,
        prefix: &Path,
        batch_index: u64,
    ) -> rusqlite::Result<i64> {
        self.write(|tx| insert_batch_snapshot_in(tx, prefix, batch_index, next_l2_tx_count_in(tx)?))
    }

    /// Close the open batch (the genesis Tip when none is open) and register
    /// `prefix` as its snapshot: the fixture for a non-baseline row, since a
    /// second baseline is refused. Returns the snapshot id and batch index.
    pub(crate) fn insert_closed_batch_snapshot(
        &mut self,
        prefix: &Path,
    ) -> rusqlite::Result<(i64, u64)> {
        let mut head = match self.open_state()? {
            Some(head) => head,
            None => self.initialize_open_state(0, SafeInputRange::empty_at(0))?,
        };
        let batch_index = head.batch_index;
        self.close_frame_and_batch(&mut head)?;
        Ok((
            self.insert_batch_snapshot(prefix, batch_index)?,
            batch_index,
        ))
    }

    pub(crate) fn insert_baseline_snapshot(
        &mut self,
        prefix: &Path,
        count: L2TxCount,
    ) -> rusqlite::Result<i64> {
        self.write(|tx| insert_baseline_snapshot_in(tx, prefix, count))
    }

    /// Pin the oracle's observation stamp instead of the wall clock.
    pub(crate) fn set_log_gas_price_updated_at_ms_for_test(
        &mut self,
        updated_at_ms: u64,
    ) -> rusqlite::Result<()> {
        let changed = self.conn.execute(
            "UPDATE batch_policy SET log_gas_price_updated_at_ms = ?1 WHERE singleton_id = 0",
            params![u64_to_i64(updated_at_ms)],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed));
        }
        Ok(())
    }
}

/// A provenance-free [`StoredSafeInput`] as a row with explicit zero
/// provenance.
fn synthetic_row(input: &StoredSafeInput) -> IngestedSafeInput {
    IngestedSafeInput {
        sender: input.sender,
        payload: input.payload.clone(),
        block_number: input.block_number,
        block_timestamp: 0,
        transaction_hash: B256::ZERO,
    }
}

/// One row of [`Storage::ordered_l2_txs_for_batch`], built the way the
/// canonical history page builds its rows: a user op carries its frame's fee
/// and safe block, a direct its L1 index, block and timestamp.
fn decode_l2_tx_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<L2Tx> {
    let sender_bytes: Vec<u8> = row
        .get::<_, Option<Vec<u8>>>(1)?
        .expect("ordered replay row: missing sender");
    assert_eq!(
        sender_bytes.len(),
        20,
        "ordered replay row: sender must be 20 bytes"
    );
    let sender = Address::from_slice(sender_bytes.as_slice());
    let required = |index: usize, column: &str| -> rusqlite::Result<i64> {
        Ok(row
            .get::<_, Option<i64>>(index)?
            .unwrap_or_else(|| panic!("ordered replay row: missing {column}")))
    };
    if row.get::<_, i64>(0)? == 0 {
        Ok(L2Tx::UserOp(UserOpTx {
            sender,
            nonce: i64_to_u32(required(4, "nonce")?),
            // The persisted frame fee (log-space exponent), as canonical execution sees it.
            fee: i64_to_u16(required(3, "fee")?),
            block: i64_to_u64(required(5, "safe_block")?),
            data: row
                .get::<_, Option<Vec<u8>>>(2)?
                .expect("ordered replay row: missing data"),
        }))
    } else {
        Ok(L2Tx::Direct(Direct {
            input_index: i64_to_u64(required(6, "safe_input_index")?),
            sender,
            block: i64_to_u64(required(8, "block_number")?),
            timestamp: i64_to_u64(required(9, "block_timestamp")?),
            payload: row
                .get::<_, Option<Vec<u8>>>(7)?
                .expect("ordered replay row: missing payload"),
        }))
    }
}

/// Alpha 17 (17000/1000): `log_alpha` 364 = round(log(17)) and
/// `log_one_plus_alpha` 371 = round(log(18)), base 129/128. The batch-size
/// target becomes 1403 - 364 - 419 = 620 (about 124 bytes), below one 126-byte
/// user op, so every op closes its batch.
pub(crate) fn set_one_op_batch_target(storage: &mut Storage) {
    storage
        .conn
        .execute(
            "UPDATE batch_policy SET log_alpha = 364, log_one_plus_alpha = 371 \
             WHERE singleton_id = 0",
            [],
        )
        .expect("set alpha");
}

fn close_frame_derived_in(
    tx: &rusqlite::Transaction<'_>,
    head: &WriteHead,
    next_safe_block: u64,
    leading_direct_range: SafeInputRange,
) -> rusqlite::Result<BatchPolicy> {
    let (policy, next_frame_in_batch) = open_successor_frame_in(tx, head, next_safe_block)?;
    persist_frame_direct_sequence_derived(
        tx,
        head.batch_index,
        next_frame_in_batch,
        leading_direct_range,
    )?;
    Ok(policy)
}

/// A fresh database with the genesis era and the test identity
/// ([`SENDER_A`]) but no baseline snapshot, for recovery tests that register
/// their own snapshots or test their absence.
pub(crate) fn temp_db_with_default_deployment_identity(name: &str) -> TestDb {
    let db = temp_db(name);
    let mut storage = Storage::open_with_era(&db.path, &NewEra::EMPTY_GENESIS);
    pin_test_deployment_identity(&mut storage, SENDER_A);
    db
}

/// Wire bytes of the local valid closed batch at `nonce` — the
/// production-faithful "our batch landed on L1" payload. Hash-matches the
/// seal-time stamp, so the content-identity check accepts it.
/// Panics if no valid closed local batch carries `nonce` (test bug: an
/// accepted landing without a matching local batch is, by design, a
/// canonical divergence).
pub(crate) fn local_batch_payload(storage: &mut Storage, nonce: u64) -> Vec<u8> {
    storage
        .pending_batches(nonce)
        .expect("load local closed batches")
        .into_iter()
        .find(|b| b.nonce == nonce)
        .unwrap_or_else(|| panic!("no valid closed local batch at nonce {nonce}"))
        .encoded
}

/// Insert safe inputs whose payloads are SSZ-encoded batches with the given
/// nonces, all attributed to `sender`. `sender` doubles as the
/// batch-submitter address passed to `append_safe_inputs`, so the recorded
/// acceptances follow this sender.
///
/// Nonces with a valid closed local batch land with that batch's real wire
/// bytes (the landing will be *accepted* — the content-identity check
/// requires byte equality with the seal-time hash). Nonces without one get
/// a synthetic empty-batch payload — sound **only** for landings the
/// acceptance fold rejects (e.g. past a nonce gap); a synthetic landing
/// that would be accepted trips the divergence marker and freezes the
/// frontier.
pub(crate) fn seed_safe_inputs_with_batch_nonces(
    storage: &mut Storage,
    sender: Address,
    safe_block: u64,
    nonces: &[u64],
) {
    let inputs: Vec<StoredSafeInput> = nonces
        .iter()
        .map(|nonce| {
            let payload = storage
                .pending_batches(*nonce)
                .expect("load local closed batches")
                .into_iter()
                .find(|b| b.nonce == *nonce)
                .map(|b| b.encoded)
                .unwrap_or_else(|| {
                    ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
                        nonce: *nonce,
                        frames: Vec::new(),
                    })
                });
            StoredSafeInput {
                sender,
                payload,
                block_number: safe_block,
            }
        })
        .collect();
    storage
        .append_safe_inputs(
            safe_block,
            inputs.as_slice(),
            sender,
            &default_protocol_timing(),
        )
        .expect("append safe inputs");
}

/// Create N closed batches (batch indices `0..count-1`) plus one open batch (index `count`).
pub(crate) fn seed_closed_batches(storage: &mut Storage, count: u64) {
    let mut head = storage
        .initialize_open_state(0, SafeInputRange::empty_at(0))
        .expect("initialize open state");
    for _ in 0..count {
        storage
            .close_frame_and_batch(&mut head)
            .expect("close batch");
    }
}

/// Pull every valid sequenced L2 tx out of storage, dropping the offset.
/// Test-only convenience around `canonical_history_page`.
pub(crate) fn all_ordered_l2_txs(storage: &mut Storage) -> Vec<L2Tx> {
    let bounds = storage.history_bounds().expect("history bounds");
    storage
        .canonical_history_page(
            sequencer_core::history::HistoryClaim {
                version: bounds.version,
                next_input: bounds.available_from,
            },
            1_000_000,
        )
        .expect("all application inputs")
        .rows
        .into_iter()
        .map(|row| match row.context {
            super::L2TxContext::UserOp { tx, .. } => L2Tx::UserOp(tx),
            super::L2TxContext::Direct { tx, .. } => L2Tx::Direct(tx),
        })
        .collect()
}

/// SSZ-encoded single-frame batch payload at the given (nonce, safe_block).
pub(crate) fn make_stale_batch_payload(nonce: u64, safe_block: u64) -> Vec<u8> {
    ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch {
        nonce,
        frames: vec![sequencer_core::batch::Frame {
            safe_block,
            fee_price: 0,
            user_ops: vec![],
        }],
    })
}

/// Plant the canonical-divergence marker. Test-only lever for
/// I15 scenarios; production writes it only inside the content-identity
/// check's sync transaction.
pub(crate) fn record_canonical_divergence(
    storage: &mut Storage,
    nonce: u64,
    safe_input_index: u64,
) {
    storage
        .conn
        .execute(
            "INSERT INTO canonical_divergence \
             (singleton_id, nonce, safe_input_index, kind, detected_at_ms) \
             VALUES (0, ?1, ?2, 'mismatch', 0)",
            [
                i64::try_from(nonce).unwrap(),
                i64::try_from(safe_input_index).unwrap(),
            ],
        )
        .expect("record canonical divergence");
}

/// Recovery-test plumbing over [`Storage`]. `land` and `advance_safe_head`
/// append through the production path with [`SENDER_A`] as the submitter and
/// default timing; every input lands at `safe_block`.
pub(crate) trait RecoveryFixture {
    /// Exercise the same frame-advance-before-close sequence as the live lane.
    fn close_batch_at(
        &mut self,
        head: &mut crate::storage::WriteHead,
        safe_block: u64,
    ) -> rusqlite::Result<()>;
    fn land(&mut self, safe_block: u64, inputs: &[(Address, Vec<u8>)]);
    fn advance_safe_head(&mut self, safe_block: u64);
}
impl RecoveryFixture for Storage {
    fn close_batch_at(
        &mut self,
        head: &mut crate::storage::WriteHead,
        safe_block: u64,
    ) -> rusqlite::Result<()> {
        if head.safe_block != safe_block {
            let start = self.next_undrained_safe_input_index()?;
            let end: i64 = self.read(|tx| {
                tx.query_row(
                    "SELECT COUNT(*) FROM safe_inputs WHERE block_number <= ?1",
                    [i64::try_from(safe_block).unwrap()],
                    |r| r.get(0),
                )
            })?;
            self.close_frame_only(
                head,
                safe_block,
                SafeInputRange::new(start, u64::try_from(end).unwrap()),
            )?;
        }
        self.close_frame_and_batch(head)
    }

    #[track_caller]
    fn land(&mut self, safe_block: u64, inputs: &[(Address, Vec<u8>)]) {
        let inputs: Vec<StoredSafeInput> = inputs
            .iter()
            .map(|(sender, payload)| StoredSafeInput {
                sender: *sender,
                payload: payload.clone(),
                block_number: safe_block,
            })
            .collect();
        self.append_safe_inputs(safe_block, &inputs, SENDER_A, &default_protocol_timing())
            .expect("append safe inputs");
    }

    #[track_caller]
    fn advance_safe_head(&mut self, safe_block: u64) {
        self.land(safe_block, &[]);
    }
}

/// A storage over a fresh set-up database ([`set_up_storage`], submitter
/// [`SENDER_A`]). Bind the [`TestDb`] as `_db`, never `_`: dropping it
/// deletes the directory under the open connection.
pub(crate) fn open_temp_storage(name: &str) -> (TestDb, Storage) {
    let db = temp_db(name);
    let storage = set_up_storage(&db.path, SENDER_A);
    (db, storage)
}
