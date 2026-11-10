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
    Admission, AppError, AppOutput, AppOutputs, Application, CanonicalState, PolicyRefusal,
    SchedulerLimits,
};
use sequencer_core::history::ExecutedInputCount;
use sequencer_core::l2_tx::{Direct, UserOpTx};
use sequencer_core::user_op::UserOp;

/// The wallet template's scheduler constants. Provisional: a deposit's payload
/// is 72 bytes plus its exec-layer data, so 1 KiB leaves that data room, and a
/// full fridge then holds at most 16 MiB of payloads.
pub const WALLET_SCHEDULER_LIMITS: SchedulerLimits = SchedulerLimits {
    fridge_bound: 16_384,
    max_direct_payload_bytes: 1024,
};

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
    executed_input_count: ExecutedInputCount,
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
    address!("0x79b88d2f598d8D001832D69f5201332A3f1e14c8");
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
            executed_input_count: ExecutedInputCount::ZERO,
        }
    }

    /// Reconstruct from decoded snapshot parts. Used by `crate::wallet_snapshot::decode`.
    pub(crate) fn from_snapshot_parts(
        config: WalletConfig,
        balances: BTreeMap<Address, U256>,
        nonces: BTreeMap<Address, u32>,
        executed_input_count: u64,
    ) -> Self {
        Self {
            config,
            balances,
            nonces,
            executed_input_count: ExecutedInputCount::new(executed_input_count),
        }
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
        self.executed_input_count = ExecutedInputCount::new(count);
    }

    fn balance_of(&self, addr: &Address) -> U256 {
        *self.balances.get(addr).unwrap_or(&U256::ZERO)
    }

    // Wallet-specific read queries for tests and tooling. The sequencer reads
    // the nonce through `Application::user_nonce`, which answers the same.
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

    /// The shared executor skips an op whose sender is at `u32::MAX` before it
    /// reaches the wallet, so overflow here is an impossible state.
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
    fn user_nonce(&self, sender: Address) -> Result<u32, AppError> {
        Ok(self.expected_nonce(&sender))
    }

    /// Charges what the sender has, up to the fee, and credits the sequencer
    /// exactly that, so an underfunded op neither fails nor mints. The method
    /// then runs on the remaining balance: one code path for every op.
    fn apply_user_op(&mut self, op: &UserOpTx) -> Result<AppOutputs, AppError> {
        let sender = op.sender;
        let fee_cost = sequencer_core::fee::fee_to_linear(op.fee);
        let balance = self.balance_of(&sender);
        let charged = balance.min(fee_cost);

        self.bump_nonce(sender);
        self.balances.insert(sender, balance - charged);
        self.credit(self.config.sequencer_address, charged);
        let mut outputs = Vec::new();

        let method = Method::from_ssz_bytes(op.data.as_slice()).ok();
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

        self.executed_input_count.advance();
        Ok(outputs)
    }

    fn apply_direct(&mut self, direct: &Direct) -> Result<AppOutputs, AppError> {
        let mut outputs = Vec::new();
        let deposit = (direct.sender == self.config.erc20_portal_address)
            .then(|| Erc20Deposit::decode(&direct.payload))
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
                        portal = %direct.sender,
                        token = %deposit.token,
                        sender = %deposit.sender,
                        input_index = direct.input_index,
                        block_number = direct.block,
                        "ignoring unsupported ERC-20 deposit token"
                    );
                }
            }
            Ok(None) => {}
            Err(reason) => {
                error!(
                    portal = %direct.sender,
                    input_index = direct.input_index,
                    block_number = direct.block,
                    error = %reason,
                    "ignoring malformed trusted ERC-20 deposit payload"
                );
            }
        }

        self.executed_input_count.advance();
        Ok(outputs)
    }

    fn executed_input_count(&self) -> ExecutedInputCount {
        self.executed_input_count
    }

    fn max_method_payload_bytes() -> usize {
        WALLET_MAX_METHOD_PAYLOAD_BYTES
    }

    fn scheduler_limits() -> SchedulerLimits {
        WALLET_SCHEDULER_LIMITS
    }

    /// Refuses an op the sender cannot pay for in full: it would pay the
    /// operator less than the L1 data it costs. The nonce is not checked here.
    fn admit_user_op(
        &self,
        sender: Address,
        _op: &UserOp,
        fee: u16,
    ) -> Result<Admission, AppError> {
        let required = sequencer_core::fee::fee_to_linear(fee);
        let available = self.balance_of(&sender);
        if available < required {
            return Ok(Admission::Refuse(PolicyRefusal::Underfunded {
                required,
                available,
            }));
        }
        Ok(Admission::Admit)
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

    /// The wallet's bytes hold its count, so a count that disagrees with the
    /// machine's means a corrupt or misread export, never a state to install.
    fn from_canonical_state(
        state_file: &Path,
        executed_input_count: ExecutedInputCount,
    ) -> Result<Self, AppError> {
        let bytes = std::fs::read(state_file)?;
        let app = crate::wallet_snapshot::decode(&bytes)?;
        if app.executed_input_count != executed_input_count {
            return Err(AppError::Internal {
                reason: format!(
                    "canonical state holds L2 tx count {}, but the machine reports {}",
                    app.executed_input_count.get(),
                    executed_input_count.get()
                ),
            });
        }
        Ok(app)
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

    use super::{ExecutedInputCount, WalletApp, WalletConfig};
    use crate::application::{DepositNotice, Transfer, TransferNotice, Withdrawal};
    use sequencer_core::application::{
        Admission, AppError, AppOutput, AppOutputs, Application, CanonicalState, Executed,
        PolicyRefusal, Refusal, SkipReason, check_live_user_op, execute,
    };
    use sequencer_core::l2_tx::{Direct, L2Tx, UserOpTx};
    use sequencer_core::user_op::UserOp;

    fn user_tx(sender: Address, nonce: u32, fee: u16, data: Vec<u8>) -> L2Tx {
        L2Tx::UserOp(UserOpTx {
            sender,
            nonce,
            fee,
            block: 123,
            data,
        })
    }

    fn direct_tx(sender: Address, payload: Vec<u8>) -> L2Tx {
        L2Tx::Direct(Direct {
            input_index: 7,
            sender,
            block: 123,
            timestamp: 1_700_000_000,
            payload,
        })
    }

    /// Run `tx` through the shared executor and return its outputs.
    fn executed(app: &mut WalletApp, tx: &L2Tx) -> AppOutputs {
        match execute(app, tx).expect("execute") {
            Executed::Done { outputs, .. } => outputs,
            Executed::Skipped(reason) => panic!("expected execution, skipped: {reason}"),
        }
    }

    fn total_supply(app: &WalletApp) -> U256 {
        app.balances.values().copied().sum()
    }

    fn transfer_data(amount: u64, to: Address) -> Vec<u8> {
        ssz::Encode::as_ssz_bytes(&super::Method::Transfer(Transfer {
            amount: U256::from(amount),
            to,
        }))
    }

    #[test]
    fn sepolia_dedicated_submitter_is_the_batch_sender_and_fee_recipient() {
        use sequencer_core::batch::Batch;
        use sequencer_core::scheduler::{InputOutcome, L1Input, Scheduler, SchedulerConfig};
        use ssz::Encode;

        let submitter = address!("0x79b88d2f598d8D001832D69f5201332A3f1e14c8");
        let previous_submitter = address!("0x16d5FF3Fdd14e2a86FBA77cbcE6B3Cd9C32b8Ff3");
        let config = WalletConfig::sepolia();
        assert_eq!(config.sequencer_address, submitter);
        assert_eq!(WalletConfig::default().sequencer_address, submitter);

        let mut scheduler = Scheduler::genesis(SchedulerConfig::for_application::<WalletApp>(
            super::SEPOLIA_SEQUENCER_ADDRESS,
        ));
        let input = |index, sender| L1Input {
            index,
            sender,
            block: 10,
            timestamp: 0,
            chain_id: 11155111,
            app_contract: Address::ZERO,
            payload: Batch {
                nonce: 0,
                frames: Vec::new(),
            }
            .as_ssz_bytes(),
        };

        let previous = scheduler.feed(input(0, previous_submitter));
        assert_eq!(previous, InputOutcome::DirectQueued);
        assert_eq!(scheduler.state().next_batch_nonce, 0);

        let current = scheduler.feed(input(1, submitter));
        assert_eq!(current, InputOutcome::BatchAccepted);
        assert_eq!(scheduler.state().next_batch_nonce, 1);
    }

    #[test]
    fn admission_is_repeatable_and_preserves_state() {
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
            // Admission ignores the nonce: the executor's rule comes first.
            for nonce in [0, 1] {
                user_op.nonce = nonce;
                assert_eq!(
                    app.admit_user_op(sender, &user_op, 0).unwrap(),
                    Admission::Admit
                );
            }
            assert!(matches!(
                app.admit_user_op(Address::ZERO, &user_op, 0).unwrap(),
                Admission::Refuse(PolicyRefusal::Underfunded { .. })
            ));
            assert_eq!(app.user_nonce(sender).unwrap(), 0);
            assert_eq!(app.canonical_snapshot_bytes().unwrap(), before);
        }
    }

    #[test]
    fn admission_refuses_an_underfunded_op_with_the_served_text() {
        let mut app = WalletApp::default();
        let sender = Address::from_slice(&[0x11; 20]);
        let fee: u16 = 1000;
        let required = sequencer_core::fee::fee_to_linear(fee);
        let available = required - U256::from(1_u64);
        app.balances.insert(sender, available);
        let user_op = UserOp {
            nonce: 0,
            max_fee: fee,
            data: Vec::new().into(),
        };

        let underfunded = PolicyRefusal::Underfunded {
            required,
            available,
        };
        assert_eq!(
            app.admit_user_op(sender, &user_op, fee).unwrap(),
            Admission::Refuse(underfunded.clone())
        );
        let refusal = check_live_user_op(&app, sender, &user_op, fee).unwrap();
        assert_eq!(refusal, Some(Refusal::Policy(underfunded)));
        assert_eq!(
            refusal.unwrap().to_string(),
            format!("insufficient balance for fee: required {required}, available {available}")
        );

        app.balances.insert(sender, required);
        assert_eq!(
            check_live_user_op(&app, sender, &user_op, fee).unwrap(),
            None
        );
    }

    #[test]
    fn user_nonce_reports_next_nonce() {
        let mut app = WalletApp::default();
        let sender = Address::from_slice(&[0x11; 20]);
        let other = Address::from_slice(&[0x22; 20]);
        assert_eq!(app.user_nonce(sender).unwrap(), 0);

        for expected in 1..=3 {
            executed(&mut app, &user_tx(sender, expected - 1, 0, Vec::new()));
            assert_eq!(app.user_nonce(sender).unwrap(), expected);
            assert_eq!(app.current_user_nonce(sender), expected);
        }
        assert_eq!(app.user_nonce(other).unwrap(), 0);
    }

    #[test]
    fn wrong_nonce_is_skipped_without_state_change() {
        let mut app = WalletApp::default();
        let sender = Address::from_slice(&[0x11; 20]);
        app.balances.insert(sender, U256::from(10_000_u64));
        let before = app.canonical_snapshot_bytes().unwrap();

        assert_eq!(
            execute(&mut app, &user_tx(sender, 1, 100, Vec::new())).unwrap(),
            Executed::Skipped(SkipReason::WrongNonce {
                expected: 0,
                got: 1
            })
        );
        assert_eq!(app.canonical_snapshot_bytes().unwrap(), before);
    }

    #[test]
    fn execute_user_op_charges_current_fee() {
        let mut app = WalletApp::new(WalletConfig::default());
        let sender = Address::from_slice(&[0x22; 20]);
        let initial_balance = U256::from(1000_u64);
        app.balances.insert(sender, initial_balance);

        // fee exponent 100 → fee_to_linear(100) ≈ 2 ((129/128)^100 ≈ 2.17, truncated)
        let fee_exponent: u16 = 100;
        let gas_cost = sequencer_core::fee::fee_to_linear(fee_exponent);
        let outputs = executed(&mut app, &user_tx(sender, 0, fee_exponent, Vec::new()));

        assert_eq!(app.current_user_nonce(sender), 1);
        assert_eq!(app.current_user_balance(sender), initial_balance - gas_cost);
        assert_eq!(app.executed_input_count().get(), 1);
        assert!(outputs.is_empty());
    }

    #[test]
    fn underfunded_op_executes_charging_what_the_sender_has() {
        let sequencer = address!("0x9999999999999999999999999999999999999999");
        let mut app = WalletApp::new(WalletConfig {
            sequencer_address: sequencer,
            ..WalletConfig::default()
        });
        let sender = address!("0x1111111111111111111111111111111111111111");
        let recipient = address!("0x2222222222222222222222222222222222222222");
        let fee: u16 = 1000;
        let fee_cost = sequencer_core::fee::fee_to_linear(fee);
        let balance = fee_cost - U256::from(1_u64);
        app.balances.insert(sender, balance);
        app.balances.insert(sequencer, U256::from(5_u64));

        let outputs = executed(
            &mut app,
            &user_tx(sender, 0, fee, transfer_data(1, recipient)),
        );

        assert!(outputs.is_empty(), "nothing is left to transfer");
        assert_eq!(app.current_user_balance(sender), U256::ZERO);
        assert_eq!(
            app.current_user_balance(sequencer),
            U256::from(5_u64) + balance,
            "the sequencer gains exactly what was charged"
        );
        assert_eq!(app.current_user_balance(recipient), U256::ZERO);
        assert_eq!(app.current_user_nonce(sender), 1);
        assert_eq!(app.executed_input_count().get(), 1);
    }

    #[test]
    fn underfunded_ops_conserve_total_supply() {
        let sequencer = address!("0x9999999999999999999999999999999999999999");
        let mut app = WalletApp::new(WalletConfig {
            sequencer_address: sequencer,
            ..WalletConfig::default()
        });
        let sender = address!("0x1111111111111111111111111111111111111111");
        let bystander = address!("0x3333333333333333333333333333333333333333");
        let fee: u16 = 1000;
        app.balances.insert(sender, U256::from(7_u64));
        app.balances.insert(bystander, U256::from(11_u64));
        app.balances.insert(sequencer, U256::from(13_u64));
        let supply = total_supply(&app);

        // The sequencer's own underfunded op pays itself: no net change. The
        // transfers exceed every balance, so only the fees move.
        for (from, nonce) in [(sender, 0), (sender, 1), (sequencer, 0)] {
            let data = transfer_data(1_000_000, bystander);
            executed(&mut app, &user_tx(from, nonce, fee, data));
            assert_eq!(total_supply(&app), supply);
        }
        assert_eq!(app.current_user_balance(sender), U256::ZERO);
        assert_eq!(app.current_user_balance(bystander), U256::from(11_u64));
        assert_eq!(app.current_user_balance(sequencer), U256::from(20_u64));
        assert_eq!(app.executed_input_count().get(), 3);
    }

    #[test]
    fn zero_balance_op_charges_nothing_and_counts() {
        let sequencer = address!("0x9999999999999999999999999999999999999999");
        let mut app = WalletApp::new(WalletConfig {
            sequencer_address: sequencer,
            ..WalletConfig::default()
        });
        let sender = address!("0x1111111111111111111111111111111111111111");

        let outputs = executed(&mut app, &user_tx(sender, 0, 1000, Vec::new()));

        assert!(outputs.is_empty());
        assert_eq!(app.current_user_balance(sender), U256::ZERO);
        assert_eq!(app.current_user_balance(sequencer), U256::ZERO);
        assert_eq!(app.current_user_nonce(sender), 1);
        assert_eq!(app.executed_input_count().get(), 1);
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
        let outputs = executed(
            &mut app,
            &direct_tx(
                super::SEPOLIA_ERC20_PORTAL_ADDRESS,
                encode_erc20_deposit_payload(
                    super::SEPOLIA_USDC_ADDRESS,
                    nested_sender,
                    U256::from(250_u64),
                ),
            ),
        );

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
        let outputs = executed(
            &mut app,
            &direct_tx(
                address!("0x3333333333333333333333333333333333333333"),
                encode_erc20_deposit_payload(
                    super::SEPOLIA_USDC_ADDRESS,
                    nested_sender,
                    U256::from(250_u64),
                ),
            ),
        );

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
        let outputs = executed(
            &mut app,
            &direct_tx(
                super::SEPOLIA_ERC20_PORTAL_ADDRESS,
                encode_erc20_deposit_payload(unsupported_token, nested_sender, U256::from(250_u64)),
            ),
        );

        assert_eq!(app.current_user_balance(nested_sender), before);
        assert_eq!(app.executed_input_count().get(), 1);
        assert!(outputs.is_empty());
    }

    #[test]
    fn malformed_trusted_portal_deposit_is_a_no_op() {
        let mut app = WalletApp::new(WalletConfig::default());

        let outputs = executed(
            &mut app,
            &direct_tx(super::SEPOLIA_ERC20_PORTAL_ADDRESS, vec![0xaa; 10]),
        );

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
        let outputs = executed(
            &mut app,
            &user_tx(sender, 0, fee_exponent, transfer_data(123, recipient)),
        );

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
        let data = ssz::Encode::as_ssz_bytes(&super::Method::Withdrawal(Withdrawal {
            amount: U256::from(123_u64),
        }));

        let outputs = executed(&mut app, &user_tx(sender, 0, fee_exponent, data));

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
    fn business_failures_after_fee_admission_charge_fee_and_consume_nonce() {
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
        let too_much = 1_000_u64;
        // Selector 2 was the retired `Deposit { amount, to }` method, laid out
        // like `Transfer`; an op shaped like it must not mint to `to`.
        let mut retired_deposit = vec![2];
        retired_deposit.extend(ssz::Encode::as_ssz_bytes(&Transfer {
            amount: U256::from(too_much),
            to: recipient,
        }));
        let mut replay = app.clone();

        for (i, data) in [
            transfer_data(too_much, recipient),
            ssz::Encode::as_ssz_bytes(&super::Method::Withdrawal(Withdrawal {
                amount: U256::from(too_much),
            })),
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
            // The lane's sequence: the pre-check admits it, then it executes.
            assert_eq!(
                check_live_user_op(&app, sender, &user_op, fee).unwrap(),
                None
            );
            let tx = L2Tx::UserOp(UserOpTx::new(sender, &user_op, fee, safe_block));
            let live = execute(&mut app, &tx).unwrap();
            assert!(
                matches!(&live, Executed::Done { outputs, .. } if outputs.is_empty()),
                "a fee-funded op is included even when its method fails"
            );
            // Restart catch-up replays the persisted op without admission.
            let replayed = execute(&mut replay, &tx).unwrap();
            assert_eq!(replayed, live);

            let charged = fee_cost * U256::from(nonce + 1);
            assert_eq!(app.current_user_nonce(sender), nonce + 1);
            assert_eq!(app.current_user_balance(sender), funded - charged);
            assert_eq!(app.current_user_balance(sequencer), charged);
            assert_eq!(app.current_user_balance(recipient), U256::ZERO);
            assert_eq!(app.executed_input_count().get(), u64::from(nonce) + 1);
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
        executed(&mut app, &user_tx(sender, 0, fee_exponent, Vec::new()));

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
        app.executed_input_count = ExecutedInputCount::new(42);

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
        assert_eq!(restored.executed_input_count, app.executed_input_count);
    }

    #[test]
    fn from_canonical_state_round_trips_and_rejects_count_mismatch() {
        let mut app = WalletApp::default();
        let sender = address!("0x1111111111111111111111111111111111111111");
        app.balances.insert(sender, U256::from(1234_u64));
        app.nonces.insert(sender, 4);
        app.executed_input_count = ExecutedInputCount::new(42);
        let bytes = app.canonical_snapshot_bytes().unwrap();

        let prefix = temp_dump_prefix();
        app.create_dump(&prefix).expect("create dump");
        let state_file = WalletApp::state_file_in_dump(&prefix);

        let restored = WalletApp::from_canonical_state(&state_file, ExecutedInputCount::new(42))
            .expect("matching count");
        let mismatch = WalletApp::from_canonical_state(&state_file, ExecutedInputCount::new(41));
        std::fs::remove_dir_all(&prefix).expect("cleanup dump");

        assert_eq!(restored.canonical_snapshot_bytes().unwrap(), bytes);
        assert_eq!(restored.executed_input_count().get(), 42);
        match mismatch {
            Err(AppError::Internal { reason }) => assert_eq!(
                reason,
                "canonical state holds L2 tx count 42, but the machine reports 41"
            ),
            other => panic!("expected a count mismatch, got {other:?}"),
        }
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
        executed(&mut first, &user_tx(sender, 0, 0, Vec::new()));
        assert_ne!(first.canonical_snapshot_bytes().unwrap(), before);
        assert_eq!(live.canonical_snapshot_bytes().unwrap(), before);
        executed(&mut live, &user_tx(sender, 0, 0, Vec::new()));
        assert_ne!(live.canonical_snapshot_bytes().unwrap(), before);
        assert_eq!(second.canonical_snapshot_bytes().unwrap(), before);
        assert_eq!(
            std::fs::read(WalletApp::state_file_in_dump(&prefix)).unwrap(),
            before
        );
        std::fs::remove_dir_all(&prefix).unwrap();
        executed(&mut first, &user_tx(sender, 1, 0, Vec::new()));
        assert_eq!(first.executed_input_count().get(), 2);
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
