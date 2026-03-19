// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use app_core::application::{WalletApp, WalletConfig};
use sequencer_core::api::WsTxMessage;
use sequencer_core::application::Application;
use sequencer_core::l2_tx::{DirectInput, ValidUserOp};

use crate::HarnessResult;

pub struct ReplayWalletApp {
    app: WalletApp,
}

impl ReplayWalletApp {
    pub fn devnet() -> Self {
        Self {
            app: WalletApp::new(WalletConfig::devnet()),
        }
    }

    pub fn apply(&mut self, message: WsTxMessage) -> HarnessResult<()> {
        apply_ws_message(&mut self.app, message)
    }

    pub fn current_user_balance(&self, sender: Address) -> U256 {
        self.app.current_user_balance(sender)
    }

    pub fn current_user_nonce(&self, sender: Address) -> u32 {
        self.app.current_user_nonce(sender)
    }

    pub fn executed_input_count(&self) -> u64 {
        self.app.executed_input_count()
    }
}

pub(crate) fn apply_ws_message<A: Application>(
    app: &mut A,
    message: WsTxMessage,
) -> HarnessResult<()> {
    match message {
        WsTxMessage::DirectInput {
            sender,
            block_number,
            payload,
            ..
        } => app.execute_direct_input(&DirectInput {
            sender: decode_address(sender.as_str()),
            block_number,
            payload: decode_hex_prefixed(payload.as_str()),
        })?,
        WsTxMessage::UserOp {
            sender, fee, data, ..
        } => app.execute_valid_user_op(&ValidUserOp {
            sender: decode_address(sender.as_str()),
            fee,
            data: decode_hex_prefixed(data.as_str()),
        })?,
    }
    Ok(())
}

fn decode_hex_prefixed(value: &str) -> Vec<u8> {
    assert!(value.starts_with("0x"), "hex field must be 0x-prefixed");
    alloy_primitives::hex::decode(value).expect("decode hex")
}

fn decode_address(value: &str) -> Address {
    Address::from_slice(&decode_hex_prefixed(value))
}
