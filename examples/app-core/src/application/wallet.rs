// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::HashMap;

use alloy_primitives::{Address, U256};
use ssz::Decode;

use super::{
    MAX_METHOD_PAYLOAD_BYTES as WALLET_MAX_METHOD_PAYLOAD_BYTES, Method, prefunded_addresses,
};
use sequencer_core::application::{AppError, Application, InvalidReason};
use sequencer_core::l2_tx::ValidUserOp;
use sequencer_core::user_op::UserOp;

#[derive(Debug, Clone, Copy)]
pub struct WalletConfig;

impl Default for WalletConfig {
    fn default() -> Self {
        Self
    }
}

#[derive(Debug)]
pub struct WalletApp {
    balances: HashMap<Address, U256>,
    nonces: HashMap<Address, u32>,
    executed_input_count: u64,
}

pub const PREFUNDED_BALANCE: u64 = 1_000_000;

impl WalletApp {
    pub fn new(_config: WalletConfig) -> Self {
        let mut balances = HashMap::with_capacity(prefunded_addresses().len());
        for address in prefunded_addresses() {
            balances.insert(*address, U256::from(PREFUNDED_BALANCE));
        }

        Self {
            balances,
            nonces: HashMap::new(),
            executed_input_count: 0,
        }
    }

    fn balance_of(&self, addr: &Address) -> U256 {
        *self.balances.get(addr).unwrap_or(&U256::ZERO)
    }

    fn credit(&mut self, addr: Address, amount: U256) {
        let current = self.balance_of(&addr);
        self.balances.insert(addr, current + amount);
    }

    fn debit_if_possible(&mut self, addr: Address, amount: U256) -> bool {
        let current = self.balance_of(&addr);
        if current < amount {
            return false;
        }
        self.balances.insert(addr, current - amount);
        true
    }

    fn expected_nonce(&self, addr: &Address) -> u32 {
        self.nonces.get(addr).copied().unwrap_or(0)
    }

    fn bump_nonce(&mut self, addr: Address) {
        let next = self.expected_nonce(&addr).wrapping_add(1);
        self.nonces.insert(addr, next);
    }
}

impl Default for WalletApp {
    fn default() -> Self {
        Self::new(WalletConfig)
    }
}

impl Application for WalletApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = WALLET_MAX_METHOD_PAYLOAD_BYTES;

    fn current_user_nonce(&self, sender: Address) -> u32 {
        self.expected_nonce(&sender)
    }

    fn current_user_balance(&self, sender: Address) -> U256 {
        self.balance_of(&sender)
    }

    fn validate_user_op(
        &self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u64,
    ) -> Result<(), InvalidReason> {
        let expected_nonce = self.expected_nonce(&sender);
        if user_op.nonce != expected_nonce {
            return Err(InvalidReason::InvalidNonce {
                expected: expected_nonce,
                got: user_op.nonce,
            });
        }

        let max_fee = user_op.max_fee;
        // Users sign a cap; sequencer executes against the committed frame fee.
        if u64::from(max_fee) < current_fee {
            return Err(InvalidReason::InvalidMaxFee {
                max_fee,
                base_fee: current_fee,
            });
        }

        let gas_cost = U256::from(current_fee);
        let balance = self.balance_of(&sender);
        if balance < gas_cost {
            return Err(InvalidReason::InsufficientGasBalance {
                required: gas_cost,
                available: balance,
            });
        }

        Ok(())
    }

    fn execute_valid_user_op(&mut self, user_op: &ValidUserOp) -> Result<(), AppError> {
        let sender = user_op.sender;
        let gas_cost = U256::from(user_op.fee);
        let balance = self.balance_of(&sender);
        if balance < gas_cost {
            return Err(AppError::Internal {
                reason: "validated user op cannot pay gas".to_string(),
            });
        }

        self.bump_nonce(sender);
        self.balances.insert(sender, balance - gas_cost);

        let method = Method::from_ssz_bytes(user_op.data.as_slice()).ok();
        match method.as_ref() {
            Some(Method::Transfer(transfer)) => {
                if self.debit_if_possible(sender, transfer.amount) {
                    self.credit(transfer.to, transfer.amount);
                }
            }
            Some(Method::Withdrawal(withdrawal)) => {
                let _ = self.debit_if_possible(sender, withdrawal.amount);
            }
            None => {}
        }

        self.executed_input_count = self.executed_input_count.saturating_add(1);
        Ok(())
    }

    fn execute_direct_input(
        &mut self,
        _input: &sequencer_core::l2_tx::DirectInput,
    ) -> Result<(), AppError> {
        self.executed_input_count = self.executed_input_count.saturating_add(1);
        Ok(())
    }

    fn executed_input_count(&self) -> u64 {
        self.executed_input_count
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use ssz_derive::{Decode, Encode};

    use super::{WalletApp, WalletConfig};
    use sequencer_core::application::{Application, InvalidReason};
    use sequencer_core::l2_tx::ValidUserOp;
    use sequencer_core::user_op::UserOp;

    #[test]
    fn validate_rejects_when_max_fee_below_current_fee() {
        let mut app = WalletApp::new(WalletConfig);
        let sender = Address::from_slice(&[0x11; 20]);
        app.balances.insert(sender, U256::from(10_u64));

        let user_op = UserOp {
            nonce: 0,
            max_fee: 1,
            data: Vec::<u8>::new().into(),
        };

        let err = app
            .validate_user_op(sender, &user_op, 2)
            .expect_err("max_fee < current_fee should be invalid");
        assert_eq!(
            err,
            InvalidReason::InvalidMaxFee {
                max_fee: 1,
                base_fee: 2
            }
        );
    }

    #[test]
    fn execute_valid_user_op_charges_current_fee() {
        let mut app = WalletApp::new(WalletConfig);
        let sender = Address::from_slice(&[0x22; 20]);
        app.balances.insert(sender, U256::from(10_u64));

        let valid = ValidUserOp {
            sender,
            fee: 3,
            data: Vec::new(),
        };
        app.execute_valid_user_op(&valid).expect("execute valid op");

        assert_eq!(app.current_user_nonce(sender), 1);
        assert_eq!(app.current_user_balance(sender), U256::from(7_u64));
    }

    #[test]
    fn wallet_starts_with_prefunded_anvil_accounts() {
        let app = WalletApp::new(WalletConfig);
        let addresses = super::prefunded_addresses();
        assert!(addresses.len() >= 2);

        for address in addresses.iter().take(2) {
            assert_eq!(
                app.current_user_balance(*address),
                U256::from(super::PREFUNDED_BALANCE)
            );
        }
    }

    #[derive(PartialEq, Debug, Encode, Decode, Clone)]
    struct LegacyDeposit {
        amount: U256,
        to: Address,
    }

    #[derive(PartialEq, Debug, Encode, Decode, Clone)]
    struct LegacyWithdrawal {
        amount: U256,
    }

    #[derive(PartialEq, Debug, Encode, Decode, Clone)]
    struct LegacyTransfer {
        amount: U256,
        to: Address,
    }

    #[derive(PartialEq, Debug, Encode, Decode, Clone)]
    #[ssz(enum_behaviour = "union")]
    enum LegacyMethod {
        Withdrawal(LegacyWithdrawal),
        Transfer(LegacyTransfer),
        Deposit(LegacyDeposit),
    }

    #[test]
    fn legacy_deposit_payload_is_included_as_no_op() {
        let mut app = WalletApp::new(WalletConfig);
        let sender = super::prefunded_addresses()[0];
        let recipient = Address::from_slice(&[0x77; 20]);
        let before_sender_nonce = app.current_user_nonce(sender);
        let before_sender_balance = app.current_user_balance(sender);
        let before_recipient = app.current_user_balance(recipient);

        let legacy = LegacyMethod::Deposit(LegacyDeposit {
            amount: U256::from(123_u64),
            to: recipient,
        });
        let valid = ValidUserOp {
            sender,
            fee: 0,
            data: ssz::Encode::as_ssz_bytes(&legacy),
        };

        app.execute_valid_user_op(&valid)
            .expect("execute valid user op");

        assert_eq!(app.current_user_nonce(sender), before_sender_nonce + 1);
        assert_eq!(app.current_user_balance(sender), before_sender_balance);
        assert_eq!(app.current_user_balance(recipient), before_recipient);
    }
}
