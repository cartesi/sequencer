// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared test fixtures used by `#[cfg(test)]` modules in `storage/`.

use alloy_primitives::Address;
use sequencer_core::l2_tx::SequencedL2Tx;
use sequencer_core::protocol::ProtocolConfig;
use tempfile::TempDir;

use super::{SafeInputRange, Storage, StoredSafeInput};

pub(crate) const SENDER_A: Address = Address::repeat_byte(0xAA);
pub(crate) const SENDER_B: Address = Address::repeat_byte(0xBB);

/// Default protocol config for tests that don't care about the specific
/// submitter address or margin. Uses `SENDER_A` as the submitter.
pub(crate) fn default_protocol_config() -> ProtocolConfig {
    protocol_config_for(SENDER_A)
}

/// Protocol config with a specific submitter address and the default
/// `MAX_WAIT_BLOCKS`. Common test shape: seed via this sender, assert against
/// it. For explicit `max_wait_blocks` tuning build `ProtocolConfig` directly.
pub(crate) fn protocol_config_for(sender: Address) -> ProtocolConfig {
    ProtocolConfig {
        batch_submitter: sender,
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

/// Insert safe inputs whose payloads are SSZ-encoded batches with the given nonces,
/// all attributed to `sender`. Uses `protocol_config_for(sender)` so the
/// populated `safe_accepted_batches` view matches this sender.
pub(crate) fn seed_safe_inputs_with_batch_nonces(
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
    let protocol = protocol_config_for(sender);
    storage
        .append_safe_inputs(safe_block, inputs.as_slice(), &protocol)
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
        .map(|(_offset, tx)| tx)
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
