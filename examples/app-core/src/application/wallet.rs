// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, U256, address};
use ssz::{Decode, Encode};
use ssz_derive::{Decode as SszDecode, Encode as SszEncode};
use tracing::{error, warn};
use types::alloy_sol_types::SolCall;
use types::{Erc20Deposit, Erc20Transfer};

use super::MAX_METHOD_PAYLOAD_BYTES as WALLET_MAX_METHOD_PAYLOAD_BYTES;
use super::Method;
use super::{DepositNotice, TransferNotice};
use sequencer_core::application::{AppError, AppOutput, AppOutputs, Application, InvalidReason};
use sequencer_core::l2_tx::ValidUserOp;
use sequencer_core::user_op::UserOp;

#[derive(Debug, Clone, Copy)]
pub struct WalletConfig {
    pub erc20_portal_address: Address,
    pub supported_erc20_token: Address,
    /// Address that receives fee revenue. Defaults to `Address::ZERO` (burned).
    pub sequencer_address: Address,
}

impl WalletConfig {
    pub const fn sepolia() -> Self {
        Self {
            erc20_portal_address: SEPOLIA_ERC20_PORTAL_ADDRESS,
            supported_erc20_token: SEPOLIA_USDC_ADDRESS,
            sequencer_address: SEPOLIA_SEQUENCER_ADDRESS,
        }
    }

    pub const fn devnet() -> Self {
        Self {
            erc20_portal_address: SEPOLIA_ERC20_PORTAL_ADDRESS,
            supported_erc20_token: DEVNET_MOCK_USDC_ADDRESS,
            sequencer_address: DEVNET_SEQUENCER_ADDRESS,
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

#[derive(Debug, Clone, PartialEq, Eq, SszEncode, SszDecode)]
struct SnapshotBalance {
    address: [u8; 20],
    balance_be: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, SszEncode, SszDecode)]
struct SnapshotNonce {
    address: [u8; 20],
    nonce: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, SszEncode, SszDecode)]
struct WalletSnapshotV1 {
    erc20_portal_address: [u8; 20],
    supported_erc20_token: [u8; 20],
    sequencer_address: [u8; 20],
    balances: Vec<SnapshotBalance>,
    nonces: Vec<SnapshotNonce>,
    executed_input_count: u64,
}

pub const SEPOLIA_ERC20_PORTAL_ADDRESS: Address =
    address!("0xACA6586A0Cf05bD831f2501E7B4aea550dA6562D");
pub const SEPOLIA_USDC_ADDRESS: Address = address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
pub const DEVNET_MOCK_USDC_ADDRESS: Address =
    address!("0x95d0c8A7d11342299807A2Fc19ac44C2321cCc68");
pub const SEPOLIA_SEQUENCER_ADDRESS: Address =
    address!("0x16d5FF3Fdd14e2a86FBA77cbcE6B3Cd9C32b8Ff3");
pub const DEVNET_SEQUENCER_ADDRESS: Address =
    address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
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
    ) -> Result<Option<Erc20Deposit>, types::Erc20DepositDecodeError> {
        if input.sender != portal_address {
            return Ok(None);
        }

        Erc20Deposit::decode(&input.payload).map(Some)
    }

    /// Serialize the wallet's logical state to deterministic SSZ bytes.
    ///
    /// Determinism is enforced by sorting `balances` and `nonces` by
    /// address before encoding; `WalletApp` stores them in `HashMap`s
    /// whose iteration order is non-deterministic.
    fn snapshot_bytes(&self) -> Vec<u8> {
        let mut balances: Vec<_> = self
            .balances
            .iter()
            .map(|(address, balance)| SnapshotBalance {
                address: address.into_array(),
                balance_be: balance.to_be_bytes(),
            })
            .collect();
        balances.sort_unstable_by_key(|entry| entry.address);

        let mut nonces: Vec<_> = self
            .nonces
            .iter()
            .map(|(address, nonce)| SnapshotNonce {
                address: address.into_array(),
                nonce: *nonce,
            })
            .collect();
        nonces.sort_unstable_by_key(|entry| entry.address);

        WalletSnapshotV1 {
            erc20_portal_address: self.config.erc20_portal_address.into_array(),
            supported_erc20_token: self.config.supported_erc20_token.into_array(),
            sequencer_address: self.config.sequencer_address.into_array(),
            balances,
            nonces,
            executed_input_count: self.executed_input_count,
        }
        .as_ssz_bytes()
    }

    /// Decode SSZ-encoded snapshot bytes into a fresh `WalletApp`.
    ///
    /// Rejects snapshots containing duplicate addresses in `balances` or
    /// `nonces`. Without this check, multiple distinct byte sequences
    /// could decode to the same logical state (whichever entry came
    /// last in the encoded list would win silently), breaking the
    /// canonicality the watchdog relies on for byte-for-byte comparison.
    fn from_snapshot_bytes(bytes: &[u8]) -> Result<Self, AppError> {
        let decoded = WalletSnapshotV1::from_ssz_bytes(bytes).map_err(|e| AppError::Internal {
            // ssz::DecodeError doesn't implement Display, so Debug is the
            // only way to surface variant info as a string. If downstream
            // needs to match on decode kinds programmatically, introduce a
            // dedicated AppError variant that carries the typed error
            // instead of fixing the format string.
            reason: format!("snapshot decode failed: {e:?}"),
        })?;

        let mut balances = HashMap::with_capacity(decoded.balances.len());
        for entry in decoded.balances {
            let address = Address::from(entry.address);
            let balance = U256::from_be_bytes(entry.balance_be);
            if balances.insert(address, balance).is_some() {
                return Err(AppError::Internal {
                    reason: format!("snapshot contains duplicate balance entry for {address}"),
                });
            }
        }

        let mut nonces = HashMap::with_capacity(decoded.nonces.len());
        for entry in decoded.nonces {
            let address = Address::from(entry.address);
            if nonces.insert(address, entry.nonce).is_some() {
                return Err(AppError::Internal {
                    reason: format!("snapshot contains duplicate nonce entry for {address}"),
                });
            }
        }

        Ok(Self {
            config: WalletConfig {
                erc20_portal_address: Address::from(decoded.erc20_portal_address),
                supported_erc20_token: Address::from(decoded.supported_erc20_token),
                sequencer_address: Address::from(decoded.sequencer_address),
            },
            balances,
            nonces,
            executed_input_count: decoded.executed_input_count,
        })
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
        current_fee: u16,
    ) -> Result<(), InvalidReason> {
        let expected_nonce = self.expected_nonce(&sender);
        if user_op.nonce != expected_nonce {
            return Err(InvalidReason::InvalidNonce {
                expected: expected_nonce,
                got: user_op.nonce,
            });
        }

        // max_fee < current_fee is already checked by the trait default in
        // validate_and_execute_user_op. No need to repeat here.

        let gas_cost = sequencer_core::fee::fee_to_linear(current_fee);
        let balance = self.balance_of(&sender);
        if balance < gas_cost {
            return Err(InvalidReason::InsufficientGasBalance {
                required: gas_cost,
                available: balance,
            });
        }

        Ok(())
    }

    fn execute_valid_user_op(&mut self, user_op: &ValidUserOp) -> Result<AppOutputs, AppError> {
        let sender = user_op.sender;
        let gas_cost = sequencer_core::fee::fee_to_linear(user_op.fee);
        let balance = self.balance_of(&sender);
        if balance < gas_cost {
            return Err(AppError::Internal {
                reason: "validated user op cannot pay gas".to_string(),
            });
        }

        self.bump_nonce(sender);
        self.balances.insert(sender, balance - gas_cost);
        self.credit(self.config.sequencer_address, gas_cost);
        let mut outputs = Vec::new();

        let method = Method::from_ssz_bytes(user_op.data.as_slice()).ok();
        match method.as_ref() {
            Some(Method::Transfer(transfer)) if self.debit_if_possible(sender, transfer.amount) => {
                self.credit(transfer.to, transfer.amount);
                outputs.push(AppOutput::Notice(
                    TransferNotice {
                        sender,
                        recipient: transfer.to,
                        amount: transfer.amount,
                    }
                    .abi_encode(),
                ));
            }
            Some(Method::Withdrawal(withdrawal))
                if self.debit_if_possible(sender, withdrawal.amount) =>
            {
                outputs.push(AppOutput::Voucher {
                    destination: self.config.supported_erc20_token,
                    value: U256::ZERO,
                    payload: Erc20Transfer {
                        recipient: sender,
                        amount: withdrawal.amount,
                    }
                    .abi_encode(),
                });
            }
            _ => {}
        }

        self.executed_input_count = self.executed_input_count.saturating_add(1);
        Ok(outputs)
    }

    fn execute_direct_input(
        &mut self,
        input: &sequencer_core::l2_tx::DirectInput,
    ) -> Result<AppOutputs, AppError> {
        let mut outputs = Vec::new();
        match Self::decode_portal_erc20_deposit(self.config.erc20_portal_address, input) {
            Ok(Some(deposit)) => {
                if deposit.token == self.config.supported_erc20_token {
                    self.credit(deposit.sender, deposit.value);
                    outputs.push(AppOutput::Notice(
                        DepositNotice {
                            token: deposit.token,
                            sender: deposit.sender,
                            amount: deposit.value,
                        }
                        .abi_encode(),
                    ));
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
        Ok(outputs)
    }

    fn executed_input_count(&self) -> u64 {
        self.executed_input_count
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let state_path = Self::state_file_in_dump(prefix);
        let bytes = std::fs::read(&state_path)?;
        Self::from_snapshot_bytes(&bytes)
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
        // `create_dir` (not `create_dir_all`) deliberately errors if the
        // prefix already exists. Snapshot prefixes are expected to be
        // unique per call; a collision means a lane bug worth surfacing
        // loudly rather than silently overwriting prior state.
        std::fs::create_dir(prefix)?;
        let bytes = self.snapshot_bytes();
        std::fs::write(Self::state_file_in_dump(prefix), bytes)?;
        Ok(())
    }

    fn delete_dump(prefix: &Path) -> Result<(), AppError> {
        std::fs::remove_dir_all(prefix)?;
        Ok(())
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        prefix.join("state")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use alloy_primitives::{Address, U256, address};
    use ssz_derive::{Decode, Encode};
    use types::ERC20_DEPOSIT_PREFIX_BYTES;
    use types::Erc20Transfer;
    use types::alloy_sol_types::SolCall;

    use super::{WalletApp, WalletConfig};
    use crate::application::{DepositNotice, Transfer, TransferNotice, Withdrawal};
    use sequencer_core::application::{AppError, AppOutput, Application, InvalidReason};
    use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
    use sequencer_core::user_op::UserOp;

    #[test]
    fn validate_rejects_when_max_fee_below_current_fee() {
        use sequencer_core::application::{Application, ExecutionOutcome};

        let mut app = WalletApp::new(WalletConfig::default());
        let sender = Address::from_slice(&[0x11; 20]);
        app.balances.insert(sender, U256::from(10_u64));

        let user_op = UserOp {
            nonce: 0,
            max_fee: 1,
            data: Vec::<u8>::new().into(),
        };

        // The max_fee < current_fee check now lives in the trait default
        // (validate_and_execute_user_op), not in validate_user_op directly.
        let result = app
            .validate_and_execute_user_op(sender, &user_op, 2)
            .expect("should return Ok(Invalid), not Err");
        assert_eq!(
            result,
            ExecutionOutcome::Invalid(InvalidReason::InvalidMaxFee {
                max_fee: 1,
                base_fee: 2
            })
        );
    }

    #[test]
    fn execute_valid_user_op_charges_current_fee() {
        let mut app = WalletApp::new(WalletConfig::default());
        let sender = Address::from_slice(&[0x22; 20]);
        let initial_balance = U256::from(1000_u64);
        app.balances.insert(sender, initial_balance);

        // fee exponent 100 → fee_to_linear(100) ≈ 2 ((129/128)^100 ≈ 2.17, truncated)
        let fee_exponent: u16 = 100;
        let valid = ValidUserOp {
            sender,
            fee: fee_exponent,
            data: Vec::new(),
        };
        let gas_cost = sequencer_core::fee::fee_to_linear(fee_exponent);
        let outputs = app.execute_valid_user_op(&valid).expect("execute valid op");

        assert_eq!(app.current_user_nonce(sender), 1);
        assert_eq!(app.current_user_balance(sender), initial_balance - gas_cost);
        assert!(outputs.is_empty());
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
        // fee exponent 0 → fee_to_linear(0) = 1 (minimum fee)
        let valid = ValidUserOp {
            sender,
            fee: 0,
            data: ssz::Encode::as_ssz_bytes(&legacy),
        };

        let outputs = app
            .execute_valid_user_op(&valid)
            .expect("execute valid user op");

        assert_eq!(app.current_user_nonce(sender), before_sender_nonce + 1);
        // Gas cost of 1 unit (fee_to_linear(0) = 1) is deducted
        assert_eq!(
            app.current_user_balance(sender),
            before_sender_balance - U256::from(1u64)
        );
        assert_eq!(app.current_user_balance(recipient), before_recipient);
        assert!(outputs.is_empty());
    }

    fn encode_erc20_deposit_payload(token: Address, sender: Address, value: U256) -> Vec<u8> {
        let mut payload = Vec::with_capacity(ERC20_DEPOSIT_PREFIX_BYTES);
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
        let outputs = app
            .execute_direct_input(&DirectInput {
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
        assert_eq!(outputs.len(), 1);
        match &outputs[0] {
            AppOutput::Notice(payload) => {
                let notice = DepositNotice::abi_decode(payload).expect("decode deposit notice");
                assert_eq!(notice.token, super::SEPOLIA_USDC_ADDRESS);
                assert_eq!(notice.sender, nested_sender);
                assert_eq!(notice.amount, U256::from(250_u64));
            }
            other => panic!("expected deposit notice, got {other:?}"),
        }
    }

    #[test]
    fn non_portal_direct_input_remains_no_op() {
        let mut app = WalletApp::new(WalletConfig::default());
        let nested_sender = address!("0x7777777777777777777777777777777777777777");

        let before = app.current_user_balance(nested_sender);
        let outputs = app
            .execute_direct_input(&DirectInput {
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
        assert!(outputs.is_empty());
    }

    #[test]
    fn trusted_portal_unsupported_token_is_a_no_op() {
        let mut app = WalletApp::new(WalletConfig::default());
        let nested_sender = address!("0x7777777777777777777777777777777777777777");
        let unsupported_token = address!("0x9999999999999999999999999999999999999999");

        let before = app.current_user_balance(nested_sender);
        let outputs = app
            .execute_direct_input(&DirectInput {
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
        assert!(outputs.is_empty());
    }

    #[test]
    fn malformed_trusted_portal_deposit_is_a_no_op() {
        let mut app = WalletApp::new(WalletConfig::default());

        let outputs = app
            .execute_direct_input(&DirectInput {
                sender: super::SEPOLIA_ERC20_PORTAL_ADDRESS,
                block_number: 123,
                payload: vec![0xaa; 10],
            })
            .expect("malformed trusted portal payload should be ignored");

        assert_eq!(app.executed_input_count(), 1);
        assert!(outputs.is_empty());
    }

    #[test]
    fn transfer_emits_notice() {
        let mut app = WalletApp::new(WalletConfig::default());
        let sender = address!("0x1111111111111111111111111111111111111111");
        let recipient = address!("0x2222222222222222222222222222222222222222");
        app.balances.insert(sender, U256::from(500_u64));

        // fee exponent 10 → fee_to_linear(10) = 1 ((129/128)^10 ≈ 1.08, truncated)
        let fee_exponent: u16 = 10;
        let gas_cost = sequencer_core::fee::fee_to_linear(fee_exponent);
        let valid = ValidUserOp {
            sender,
            fee: fee_exponent,
            data: ssz::Encode::as_ssz_bytes(&super::Method::Transfer(Transfer {
                amount: U256::from(123_u64),
                to: recipient,
            })),
        };

        let outputs = app.execute_valid_user_op(&valid).expect("execute transfer");

        assert_eq!(
            app.current_user_balance(sender),
            U256::from(500_u64) - gas_cost - U256::from(123_u64)
        );
        assert_eq!(app.current_user_balance(recipient), U256::from(123_u64));
        assert_eq!(outputs.len(), 1);
        match &outputs[0] {
            AppOutput::Notice(payload) => {
                let notice = TransferNotice::abi_decode(payload).expect("decode transfer notice");
                assert_eq!(notice.sender, sender);
                assert_eq!(notice.recipient, recipient);
                assert_eq!(notice.amount, U256::from(123_u64));
            }
            other => panic!("expected transfer notice, got {other:?}"),
        }
    }

    #[test]
    fn withdrawal_emits_voucher() {
        let mut app = WalletApp::new(WalletConfig::default());
        let sender = address!("0x1111111111111111111111111111111111111111");
        app.balances.insert(sender, U256::from(500_u64));

        let fee_exponent: u16 = 10;
        let gas_cost = sequencer_core::fee::fee_to_linear(fee_exponent);
        let valid = ValidUserOp {
            sender,
            fee: fee_exponent,
            data: ssz::Encode::as_ssz_bytes(&super::Method::Withdrawal(Withdrawal {
                amount: U256::from(123_u64),
            })),
        };

        let outputs = app
            .execute_valid_user_op(&valid)
            .expect("execute withdrawal");

        assert_eq!(
            app.current_user_balance(sender),
            U256::from(500_u64) - gas_cost - U256::from(123_u64)
        );
        assert_eq!(outputs.len(), 1);
        match &outputs[0] {
            AppOutput::Voucher {
                destination,
                value,
                payload,
            } => {
                assert_eq!(*destination, super::SEPOLIA_USDC_ADDRESS);
                assert_eq!(*value, U256::ZERO);
                let transfer =
                    Erc20Transfer::abi_decode(payload).expect("decode withdrawal voucher");
                assert_eq!(transfer.recipient, sender);
                assert_eq!(transfer.amount, U256::from(123_u64));
            }
            other => panic!("expected withdrawal voucher, got {other:?}"),
        }
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

    #[test]
    fn fee_credits_sequencer_address() {
        let sequencer = address!("0x9999999999999999999999999999999999999999");
        let config = WalletConfig {
            sequencer_address: sequencer,
            ..WalletConfig::default()
        };
        let mut app = WalletApp::new(config);
        let sender = address!("0x1111111111111111111111111111111111111111");
        app.balances.insert(sender, U256::from(10_000_u64));

        let fee_exponent: u16 = 100;
        let gas_cost = sequencer_core::fee::fee_to_linear(fee_exponent);
        let valid = ValidUserOp {
            sender,
            fee: fee_exponent,
            data: Vec::new(),
        };
        app.execute_valid_user_op(&valid).expect("execute op");

        assert_eq!(
            app.current_user_balance(sender),
            U256::from(10_000_u64) - gas_cost
        );
        assert_eq!(
            app.current_user_balance(sequencer),
            gas_cost,
            "sequencer should receive the fee"
        );
    }

    #[test]
    fn fee_sent_to_zero_address_when_default() {
        let config = WalletConfig {
            sequencer_address: Address::ZERO,
            ..WalletConfig::default()
        };
        let mut app = WalletApp::new(config);
        let sender = address!("0x1111111111111111111111111111111111111111");
        app.balances.insert(sender, U256::from(10_000_u64));

        let fee_exponent: u16 = 100;
        let gas_cost = sequencer_core::fee::fee_to_linear(fee_exponent);
        let valid = ValidUserOp {
            sender,
            fee: fee_exponent,
            data: Vec::new(),
        };
        app.execute_valid_user_op(&valid).expect("execute op");

        assert_eq!(
            app.current_user_balance(sender),
            U256::from(10_000_u64) - gas_cost
        );
        // Fee goes to address zero — effectively burned.
        assert_eq!(app.current_user_balance(Address::ZERO), gas_cost);
    }

    #[test]
    fn create_dump_then_from_dump_round_trips_full_state() {
        let mut app = WalletApp::new(WalletConfig {
            erc20_portal_address: address!("0x1212121212121212121212121212121212121212"),
            supported_erc20_token: address!("0x3434343434343434343434343434343434343434"),
            sequencer_address: address!("0x5656565656565656565656565656565656565656"),
        });
        let alice = address!("0x1111111111111111111111111111111111111111");
        let bob = address!("0x2222222222222222222222222222222222222222");
        app.balances.insert(alice, U256::from(1234_u64));
        app.balances.insert(bob, U256::from(5678_u64));
        app.nonces.insert(alice, 4);
        app.nonces.insert(bob, 9);
        app.executed_input_count = 42;

        let prefix = temp_dump_prefix();
        app.create_dump(&prefix).expect("create dump");

        let restored = WalletApp::from_dump(&prefix).expect("load dump");

        WalletApp::delete_dump(&prefix).expect("cleanup dump");

        assert_eq!(
            restored.config.erc20_portal_address,
            app.config.erc20_portal_address
        );
        assert_eq!(
            restored.config.supported_erc20_token,
            app.config.supported_erc20_token
        );
        assert_eq!(
            restored.config.sequencer_address,
            app.config.sequencer_address
        );
        assert_eq!(restored.balances, app.balances);
        assert_eq!(restored.nonces, app.nonces);
        assert_eq!(restored.executed_input_count, app.executed_input_count);
    }

    #[test]
    fn create_dump_produces_deterministic_bytes() {
        // Insert in scrambled order so the HashMap's non-deterministic
        // iteration order is exercised. The encoder must sort to make
        // the resulting bytes stable; without sorting, two `create_dump`
        // calls on the same logical state could produce different files.
        let mut app = WalletApp::new(WalletConfig::default());
        app.balances.insert(
            address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            U256::from(1_u64),
        );
        app.balances.insert(
            address!("0x1111111111111111111111111111111111111111"),
            U256::from(2_u64),
        );
        app.balances.insert(
            address!("0x5555555555555555555555555555555555555555"),
            U256::from(3_u64),
        );
        app.nonces
            .insert(address!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), 7);
        app.nonces
            .insert(address!("0x1111111111111111111111111111111111111111"), 8);
        app.executed_input_count = 99;

        let prefix_a = temp_dump_prefix();
        let prefix_b = temp_dump_prefix();
        app.create_dump(&prefix_a).expect("first dump");
        app.create_dump(&prefix_b).expect("second dump");

        let bytes_a = std::fs::read(WalletApp::state_file_in_dump(&prefix_a)).expect("read a");
        let bytes_b = std::fs::read(WalletApp::state_file_in_dump(&prefix_b)).expect("read b");

        WalletApp::delete_dump(&prefix_a).expect("cleanup a");
        WalletApp::delete_dump(&prefix_b).expect("cleanup b");

        assert_eq!(
            bytes_a, bytes_b,
            "create_dump must produce byte-identical files for identical logical state"
        );
    }

    #[test]
    fn from_dump_rejects_malformed_bytes() {
        let prefix = temp_dump_prefix();
        std::fs::create_dir_all(&prefix).expect("mkdir");
        std::fs::write(WalletApp::state_file_in_dump(&prefix), [0x01, 0x02, 0x03])
            .expect("write malformed");

        let err = WalletApp::from_dump(&prefix).expect_err("invalid bytes should fail");
        WalletApp::delete_dump(&prefix).expect("cleanup");

        match err {
            AppError::Internal { reason } => assert!(
                reason.contains("snapshot decode failed"),
                "expected decode error, got: {reason}"
            ),
            other => panic!("expected Internal decode error, got {other:?}"),
        }
    }

    #[test]
    fn from_snapshot_bytes_rejects_duplicate_balance_addresses() {
        // Hand-crafted snapshot with two entries for the same address.
        // The encoder never produces this, but the decoder must reject it
        // to keep snapshot bytes canonical: without this check, multiple
        // distinct byte sequences could decode to the same logical state.
        let snapshot = super::WalletSnapshotV1 {
            erc20_portal_address: [0; 20],
            supported_erc20_token: [0; 20],
            sequencer_address: [0; 20],
            balances: vec![
                super::SnapshotBalance {
                    address: [1; 20],
                    balance_be: [0; 32],
                },
                super::SnapshotBalance {
                    address: [1; 20],
                    balance_be: [0; 32],
                },
            ],
            nonces: vec![],
            executed_input_count: 0,
        };
        let bytes = ssz::Encode::as_ssz_bytes(&snapshot);

        let err =
            WalletApp::from_snapshot_bytes(&bytes).expect_err("duplicate balance should reject");
        match err {
            AppError::Internal { reason } => assert!(
                reason.contains("duplicate balance"),
                "expected duplicate-balance error, got: {reason}"
            ),
            other => panic!("expected Internal duplicate error, got {other:?}"),
        }
    }

    #[test]
    fn from_snapshot_bytes_rejects_duplicate_nonce_addresses() {
        let snapshot = super::WalletSnapshotV1 {
            erc20_portal_address: [0; 20],
            supported_erc20_token: [0; 20],
            sequencer_address: [0; 20],
            balances: vec![],
            nonces: vec![
                super::SnapshotNonce {
                    address: [1; 20],
                    nonce: 0,
                },
                super::SnapshotNonce {
                    address: [1; 20],
                    nonce: 1,
                },
            ],
            executed_input_count: 0,
        };
        let bytes = ssz::Encode::as_ssz_bytes(&snapshot);

        let err =
            WalletApp::from_snapshot_bytes(&bytes).expect_err("duplicate nonce should reject");
        match err {
            AppError::Internal { reason } => assert!(
                reason.contains("duplicate nonce"),
                "expected duplicate-nonce error, got: {reason}"
            ),
            other => panic!("expected Internal duplicate error, got {other:?}"),
        }
    }

    #[test]
    fn state_file_in_dump_lives_inside_prefix() {
        let prefix = PathBuf::from("/tmp/example-prefix");
        let state = WalletApp::state_file_in_dump(&prefix);
        assert!(
            state.starts_with(&prefix),
            "state file path {state:?} must live under prefix {prefix:?}"
        );
    }

    fn temp_dump_prefix() -> PathBuf {
        // Combine pid + nanos + a process-local counter so concurrent tests
        // never collide on the same prefix path, even within a single nanosecond.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("wallet-dump-{}-{nanos}-{n}", std::process::id()));
        path
    }
}
