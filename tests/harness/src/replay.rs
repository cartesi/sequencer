// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use app_core::application::{WalletApp, WalletConfig};
use sequencer_core::api::WsTxMessage;
use sequencer_core::application::{Application, CanonicalState, Executed, execute};
use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};

use crate::HarnessResult;
use crate::util::io_other;

pub struct ReplayWalletApp {
    app: WalletApp,
    /// The application no longer keeps an L1 clock, so the mirror remembers
    /// the block carried by the last message it applied.
    last_applied_block: Option<u64>,
}

impl ReplayWalletApp {
    pub fn from_dump(prefix: &std::path::Path) -> HarnessResult<Self> {
        Ok(Self {
            app: WalletApp::from_dump(prefix)?,
            last_applied_block: None,
        })
    }

    pub fn canonical_snapshot_bytes(&self) -> HarnessResult<Vec<u8>> {
        Ok(self.app.canonical_snapshot_bytes()?)
    }

    pub fn devnet() -> Self {
        Self {
            app: WalletApp::new(WalletConfig::devnet()),
            last_applied_block: None,
        }
    }

    pub fn apply(&mut self, message: WsTxMessage) -> HarnessResult<()> {
        let block = apply_ws_message(&mut self.app, message)?;
        self.last_applied_block = Some(block);
        Ok(())
    }

    pub fn current_user_balance(&self, sender: Address) -> U256 {
        self.app.current_user_balance(sender)
    }

    pub fn current_user_nonce(&self, sender: Address) -> u32 {
        self.app.current_user_nonce(sender)
    }

    pub fn l2_tx_count(&self) -> u64 {
        self.app.l2_tx_count().get()
    }

    /// The frame's safe block of the last applied user op, or the inclusion
    /// block of the last applied direct; `None` until a message is applied.
    pub fn last_applied_block(&self) -> Option<u64> {
        self.last_applied_block
    }
}

/// Runs one feed message through the shared executor, as a replica would.
/// Returns the L1 block the message carries (the frame's safe block for a
/// user op, the inclusion block for a direct), so mirrors that need an L1
/// clock can track it themselves.
pub fn apply_ws_message<A: Application>(app: &mut A, message: WsTxMessage) -> HarnessResult<u64> {
    let expected = app.l2_tx_count().get();
    if message.offset() != expected {
        return Err(io_other(format!(
            "WS history gap: expected input {expected}, got {}",
            message.offset()
        ))
        .into());
    }
    let tx = l2_tx_from_ws_message(&message)?;
    let block = match &tx {
        L2Tx::Direct(direct) => direct.block,
        L2Tx::UserOp(op) => op.block,
    };
    match execute(app, &tx)? {
        Executed::Done { index, .. } => {
            if index.get() != expected {
                return Err(io_other(format!(
                    "executor reported index {}, expected {expected}",
                    index.get()
                ))
                .into());
            }
        }
        // The feed serves executed rows only, so a skip means the mirror and
        // the sequencer disagree on a nonce.
        Executed::Skipped(reason) => {
            return Err(io_other(format!(
                "WS input {expected} would be skipped by the mirror: {reason}"
            ))
            .into());
        }
    }
    Ok(block)
}

/// Rebuilds the executor's record from a feed message; the wire names
/// `safe_block`, `block_number` and `block_timestamp` map to the record's
/// `block` and `timestamp`.
pub fn l2_tx_from_ws_message(message: &WsTxMessage) -> HarnessResult<L2Tx> {
    Ok(match message {
        WsTxMessage::DirectInput {
            sender,
            block_number,
            payload,
            input_index,
            block_timestamp,
            ..
        } => L2Tx::Direct(Direct {
            input_index: *input_index,
            sender: decode_address(sender)?,
            block: *block_number,
            timestamp: *block_timestamp,
            payload: decode_hex_prefixed(payload)?,
        }),
        WsTxMessage::UserOp {
            sender,
            nonce,
            fee,
            data,
            safe_block,
            ..
        } => L2Tx::UserOp(UserOpTx {
            sender: decode_address(sender)?,
            nonce: *nonce,
            fee: *fee,
            block: *safe_block,
            data: decode_hex_prefixed(data)?,
        }),
    })
}

fn decode_hex_prefixed(value: &str) -> HarnessResult<Vec<u8>> {
    if !value.starts_with("0x") {
        return Err(io_other(format!("WS hex field must be 0x-prefixed: {value}")).into());
    }
    alloy_primitives::hex::decode(value)
        .map_err(|err| io_other(format!("WS hex field {value}: {err}")).into())
}

fn decode_address(value: &str) -> HarnessResult<Address> {
    let bytes = decode_hex_prefixed(value)?;
    if bytes.len() != Address::len_bytes() {
        return Err(io_other(format!("WS address has {} bytes", bytes.len())).into());
    }
    Ok(Address::from_slice(&bytes))
}
