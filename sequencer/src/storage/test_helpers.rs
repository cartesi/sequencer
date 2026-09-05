// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared test fixtures used by `#[cfg(test)]` modules in `storage/`.

use alloy_primitives::Address;
use sequencer_core::l2_tx::SequencedL2Tx;
use sequencer_core::protocol::ProtocolTiming;
use tempfile::TempDir;

use super::{DeploymentIdentity, FeeOracleIdentity, SafeInputRange, Storage, StoredSafeInput};

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

pub(crate) struct TestDb {
    pub _dir: TempDir,
    pub path: String,
}

pub(crate) fn temp_db(name: &str) -> TestDb {
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

/// Pin the trusted deployment identity a production setup establishes before
/// any fresh-Tip attribution can classify safe inputs.
pub(crate) fn pin_test_deployment_identity(
    storage: &mut Storage,
    batch_submitter_address: Address,
) {
    storage
        .load_or_insert_deployment_identity(DeploymentIdentity {
            chain_id: 1,
            app_address: Address::repeat_byte(0x11),
            input_box_address: Address::repeat_byte(0x22),
            app_deployment_block: 0,
            batch_submitter_address,
            fee_oracle: FeeOracleIdentity::Fixed { log_gas_price: 0 },
        })
        .expect("pin test deployment identity");
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
    storage
        .ordered_l2_txs_page_from(0, 1_000_000)
        .expect("load all ordered l2 txs")
        .into_iter()
        .map(|row| row.tx)
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
