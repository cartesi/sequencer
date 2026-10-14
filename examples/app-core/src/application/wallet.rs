// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, U256, address};
use rollups_types::alloy_sol_types::SolCall;
use rollups_types::{Erc20Deposit, Erc20Transfer};
use ssz::Decode;
use tracing::{error, warn};

use super::MAX_METHOD_PAYLOAD_BYTES as WALLET_MAX_METHOD_PAYLOAD_BYTES;
use super::Method;
use super::{DepositNotice, TransferNotice};
use sequencer_core::application::{
    AppError, AppOutput, AppOutputs, Application, ApplicationProgress, CanonicalState,
    InvalidReason, ValidationOutcome,
};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::ValidUserOp;
use sequencer_core::user_op::UserOp;

#[derive(Debug, Clone, Copy)]
pub struct WalletConfig {
    pub erc20_portal_address: Address,
    pub supported_erc20_token: Address,
    /// Address credited with fee revenue.
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

#[derive(Debug, Clone)]
pub struct WalletApp {
    config: WalletConfig,
    // BTreeMap: iteration is ascending address bytes, the snapshot's
    // canonical entry order.
    balances: BTreeMap<Address, U256>,
    nonces: BTreeMap<Address, u32>,
    execution_progress: ApplicationProgress,
}

/// Rollups-contracts v3.0.0-alpha.10 Erc20Portal. The contracts deploy at
/// deterministic addresses, identical on every chain — this same value serves
/// Sepolia and the devnet Anvil dump (which is why `devnet()` reuses it).
pub const SEPOLIA_ERC20_PORTAL_ADDRESS: Address =
    address!("0x3332DE61a8BB9aC84893b2f552Fe81C9a6dC5419");
pub const SEPOLIA_USDC_ADDRESS: Address = address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
pub const DEVNET_MOCK_USDC_ADDRESS: Address =
    address!("0x95d0c8A7d11342299807A2Fc19ac44C2321cCc68");
pub const SEPOLIA_SEQUENCER_ADDRESS: Address =
    address!("0x16d5FF3Fdd14e2a86FBA77cbcE6B3Cd9C32b8Ff3");
/// Devnet batch-submitter / sequencer address — anvil account **9**,
/// deliberately distinct from the deployer/funder (anvil account 0).
///
/// A dedicated submitter is a load-bearing assumption of `setup`'s detection
/// gate: step 1 refuses when the submitter's wallet nonce is unsettled
/// (`pending > safe`), so the submitter must not share a key with the
/// deployer or with an e2e wallet whose transactions may be in flight.
/// Account 9 starts at nonce 0. Kept in sync
/// with the harness submitter key (`tests/harness` uses `default_private_keys`
/// index 9) and `canonical-test`'s batch sender.
pub const DEVNET_SEQUENCER_ADDRESS: Address =
    address!("0xa0Ee7A142d267C1f36714E4a8F75612F20a79720");
impl WalletApp {
    pub fn new(config: WalletConfig) -> Self {
        Self {
            config,
            balances: BTreeMap::new(),
            nonces: BTreeMap::new(),
            execution_progress: ApplicationProgress::default(),
        }
    }

    /// Reconstruct from decoded snapshot parts. Used by `crate::wallet_snapshot::decode`.
    ///
    /// The progress pair comes from untrusted dump bytes, so an incoherent
    /// pair is a typed decode error like every other corrupt-snapshot case —
    /// not a panic escaping `from_dump`'s `Result`.
    pub(crate) fn from_snapshot_parts(
        config: WalletConfig,
        balances: BTreeMap<Address, U256>,
        nonces: BTreeMap<Address, u32>,
        executed_input_count: u64,
        last_executed_safe_block: u64,
    ) -> Result<Self, AppError> {
        let execution_progress = ApplicationProgress::try_new(
            ExecutedInputCount::new(executed_input_count),
            last_executed_safe_block,
        )
        .ok_or_else(|| AppError::Internal {
            reason: format!(
                "snapshot progress is incoherent: zero executed inputs with \
                 nonzero safe-block clock {last_executed_safe_block}"
            ),
        })?;
        Ok(Self {
            config,
            balances,
            nonces,
            execution_progress,
        })
    }

    // Accessors for the canonical snapshot encoder (`crate::wallet_snapshot`).
    pub(crate) fn config(&self) -> &WalletConfig {
        &self.config
    }

    pub(crate) fn balances_iter(&self) -> impl Iterator<Item = (&Address, &U256)> {
        self.balances.iter()
    }

    pub(crate) fn nonces_iter(&self) -> impl Iterator<Item = (&Address, &u32)> {
        self.nonces.iter()
    }

    #[cfg(test)]
    pub(crate) fn balances_mut(&mut self) -> &mut BTreeMap<Address, U256> {
        &mut self.balances
    }

    #[cfg(test)]
    pub(crate) fn nonces_mut(&mut self) -> &mut BTreeMap<Address, u32> {
        &mut self.nonces
    }

    #[cfg(test)]
    pub(crate) fn set_executed_input_count(&mut self, count: u64) {
        self.execution_progress = ApplicationProgress::try_new(
            ExecutedInputCount::new(count),
            self.execution_progress.last_executed_safe_block(),
        )
        .expect("coherent progress");
    }

    fn balance_of(&self, addr: &Address) -> U256 {
        *self.balances.get(addr).unwrap_or(&U256::ZERO)
    }

    // Wallet-specific read queries (not on the Application trait — the
    // sequencer never asks: it derives nonces from persisted ops under the
    // contract's nonce rule, and app-specific query surface belongs to the app).
    pub fn current_user_nonce(&self, sender: Address) -> u32 {
        self.expected_nonce(&sender)
    }

    pub fn current_user_balance(&self, sender: Address) -> U256 {
        self.balance_of(&sender)
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
        let next = self
            .expected_nonce(&addr)
            .checked_add(1)
            .expect("wallet nonce overflow: no canonical successor");
        self.nonces.insert(addr, next);
    }
}

impl Default for WalletApp {
    fn default() -> Self {
        Self::new(WalletConfig::default())
    }
}

impl Application for WalletApp {
    fn max_method_payload_bytes() -> usize {
        WALLET_MAX_METHOD_PAYLOAD_BYTES
    }

    fn validate_user_op(
        &self,
        sender: Address,
        user_op: &UserOp,
        current_fee: u16,
    ) -> Result<ValidationOutcome, AppError> {
        let expected_nonce = self.expected_nonce(&sender);
        if user_op.nonce != expected_nonce {
            return Ok(ValidationOutcome::Reject(InvalidReason::InvalidNonce {
                expected: expected_nonce,
                got: user_op.nonce,
            }));
        }

        // max_fee < current_fee is already checked by the free function
        // validate_and_execute_user_op. No need to repeat here.

        let fee_cost = sequencer_core::fee::fee_to_linear(current_fee);
        let balance = self.balance_of(&sender);
        if balance < fee_cost {
            return Ok(ValidationOutcome::Reject(
                InvalidReason::InsufficientFeeBalance {
                    required: fee_cost,
                    available: balance,
                },
            ));
        }

        Ok(ValidationOutcome::Accept)
    }

    fn apply_valid_user_op(
        &mut self,
        user_op: &ValidUserOp,
        safe_block: u64,
    ) -> Result<AppOutputs, AppError> {
        let sender = user_op.sender;
        let fee_cost = sequencer_core::fee::fee_to_linear(user_op.fee);
        let balance = self.balance_of(&sender);
        if balance < fee_cost {
            return Err(AppError::Internal {
                reason: "validated user op cannot pay fee".to_string(),
            });
        }

        self.bump_nonce(sender);
        self.balances.insert(sender, balance - fee_cost);
        self.credit(self.config.sequencer_address, fee_cost);
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

        self.execution_progress.advance(safe_block);
        Ok(outputs)
    }

    fn apply_direct_input(
        &mut self,
        input: &sequencer_core::l2_tx::DirectInput,
    ) -> Result<AppOutputs, AppError> {
        let mut outputs = Vec::new();
        let deposit = (input.sender == self.config.erc20_portal_address)
            .then(|| Erc20Deposit::decode(&input.payload))
            .transpose();
        match deposit {
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

        self.execution_progress.advance(input.block_number);
        Ok(outputs)
    }

    fn progress(&self) -> ApplicationProgress {
        self.execution_progress
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let state_path = Self::state_file_in_dump(prefix);
        let bytes = std::fs::read(&state_path)?;
        crate::wallet_snapshot::decode(&bytes)
    }

    fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError> {
        // `create_dir` (not `create_dir_all`) deliberately errors if the
        // prefix already exists. Snapshot prefixes are expected to be
        // unique per call; a collision means a lane bug worth surfacing
        // loudly rather than silently overwriting prior state.
        std::fs::create_dir(prefix)?;
        let bytes = crate::wallet_snapshot::encode(self);

        let state_path = Self::state_file_in_dump(prefix);
        let mut file = std::fs::File::create(&state_path)?;
        file.write_all(&bytes)?;
        // fsync the file's contents, then walk up the parents to make
        // the new directory entries durable. Without this, the OS may
        // flush the SQLite WAL containing the pending-dump row to disk
        // ahead of our file contents/dentries, leaving a SQLite row
        // pointing at a path that the next crash-recovery sees as
        // missing.
        file.sync_all()?;
        // prefix/ contains state's dentry.
        std::fs::File::open(prefix)?.sync_all()?;
        // dumps_dir/ contains prefix's dentry.
        if let Some(parent) = prefix.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    fn state_file_in_dump(prefix: &Path) -> PathBuf {
        prefix.join("state")
    }
}

impl CanonicalState for WalletApp {
    fn canonical_snapshot_bytes(&self) -> Result<Vec<u8>, AppError> {
        Ok(crate::wallet_snapshot::encode(self))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use alloy_primitives::{Address, U256, address};
    use rollups_types::alloy_sol_types::SolCall;
    use rollups_types::{Erc20Deposit, Erc20Transfer};

    use super::{ApplicationProgress, ExecutedInputCount, WalletApp, WalletConfig};
    use crate::application::{DepositNotice, Transfer, TransferNotice, Withdrawal};
    use sequencer_core::application::{
        AppError, AppOutput, Application, CanonicalState, InvalidReason, ValidationOutcome,
    };
    use sequencer_core::application::{
        ExecutionOutcome, execute_direct_input, execute_valid_user_op, validate_and_execute_user_op,
    };
    use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
    use sequencer_core::user_op::UserOp;

    #[test]
    fn validation_is_repeatable_and_preserves_the_entire_state() {
        let mut app = WalletApp::default();
        let sender = Address::from_slice(&[0x11; 20]);
        app.balances.insert(sender, U256::from(10_u64));
        let before = app.canonical_snapshot_bytes().unwrap();
        let mut user_op = UserOp {
            nonce: 0,
            max_fee: 0,
            data: Vec::new().into(),
        };

        for _ in 0..2 {
            assert_eq!(
                app.validate_user_op(sender, &user_op, 0).unwrap(),
                ValidationOutcome::Accept
            );
            user_op.nonce = 1;
            assert_eq!(
                app.validate_user_op(sender, &user_op, 0).unwrap(),
                ValidationOutcome::Reject(InvalidReason::InvalidNonce {
                    expected: 0,
                    got: 1
                })
            );
            user_op.nonce = 0;
            assert!(matches!(
                app.validate_user_op(Address::ZERO, &user_op, 0).unwrap(),
                ValidationOutcome::Reject(InvalidReason::InsufficientFeeBalance { .. })
            ));
            assert_eq!(app.canonical_snapshot_bytes().unwrap(), before);
        }
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
        let outputs = execute_valid_user_op(&mut app, &valid, 123)
            .expect("execute valid op")
            .outputs;

        assert_eq!(app.current_user_nonce(sender), 1);
        assert_eq!(app.current_user_balance(sender), initial_balance - gas_cost);
        assert_eq!(app.executed_input_count().get(), 1);
        assert_eq!(app.last_executed_safe_block(), 123);
        assert!(outputs.is_empty());
    }

    fn encode_erc20_deposit_payload(token: Address, sender: Address, value: U256) -> Vec<u8> {
        Erc20Deposit {
            token,
            sender,
            value,
            exec_layer_data: Vec::new(),
        }
        .encode()
    }

    #[test]
    fn trusted_portal_usdc_deposit_credits_nested_sender() {
        let mut app = WalletApp::new(WalletConfig::default());
        let nested_sender = address!("0x7777777777777777777777777777777777777777");

        let before = app.current_user_balance(nested_sender);
        let outputs = execute_direct_input(
            &mut app,
            &DirectInput {
                sender: super::SEPOLIA_ERC20_PORTAL_ADDRESS,
                block_number: 123,
                payload: encode_erc20_deposit_payload(
                    super::SEPOLIA_USDC_ADDRESS,
                    nested_sender,
                    U256::from(250_u64),
                ),
            },
        )
        .expect("execute deposit direct input")
        .outputs;

        assert_eq!(
            app.current_user_balance(nested_sender),
            before + U256::from(250_u64)
        );
        assert_eq!(app.executed_input_count().get(), 1);
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
        let outputs = execute_direct_input(
            &mut app,
            &DirectInput {
                sender: address!("0x3333333333333333333333333333333333333333"),
                block_number: 123,
                payload: encode_erc20_deposit_payload(
                    super::SEPOLIA_USDC_ADDRESS,
                    nested_sender,
                    U256::from(250_u64),
                ),
            },
        )
        .expect("execute non-portal direct input")
        .outputs;

        assert_eq!(app.current_user_balance(nested_sender), before);
        assert_eq!(app.executed_input_count().get(), 1);
        assert!(outputs.is_empty());
    }

    #[test]
    fn trusted_portal_unsupported_token_is_a_no_op() {
        let mut app = WalletApp::new(WalletConfig::default());
        let nested_sender = address!("0x7777777777777777777777777777777777777777");
        let unsupported_token = address!("0x9999999999999999999999999999999999999999");

        let before = app.current_user_balance(nested_sender);
        let outputs = execute_direct_input(
            &mut app,
            &DirectInput {
                sender: super::SEPOLIA_ERC20_PORTAL_ADDRESS,
                block_number: 123,
                payload: encode_erc20_deposit_payload(
                    unsupported_token,
                    nested_sender,
                    U256::from(250_u64),
                ),
            },
        )
        .expect("unsupported token should be ignored")
        .outputs;

        assert_eq!(app.current_user_balance(nested_sender), before);
        assert_eq!(app.executed_input_count().get(), 1);
        assert!(outputs.is_empty());
    }

    #[test]
    fn malformed_trusted_portal_deposit_is_a_no_op() {
        let mut app = WalletApp::new(WalletConfig::default());

        let outputs = execute_direct_input(
            &mut app,
            &DirectInput {
                sender: super::SEPOLIA_ERC20_PORTAL_ADDRESS,
                block_number: 123,
                payload: vec![0xaa; 10],
            },
        )
        .expect("malformed trusted portal payload should be ignored")
        .outputs;

        assert_eq!(app.executed_input_count().get(), 1);
        assert_eq!(app.last_executed_safe_block(), 123);
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

        let outputs = execute_valid_user_op(&mut app, &valid, 0)
            .expect("execute transfer")
            .outputs;

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

        let outputs = execute_valid_user_op(&mut app, &valid, 0)
            .expect("execute withdrawal")
            .outputs;

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
    fn business_failures_after_fee_validation_charge_fee_and_consume_nonce() {
        let sequencer = address!("0x9999999999999999999999999999999999999999");
        let mut app = WalletApp::new(WalletConfig {
            sequencer_address: sequencer,
            ..WalletConfig::default()
        });
        let sender = address!("0x1111111111111111111111111111111111111111");
        let recipient = address!("0x2222222222222222222222222222222222222222");
        let funded = U256::from(100_u64);
        app.balances.insert(sender, funded);
        let fee: u16 = 10;
        let fee_cost = sequencer_core::fee::fee_to_linear(fee);
        let too_much = U256::from(1_000_u64);
        // Selector 2 was the retired `Deposit { amount, to }` method, laid out
        // like `Transfer`; an op shaped like it must not mint to `to`.
        let mut retired_deposit = vec![2];
        retired_deposit.extend(ssz::Encode::as_ssz_bytes(&Transfer {
            amount: too_much,
            to: recipient,
        }));
        let mut replay = app.clone();

        for (i, data) in [
            ssz::Encode::as_ssz_bytes(&super::Method::Transfer(Transfer {
                amount: too_much,
                to: recipient,
            })),
            ssz::Encode::as_ssz_bytes(&super::Method::Withdrawal(Withdrawal { amount: too_much })),
            retired_deposit,
        ]
        .into_iter()
        .enumerate()
        {
            let nonce = u32::try_from(i).unwrap();
            let safe_block = 10 + u64::from(nonce);
            let user_op = UserOp {
                nonce,
                max_fee: fee,
                data: data.clone().into(),
            };
            let ExecutionOutcome::Included(live) =
                validate_and_execute_user_op(&mut app, sender, &user_op, fee, safe_block).unwrap()
            else {
                panic!("a fee-funded op is included even when its method fails");
            };
            // Restart catch-up replays the persisted op without validation.
            let replayed =
                execute_valid_user_op(&mut replay, &ValidUserOp { sender, fee, data }, safe_block)
                    .unwrap();
            assert_eq!(replayed, live);

            assert!(live.outputs.is_empty());
            let charged = fee_cost * U256::from(nonce + 1);
            assert_eq!(app.current_user_nonce(sender), nonce + 1);
            assert_eq!(app.current_user_balance(sender), funded - charged);
            assert_eq!(app.current_user_balance(sequencer), charged);
            assert_eq!(app.current_user_balance(recipient), U256::ZERO);
            assert_eq!(app.executed_input_count().get(), u64::from(nonce) + 1);
            assert_eq!(app.last_executed_safe_block(), safe_block);
        }
        assert_eq!(
            replay.canonical_snapshot_bytes().unwrap(),
            app.canonical_snapshot_bytes().unwrap()
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
        execute_valid_user_op(&mut app, &valid, 0).expect("execute op");

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
        app.execution_progress = ApplicationProgress::try_new(ExecutedInputCount::new(42), 777)
            .expect("coherent progress");

        let prefix = temp_dump_prefix();
        app.create_dump(&prefix).expect("create dump");
        assert!(WalletApp::state_file_in_dump(&prefix).starts_with(&prefix));

        let restored = WalletApp::from_dump(&prefix).expect("load dump");

        std::fs::remove_dir_all(&prefix).expect("cleanup dump");

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
        assert_eq!(restored.execution_progress, app.execution_progress);
    }

    #[test]
    fn dump_and_loaded_instances_have_independent_mutable_state() {
        let mut live = WalletApp::default();
        let sender = Address::from_slice(&[0x11; 20]);
        live.balances.insert(sender, U256::from(10_u64));
        let before = live.canonical_snapshot_bytes().unwrap();
        let prefix = temp_dump_prefix();
        live.create_dump(&prefix).unwrap();
        assert_eq!(live.canonical_snapshot_bytes().unwrap(), before);
        assert_eq!(
            std::fs::read(WalletApp::state_file_in_dump(&prefix)).unwrap(),
            before
        );

        let mut first = WalletApp::from_dump(&prefix).unwrap();
        let second = WalletApp::from_dump(&prefix).unwrap();
        let user_op = ValidUserOp {
            sender,
            fee: 0,
            data: Vec::new(),
        };
        execute_valid_user_op(&mut first, &user_op, 11).unwrap();
        assert_ne!(first.canonical_snapshot_bytes().unwrap(), before);
        assert_eq!(live.canonical_snapshot_bytes().unwrap(), before);
        execute_valid_user_op(&mut live, &user_op, 12).unwrap();
        assert_ne!(live.canonical_snapshot_bytes().unwrap(), before);
        assert_eq!(second.canonical_snapshot_bytes().unwrap(), before);
        assert_eq!(
            std::fs::read(WalletApp::state_file_in_dump(&prefix)).unwrap(),
            before
        );
        std::fs::remove_dir_all(&prefix).unwrap();
        execute_valid_user_op(&mut first, &user_op, 13).unwrap();
        assert_eq!(first.executed_input_count().get(), 2);
    }

    #[test]
    fn safe_block_clock_advances_by_max_on_both_execution_paths() {
        let mut app = WalletApp::new(WalletConfig::default());
        let sender = Address::from_slice(&[0x77; 20]);
        app.balances.insert(sender, U256::from(10_000_u64));
        assert_eq!(app.last_executed_safe_block(), 0);

        // User op carries its covering frame's safe block.
        let valid = ValidUserOp {
            sender,
            fee: 0,
            data: Vec::new(),
        };
        execute_valid_user_op(&mut app, &valid, 100).expect("execute op");
        assert_eq!(app.last_executed_safe_block(), 100);

        // A direct input advances the clock via its own inclusion block.
        let direct = sequencer_core::l2_tx::DirectInput {
            sender: Address::from_slice(&[0x88; 20]),
            block_number: 150,
            payload: Vec::new(),
        };
        execute_direct_input(&mut app, &direct).expect("execute direct");
        assert_eq!(app.last_executed_safe_block(), 150);

        // max(): an older block must never regress the clock. A direct's
        // inclusion block is <= its covering frame's safe block, so replays
        // legitimately present blocks below the current clock.
        let older_direct = sequencer_core::l2_tx::DirectInput {
            sender: Address::from_slice(&[0x88; 20]),
            block_number: 120,
            payload: Vec::new(),
        };
        execute_direct_input(&mut app, &older_direct).expect("execute older direct");
        assert_eq!(app.last_executed_safe_block(), 150);
    }

    #[test]
    fn from_dump_rejects_malformed_bytes() {
        let prefix = temp_dump_prefix();
        std::fs::create_dir_all(&prefix).expect("mkdir");
        std::fs::write(WalletApp::state_file_in_dump(&prefix), [0x01, 0x02, 0x03])
            .expect("write malformed");

        let err = WalletApp::from_dump(&prefix).expect_err("invalid bytes should fail");
        std::fs::remove_dir_all(&prefix).expect("cleanup");

        match err {
            AppError::Internal { reason } => assert!(
                reason.contains("snapshot decode failed"),
                "expected decode error, got: {reason}"
            ),
            other => panic!("expected Internal decode error, got {other:?}"),
        }
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

    const CONSENSUS_VECTORS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/consensus_vectors_v1.json"
    ));

    /// The guest emits these payloads as notices and voucher calldata, so their
    /// bytes, selector included, are consensus. Pinned once and never
    /// regenerated: a mismatch is a finding.
    #[test]
    fn output_encodings_match_the_pinned_bytes() {
        let vectors: serde_json::Value =
            serde_json::from_str(CONSENSUS_VECTORS).expect("parse consensus_vectors_v1.json");
        let pinned = &vectors["wallet_outputs"];
        let text = |value: &serde_json::Value| value.as_str().expect("pinned string").to_owned();
        let address = |key: &str| {
            text(&pinned[key])
                .parse::<Address>()
                .expect("pinned address")
        };
        let bytes = |value: &serde_json::Value| {
            alloy_primitives::hex::decode(text(value)).expect("pinned hex bytes")
        };
        let (token, sender, recipient) =
            (address("token"), address("sender"), address("recipient"));

        let cases = pinned["cases"].as_array().expect("pinned cases");
        assert_eq!(cases.len(), 3);
        for case in cases {
            let amount: U256 = text(&case["amount"]).parse().expect("pinned amount");
            assert_eq!(
                DepositNotice {
                    token,
                    sender,
                    amount
                }
                .abi_encode(),
                bytes(&case["deposit_notice"]),
                "DepositNotice, amount {amount:#x}"
            );
            assert_eq!(
                TransferNotice {
                    sender,
                    recipient,
                    amount
                }
                .abi_encode(),
                bytes(&case["transfer_notice"]),
                "TransferNotice, amount {amount:#x}"
            );
            assert_eq!(
                Erc20Transfer { recipient, amount }.abi_encode(),
                bytes(&case["erc20_transfer"]),
                "Erc20Transfer, amount {amount:#x}"
            );
        }
    }
}
