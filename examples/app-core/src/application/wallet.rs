// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, U256, address};
use ssz::Decode;
use tracing::{error, warn};
use types::alloy_sol_types::SolCall;
use types::{Erc20Deposit, Erc20Transfer};

use super::MAX_METHOD_PAYLOAD_BYTES as WALLET_MAX_METHOD_PAYLOAD_BYTES;
use super::Method;
use super::{DepositNotice, TransferNotice};
use sequencer_core::application::{
    AppError, AppOutput, AppOutputs, Application, ApplicationProgress, ApplyInputCapability,
    InvalidReason, ProgressCommitCapability,
};
use sequencer_core::history::ExecutedInputCount;
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

#[derive(Debug, Clone)]
pub struct WalletApp {
    config: WalletConfig,
    balances: HashMap<Address, U256>,
    nonces: HashMap<Address, u32>,
    execution_progress: ApplicationProgress,
}

/// Rollups-contracts v3.0.0-alpha.6 ERC20Portal. The contracts deploy at
/// deterministic addresses, identical on every chain — this same value serves
/// Sepolia and the devnet Anvil dump (which is why `devnet()` reuses it).
pub const SEPOLIA_ERC20_PORTAL_ADDRESS: Address =
    address!("0x22E57511C30CcE6CDaa742E13CE3b774fDC663b1");
pub const SEPOLIA_USDC_ADDRESS: Address = address!("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
pub const DEVNET_MOCK_USDC_ADDRESS: Address =
    address!("0x95d0c8A7d11342299807A2Fc19ac44C2321cCc68");
pub const SEPOLIA_SEQUENCER_ADDRESS: Address =
    address!("0x16d5FF3Fdd14e2a86FBA77cbcE6B3Cd9C32b8Ff3");
/// Devnet batch-submitter / sequencer address — anvil account **9**,
/// deliberately distinct from the deployer/funder (anvil account 0).
///
/// A dedicated submitter is a load-bearing assumption of `setup`'s detection
/// gate: step 1 refuses when the submitter's wallet nonce is
/// unsettled (`pending > safe`). The deployer has a non-zero nonce from
/// contract creations whose tail isn't safe at setup time, so reusing it as
/// the submitter false-positives. Account 9 starts at nonce 0. Kept in sync
/// with the harness submitter key (`tests/harness` uses `default_private_keys`
/// index 9) and `canonical-test`'s batch sender.
pub const DEVNET_SEQUENCER_ADDRESS: Address =
    address!("0xa0Ee7A142d267C1f36714E4a8F75612F20a79720");
impl WalletApp {
    pub fn new(config: WalletConfig) -> Self {
        Self {
            config,
            balances: HashMap::new(),
            nonces: HashMap::new(),
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
        balances: HashMap<Address, U256>,
        nonces: HashMap<Address, u32>,
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
    pub(crate) fn balances_mut(&mut self) -> &mut HashMap<Address, U256> {
        &mut self.balances
    }

    #[cfg(test)]
    pub(crate) fn nonces_mut(&mut self) -> &mut HashMap<Address, u32> {
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

    pub fn last_executed_safe_block(&self) -> u64 {
        self.execution_progress.last_executed_safe_block()
    }

    /// Deterministic JSON of the non-default logical state (debug only).
    fn state_json(&self) -> String {
        let mut balances: Vec<_> = self
            .balances
            .iter()
            .filter(|(_, balance)| **balance != U256::ZERO)
            .collect();
        balances.sort_by_key(|(address, _)| address.as_slice());

        let mut nonces: Vec<_> = self
            .nonces
            .iter()
            .filter(|(_, nonce)| **nonce != 0)
            .collect();
        nonces.sort_by_key(|(address, _)| address.as_slice());

        let balance_entries = balances
            .into_iter()
            .map(|(address, balance)| format!("\"{}\":\"{balance}\"", json_address(address)))
            .collect::<Vec<_>>()
            .join(",");
        let nonce_entries = nonces
            .into_iter()
            .map(|(address, nonce)| format!("\"{}\":{nonce}", json_address(address)))
            .collect::<Vec<_>>()
            .join(",");

        format!("{{\"balances\":{{{balance_entries}}},\"nonces\":{{{nonce_entries}}}}}")
    }

    fn balance_of(&self, addr: &Address) -> U256 {
        *self.balances.get(addr).unwrap_or(&U256::ZERO)
    }

    // Wallet-specific read queries (not on the Application trait — the
    // sequencer never asks; app-specific query surface belongs to the app).
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

    fn decode_portal_erc20_deposit(
        portal_address: Address,
        input: &sequencer_core::l2_tx::DirectInput,
    ) -> Result<Option<Erc20Deposit>, types::Erc20DepositDecodeError> {
        if input.sender != portal_address {
            return Ok(None);
        }

        Erc20Deposit::decode(&input.payload).map(Some)
    }
}

fn json_address(address: &Address) -> String {
    format!("0x{}", alloy_primitives::hex::encode(address.as_slice()))
}

impl Default for WalletApp {
    fn default() -> Self {
        Self::new(WalletConfig::default())
    }
}

impl Application for WalletApp {
    const MAX_METHOD_PAYLOAD_BYTES: usize = WALLET_MAX_METHOD_PAYLOAD_BYTES;

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

        // max_fee < current_fee is already checked by the free function
        // validate_and_execute_user_op. No need to repeat here.

        let fee_cost = sequencer_core::fee::fee_to_linear(current_fee);
        let balance = self.balance_of(&sender);
        if balance < fee_cost {
            return Err(InvalidReason::InsufficientFeeBalance {
                required: fee_cost,
                available: balance,
            });
        }

        Ok(())
    }

    fn apply_valid_user_op(
        &mut self,
        _capability: ApplyInputCapability<'_>,
        user_op: &ValidUserOp,
        _safe_block: u64,
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

        Ok(outputs)
    }

    fn apply_direct_input(
        &mut self,
        _capability: ApplyInputCapability<'_>,
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

        Ok(outputs)
    }

    fn execution_progress(&self) -> &ApplicationProgress {
        &self.execution_progress
    }

    fn execution_progress_mut(
        &mut self,
        _capability: ProgressCommitCapability<'_>,
    ) -> &mut ApplicationProgress {
        &mut self.execution_progress
    }

    fn canonical_snapshot_bytes(&self) -> Result<Vec<u8>, AppError> {
        Ok(crate::wallet_snapshot::encode(self))
    }

    fn export_state(&self) -> Result<String, AppError> {
        Ok(self.state_json())
    }

    fn from_dump(prefix: &Path) -> Result<Self, AppError> {
        let state_path = Self::state_file_in_dump(prefix);
        let bytes = std::fs::read(&state_path)?;
        crate::wallet_snapshot::decode(&bytes)
    }

    fn create_dump(&self, prefix: &Path) -> Result<(), AppError> {
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

    use super::{ApplicationProgress, ExecutedInputCount, WalletApp, WalletConfig};
    use crate::application::{DepositNotice, Transfer, TransferNotice, Withdrawal};
    use sequencer_core::application::{AppError, AppOutput, Application, InvalidReason};
    use sequencer_core::application::{execute_direct_input, execute_valid_user_op};
    use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
    use sequencer_core::user_op::UserOp;

    #[test]
    fn validate_rejects_when_max_fee_below_current_fee() {
        use sequencer_core::application::{ExecutionOutcome, validate_and_execute_user_op};

        let mut app = WalletApp::new(WalletConfig::default());
        let sender = Address::from_slice(&[0x11; 20]);
        app.balances.insert(sender, U256::from(10_u64));

        let user_op = UserOp {
            nonce: 0,
            max_fee: 1,
            data: Vec::<u8>::new().into(),
        };

        // The max_fee < current_fee check lives in the free function
        // validate_and_execute_user_op, not in validate_user_op directly.
        let result = validate_and_execute_user_op(&mut app, sender, &user_op, 2, 0)
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
        let outputs = execute_valid_user_op(&mut app, &valid, 0)
            .expect("execute valid op")
            .outputs;

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

        let outputs = execute_valid_user_op(&mut app, &valid, 0)
            .expect("execute valid user op")
            .outputs;

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
        execute_valid_user_op(&mut app, &valid, 0).expect("execute op");

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
        app.execution_progress = ApplicationProgress::try_new(ExecutedInputCount::new(42), 777)
            .expect("coherent progress");

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
        assert_eq!(restored.execution_progress, app.execution_progress);
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
        app.set_executed_input_count(99);

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
