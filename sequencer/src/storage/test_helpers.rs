// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Storage-domain test fixtures: seeding, the recovery fixture trait and
//! divergence markers. Crate-wide fixtures live in `crate::test_support`.

use alloy_primitives::Address;
use sequencer_core::l2_tx::SequencedL2Tx;
use sequencer_core::protocol::ProtocolTiming;

use super::convert::from_unix_ms;
use super::ingress::{
    insert_tip_rows, open_fresh_tip_in_tx, open_successor_frame_in, seal_and_open_next_batch,
};
use super::mutations::persist_frame_direct_sequence_derived;
use super::queries::{load_current_write_head, query_batch_policy};
use super::{BatchPolicy, SafeInputRange, Storage, StoredSafeInput, WriteHead};
use crate::test_support::{TestDb, pin_test_deployment_identity, temp_db};

pub(crate) const SENDER_A: Address = Address::repeat_byte(0xAA);
pub(crate) const SENDER_B: Address = Address::repeat_byte(0xBB);

/// Default protocol timing for tests that don't care about specific tuning.
/// Sender-independent (timing carries no address); pair with a submitter
/// address as needed at the call site.
pub(crate) fn default_protocol_timing() -> ProtocolTiming {
    ProtocolTiming {
        max_wait_blocks: sequencer_core::MAX_WAIT_BLOCKS,
        preemptive_margin_blocks: 75,
        l1_read_stale_after_blocks: 900,
        seconds_per_block: 12,
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
            let batch_index = insert_tip_rows(tx, Some(0), None, safe_block)?;
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
    pub(crate) fn close_frame_and_batch(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
    ) -> rusqlite::Result<()> {
        let (next_batch_index, now_ms, policy) =
            self.write(|tx| seal_and_open_next_batch(tx, head.batch_index, next_safe_block))?;
        head.move_to_next_batch(
            next_batch_index,
            from_unix_ms(now_ms),
            policy,
            next_safe_block,
        );
        Ok(())
    }

    pub(crate) fn batch_policy(&mut self) -> rusqlite::Result<BatchPolicy> {
        query_batch_policy(&self.conn)
    }

    /// The batch-tree anchor nonce (default 0).
    pub(crate) fn batch_tree_anchor(&mut self) -> rusqlite::Result<u64> {
        self.read(|tx| super::mutations::batch_tree_anchor_in(tx))
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

/// Recovery-storage fixture with the same prerequisite ordering as setup:
/// baseline schema first, then the persisted deployment identity, then any L1
/// facts or batch-tree mutations driven by the individual test.
pub(crate) fn temp_db_with_default_deployment_identity(name: &str) -> TestDb {
    let db = temp_db(name);
    let mut storage = Storage::open(db.path.as_str()).expect("open test storage");
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
/// batch-submitter address passed to `append_safe_inputs`, so the populated
/// `safe_accepted_batches` view matches this sender.
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
        let safe_block = head.safe_block;
        storage
            .close_frame_and_batch(&mut head, safe_block)
            .expect("close batch");
    }
}

/// Pull every valid sequenced L2 tx out of storage, dropping the offset.
/// Test-only convenience around `ordered_l2_txs_page_from`.
pub(crate) fn all_ordered_l2_txs(storage: &mut Storage) -> Vec<SequencedL2Tx> {
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
            super::egress::L2TxContext::UserOp { tx, .. } => SequencedL2Tx::UserOp(tx),
            super::egress::L2TxContext::DirectInput { tx, .. } => SequencedL2Tx::Direct(tx),
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
        self.close_frame_and_batch(head, safe_block)
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

/// A storage over a fresh pinned database. Bind the [`TestDb`] as `_db`, never
/// `_`: dropping it deletes the directory under the open connection.
pub(crate) fn open_temp_storage(name: &str) -> (TestDb, Storage) {
    let db = temp_db_with_default_deployment_identity(name);
    let storage = Storage::open(db.path.as_str()).expect("open storage");
    (db, storage)
}
