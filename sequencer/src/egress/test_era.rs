// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! A recovered era for tests of the feed, `/history` and the lane: setup
//! rebuilt counts 41..45 from a base at count 41 through stop block 10, one
//! rebuilt op was skipped, and one direct was still queued at the stop
//! block, so the root frame drains it as the first live row.

use std::path::Path;
use std::time::SystemTime;

use alloy_primitives::{Address, B256, Signature};
use sequencer_core::history::L2TxCount;
use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};
use sequencer_core::scheduler::Scheduled;
use sequencer_core::user_op::{SignedUserOp, UserOp};
use tokio::sync::oneshot;

use crate::egress::l2_tx_feed::BroadcastTxMessage;
use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp};
use crate::storage::test_helpers::default_protocol_timing;
use crate::storage::{IngestedSafeInput, LifecycleCommand, NewEra, Storage};
use crate::test_support::pin_test_deployment_identity;

pub(crate) const BASE: u64 = 41;
pub(crate) const REBUILT_END: u64 = 45;
pub(crate) const STOP_BLOCK: u64 = 10;
pub(crate) const FIRST_QUEUED_INPUT: u64 = 3;
pub(crate) const NEXT_BATCH_NONCE: u64 = 5;
pub(crate) const SUBMITTER: Address = Address::repeat_byte(0x5b);
pub(crate) const DIRECT_SENDER: Address = Address::repeat_byte(0xd1);
pub(crate) const OP_SENDER: Address = Address::repeat_byte(0x0b);

/// The L1 input at `index`, as the input table stores it: the timestamp and
/// transaction hash a rebuilt direct must carry on the feed.
pub(crate) fn safe_input(index: u64) -> IngestedSafeInput {
    let block = [5, 6, 8, STOP_BLOCK][usize::try_from(index).expect("fixture index")];
    let tag = 0xa0 + u8::try_from(index).expect("fixture index");
    IngestedSafeInput {
        sender: DIRECT_SENDER,
        payload: vec![tag],
        block_number: block,
        block_timestamp: 1_000 + block,
        transaction_hash: B256::repeat_byte(tag),
    }
}

/// A user op that setup rebuilt from input 1's batch.
pub(crate) fn rebuilt_op(nonce: u32) -> UserOpTx {
    UserOpTx {
        sender: OP_SENDER,
        nonce,
        fee: 9,
        block: 6,
        data: vec![0xd0, u8::try_from(nonce).expect("fixture nonce")],
    }
}

/// [`publish_rebuilt_era`], then open the root frame as `run`'s startup does.
pub(crate) fn seed_rebuilt_era(db_path: &str, baseline_prefix: &Path) -> Storage {
    let mut storage = publish_rebuilt_era(db_path, baseline_prefix);
    storage.ensure_open_tip().expect("open the root frame");
    storage
}

/// Create the database as setup leaves it, with `baseline_prefix` as the
/// era's baseline dump. Inputs 0 and 2 are directs a batch drained (batch
/// nonces 3 and 4); input 1 stands in for a batch of three ops whose second
/// (nonce 5) the executor skipped, but it is not the submitter's, so no
/// acceptance scan parses it; input 3 is a direct still queued at the stop
/// block.
pub(crate) fn publish_rebuilt_era(db_path: &str, baseline_prefix: &Path) -> Storage {
    let mut storage = Storage::initialize_for_command(db_path, LifecycleCommand::Setup)
        .expect("create the setup schema");
    pin_test_deployment_identity(&mut storage, SUBMITTER);
    storage
        .append_ingested_safe_inputs_with_timestamp(
            STOP_BLOCK,
            1_000 + STOP_BLOCK,
            &(0..4).map(safe_input).collect::<Vec<_>>(),
            &default_protocol_timing(),
            crate::clock::unix_now_ms(),
        )
        .expect("sync through the stop block");
    let direct = |index: u64, batch_nonce| Scheduled {
        batch_nonce,
        tx: L2Tx::Direct(Direct {
            input_index: index,
            sender: DIRECT_SENDER,
            block: safe_input(index).block_number,
            timestamp: safe_input(index).block_timestamp,
            payload: safe_input(index).payload,
        }),
    };
    let op = |nonce| Scheduled {
        batch_nonce: 3,
        tx: L2Tx::UserOp(rebuilt_op(nonce)),
    };
    // The op at nonce 5 is skipped, so it has no row.
    storage
        .append_rebuilt_history(&[
            (L2TxCount::new(41), direct(0, 3)),
            (L2TxCount::new(42), op(0)),
            (L2TxCount::new(43), op(1)),
            (L2TxCount::new(44), direct(2, 4)),
        ])
        .expect("store the rebuilt history");
    storage
        .publish_era(
            baseline_prefix,
            &NewEra {
                base: L2TxCount::new(BASE),
                rebuilt_end: L2TxCount::new(REBUILT_END),
                stop_block: STOP_BLOCK,
                first_queued_input: FIRST_QUEUED_INPUT,
                anchor_batch_nonce: NEXT_BATCH_NONCE,
            },
        )
        .expect("publish the era");
    storage
}

/// The feed message for each count from the base through the root frame's
/// queued direct, as JSON: a direct carries its input's block, timestamp and
/// transaction hash with its own row's batch nonce, and the skipped op has
/// no row, so it never appears.
pub(crate) fn expected_feed_message(offset: u64) -> serde_json::Value {
    let direct = |index: u64, batch_nonce| {
        let input = safe_input(index);
        BroadcastTxMessage::from_direct(
            offset,
            &Direct {
                input_index: index,
                sender: input.sender,
                block: input.block_number,
                timestamp: input.block_timestamp,
                payload: input.payload,
            },
            batch_nonce,
            input.transaction_hash,
        )
    };
    let message = match offset {
        41 => direct(0, 3),
        42 => BroadcastTxMessage::from_user_op(offset, &rebuilt_op(0), 3),
        43 => BroadcastTxMessage::from_user_op(offset, &rebuilt_op(1), 3),
        44 => direct(2, 4),
        // The first live row: the root batch drains the queued direct.
        45 => direct(FIRST_QUEUED_INPUT, NEXT_BATCH_NONCE),
        _ => panic!("the fixture fixes no message at count {offset}"),
    };
    serde_json::to_value(message).expect("serialize a feed message")
}

/// Append one live user op from `OP_SENDER` at the history head, as the lane
/// would after admitting it; returns the feed message expected for it.
pub(crate) fn append_live_user_op(storage: &mut Storage, nonce: u32) -> serde_json::Value {
    let mut head = storage
        .open_state()
        .expect("read the open frame")
        .expect("the root frame is open");
    let offset = storage.next_l2_tx_count().expect("read the history head");
    let data = vec![0xe0, u8::try_from(nonce).expect("fixture nonce")];
    let (respond_to, _response) = oneshot::channel();
    let included = IncludedUserOp {
        pending: PendingUserOp {
            signed: SignedUserOp {
                sender: OP_SENDER,
                signature: Signature::test_signature(),
                user_op: UserOp {
                    nonce,
                    max_fee: u16::MAX,
                    data: data.clone().into(),
                },
            },
            respond_to,
            received_at: SystemTime::now(),
        },
        l2_tx_index: offset,
    };
    storage
        .append_included_user_ops(&mut head, &[included])
        .expect("append a live user op");
    let op = UserOpTx {
        sender: OP_SENDER,
        nonce,
        fee: head.frame_fee(),
        block: head.safe_block(),
        data,
    };
    serde_json::to_value(BroadcastTxMessage::from_user_op(
        offset.get(),
        &op,
        NEXT_BATCH_NONCE,
    ))
    .expect("serialize a feed message")
}
