// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::HashMap;

use alloy_primitives::{Address, U256, address};
use ssz::Decode;
use tracing::{error, warn};

use super::MAX_METHOD_PAYLOAD_BYTES as WALLET_MAX_METHOD_PAYLOAD_BYTES;
use super::Method;
use sequencer_core::application::{AppError, Application, InvalidReason};
use sequencer_core::l2_tx::ValidUserOp;
use sequencer_core::user_op::UserOp;

#[derive(Debug, Clone, Copy)]
pub struct WalletConfig {
    pub erc20_portal_address: Address,
    pub supported_erc20_token: Address,
}

impl WalletConfig {
    pub const fn sepolia() -> Self {
        Self {
            erc20_portal_address: SEPOLIA_ERC20_PORTAL_ADDRESS,
            supported_erc20_token: SEPOLIA_USDC_ADDRESS,
        }
    }

    pub const fn devnet() -> Self {
        Self {
            erc20_portal_address: SEPOLIA_ERC20_PORTAL_ADDRESS,
            supported_erc20_token: DEVNET_MOCK_USDC_ADDRESS,
        }
    }
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self::sepolia()
    }
}

#[derive(Debug)]
pub struct WalletApp {
    config: WalletConfig,
    balances: HashMap<Address, U256>,
    nonces: HashMap<Address, u32>,
    executed_input_count: u64,
}

pub const SEPOLIA_ERC20_PORTAL_ADDRESS: Address =
    address!("0xACA6586A0Cf05bD831f2501E7B4aea550dA6562D");
pub const SEPOLIA_USDC_ADDRESS: Address = address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
pub const DEVNET_MOCK_USDC_ADDRESS: Address =
    address!("0x95d0c8A7d11342299807A2Fc19ac44C2321cCc68");
const ERC20_DEPOSIT_PREFIX_BYTES: usize = 20 + 20 + 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Erc20Deposit {
    token: Address,
    sender: Address,
    value: U256,
}

impl WalletApp {
    pub fn new(config: WalletConfig) -> Self {
        Self {
            config,
            balances: HashMap::new(),
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

    fn decode_portal_erc20_deposit(
        portal_address: Address,
        input: &sequencer_core::l2_tx::DirectInput,
    ) -> Result<Option<Erc20Deposit>, String> {
        if input.sender != portal_address {
            return Ok(None);
        }

        if input.payload.len() < ERC20_DEPOSIT_PREFIX_BYTES {
            return Err(format!(
                "trusted ERC-20 portal payload too short: expected at least {ERC20_DEPOSIT_PREFIX_BYTES} bytes, got {}",
                input.payload.len()
            ));
        }

        Ok(Some(Erc20Deposit {
            token: Address::from_slice(&input.payload[0..20]),
            sender: Address::from_slice(&input.payload[20..40]),
            value: U256::from_be_slice(&input.payload[40..72]),
        }))
    }
}

impl Default for WalletApp {
    fn default() -> Self {
        Self::new(WalletConfig::default())
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
        input: &sequencer_core::l2_tx::DirectInput,
    ) -> Result<(), AppError> {
        match Self::decode_portal_erc20_deposit(self.config.erc20_portal_address, input) {
            Ok(Some(deposit)) => {
                if deposit.token == self.config.supported_erc20_token {
                    self.credit(deposit.sender, deposit.value);
                } else {
                    warn!(
                        portal = %input.sender,
                        token = %deposit.token,
                        sender = %deposit.sender,
                        block_number = input.block_number,
                        "ignoring unsupported ERC-20 deposit token"
                    );
                }
            }
            Ok(None) => {}
            Err(reason) => {
                error!(
                    portal = %input.sender,
                    block_number = input.block_number,
                    error = %reason,
                    "ignoring malformed trusted ERC-20 deposit payload"
                );
            }
        }

        self.executed_input_count = self.executed_input_count.saturating_add(1);
        Ok(())
    }

    fn executed_input_count(&self) -> u64 {
        self.executed_input_count
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address};
    use ssz_derive::{Decode, Encode};

    use super::{WalletApp, WalletConfig};
    use sequencer_core::application::{Application, InvalidReason};
    use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
    use sequencer_core::user_op::UserOp;

    #[test]
    fn validate_rejects_when_max_fee_below_current_fee() {
        let mut app = WalletApp::new(WalletConfig::default());
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
        let mut app = WalletApp::new(WalletConfig::default());
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
    fn wallet_starts_with_zero_balances() {
        let app = WalletApp::new(WalletConfig::default());
        let sender = address!("0x1111111111111111111111111111111111111111");
        let recipient = address!("0x2222222222222222222222222222222222222222");

        assert_eq!(app.current_user_balance(sender), U256::ZERO);
        assert_eq!(app.current_user_balance(recipient), U256::ZERO);
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
        let mut app = WalletApp::new(WalletConfig::default());
        let sender = address!("0x1111111111111111111111111111111111111111");
        let recipient = Address::from_slice(&[0x77; 20]);
        app.balances.insert(sender, U256::from(500_u64));
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

    fn encode_erc20_deposit_payload(token: Address, sender: Address, value: U256) -> Vec<u8> {
        let mut payload = Vec::with_capacity(super::ERC20_DEPOSIT_PREFIX_BYTES);
        payload.extend_from_slice(token.as_slice());
        payload.extend_from_slice(sender.as_slice());
        payload.extend_from_slice(value.to_be_bytes::<32>().as_slice());
        payload
    }

    #[test]
    fn trusted_portal_usdc_deposit_credits_nested_sender() {
        let mut app = WalletApp::new(WalletConfig::default());
        let nested_sender = address!("0x7777777777777777777777777777777777777777");

        let before = app.current_user_balance(nested_sender);
        app.execute_direct_input(&DirectInput {
            sender: super::SEPOLIA_ERC20_PORTAL_ADDRESS,
            block_number: 123,
            payload: encode_erc20_deposit_payload(
                super::SEPOLIA_USDC_ADDRESS,
                nested_sender,
                U256::from(250_u64),
            ),
        })
        .expect("execute deposit direct input");

        assert_eq!(
            app.current_user_balance(nested_sender),
            before + U256::from(250_u64)
        );
        assert_eq!(app.executed_input_count(), 1);
    }

    #[test]
    fn non_portal_direct_input_remains_no_op() {
        let mut app = WalletApp::new(WalletConfig::default());
        let nested_sender = address!("0x7777777777777777777777777777777777777777");

        let before = app.current_user_balance(nested_sender);
        app.execute_direct_input(&DirectInput {
            sender: address!("0x3333333333333333333333333333333333333333"),
            block_number: 123,
            payload: encode_erc20_deposit_payload(
                super::SEPOLIA_USDC_ADDRESS,
                nested_sender,
                U256::from(250_u64),
            ),
        })
        .expect("execute non-portal direct input");

        assert_eq!(app.current_user_balance(nested_sender), before);
        assert_eq!(app.executed_input_count(), 1);
    }

    #[test]
    fn trusted_portal_unsupported_token_is_a_no_op() {
        let mut app = WalletApp::new(WalletConfig::default());
        let nested_sender = address!("0x7777777777777777777777777777777777777777");
        let unsupported_token = address!("0x9999999999999999999999999999999999999999");

        let before = app.current_user_balance(nested_sender);
        app.execute_direct_input(&DirectInput {
            sender: super::SEPOLIA_ERC20_PORTAL_ADDRESS,
            block_number: 123,
            payload: encode_erc20_deposit_payload(
                unsupported_token,
                nested_sender,
                U256::from(250_u64),
            ),
        })
        .expect("unsupported token should be ignored");

        assert_eq!(app.current_user_balance(nested_sender), before);
        assert_eq!(app.executed_input_count(), 1);
    }

    #[test]
    fn malformed_trusted_portal_deposit_is_a_no_op() {
        let mut app = WalletApp::new(WalletConfig::default());

        app.execute_direct_input(&DirectInput {
            sender: super::SEPOLIA_ERC20_PORTAL_ADDRESS,
            block_number: 123,
            payload: vec![0xaa; 10],
        })
        .expect("malformed trusted portal payload should be ignored");

        assert_eq!(app.executed_input_count(), 1);
    }

    #[test]
    fn devnet_config_uses_deterministic_mock_usdc_address() {
        let config = WalletConfig::devnet();
        assert_eq!(
            config.erc20_portal_address,
            super::SEPOLIA_ERC20_PORTAL_ADDRESS
        );
        assert_eq!(
            config.supported_erc20_token,
            super::DEVNET_MOCK_USDC_ADDRESS
        );
    }
}
