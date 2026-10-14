// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! InputBox inputs as `eth_getLogs` returns them, for the reader and the
//! batch poster.

use alloy::sol_types::SolCall;
use alloy_primitives::{Address, B256, U256};
use cartesi_rollups_contracts::input_box::InputBox::InputAdded;
use cartesi_rollups_contracts::inputs::Inputs::EvmAdvanceCall;

/// The ABI-encoded `EvmAdvance` input the InputBox builds for one `addInput`.
pub(crate) fn evm_advance_input(
    chain_id: u64,
    app_contract: Address,
    msg_sender: Address,
    block_number: u64,
    index: u64,
    payload: &[u8],
) -> Vec<u8> {
    EvmAdvanceCall {
        chainId: U256::from(chain_id),
        appContract: app_contract,
        msgSender: msg_sender,
        blockNumber: U256::from(block_number),
        blockTimestamp: U256::from(1_700_000_000_u64),
        prevRandao: U256::ZERO,
        index: U256::from(index),
        payload: payload.to_vec().into(),
    }
    .abi_encode()
}

/// An `InputAdded` event at `topic_index` carrying `input`, and its log.
pub(crate) fn input_added_pair(
    topic_index: u64,
    input: Vec<u8>,
    block_number: Option<u64>,
    transaction_hash: Option<B256>,
) -> (InputAdded, alloy::rpc::types::Log) {
    let event = InputAdded {
        appContract: Address::ZERO,
        index: U256::from(topic_index),
        input: input.into(),
    };
    let log = alloy::rpc::types::Log {
        inner: alloy_primitives::Log {
            address: Address::ZERO,
            data: alloy_primitives::LogData::new_unchecked(
                Vec::new(),
                alloy_primitives::Bytes::new(),
            ),
        },
        block_hash: None,
        block_number,
        block_timestamp: None,
        transaction_hash,
        transaction_index: Some(0),
        log_index: Some(0),
        removed: false,
    };
    (event, log)
}
