// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! I/O shell that runs [`CanonicalRunner`] inside the RISC-V guest: reads
//! `trolley` rollup inputs, narrows their `U256` metadata to the scheduler's
//! `u64` domain, and emits the application's notices and vouchers, a report
//! for each rejected batch, and the answer to each inspect query.

use alloy_primitives::U256;
use rollups_types::{Notice, Voucher};
use sequencer_core::application::{AppOutput, Application, CanonicalState};
use sequencer_core::scheduler::{CanonicalRunner, InputOutcome, InspectError, L1Input};
use trolley::{InputMetadata, Rollup, RollupRequest};

pub use sequencer_core::scheduler::{
    SCHEDULER_INSPECT_QUERY, STATE_INSPECT_QUERY, SchedulerConfig,
};

/// The report a rejected batch leaves. Reports are diagnostics, not consensus,
/// so every reject reason shares this one text.
pub const INVALID_BATCH_REPORT: &[u8] = b"scheduler dropped invalid batch";

/// The report for an inspect query the guest does not answer.
pub const UNSUPPORTED_QUERY_REPORT: &[u8] = b"unsupported inspect query";

/// Runs the canonical machine from genesis. Every input is accepted at the
/// machine level; the guest halts (panics) only on the fatal conditions: an
/// application error, a counter with no `u64` successor, or metadata that does
/// not fit `u64`.
pub fn run_scheduler_forever<R: Rollup, A: Application + CanonicalState>(
    mut rollup: R,
    app: A,
    scheduler_config: SchedulerConfig,
) -> ! {
    let mut runner = CanonicalRunner::genesis(app, scheduler_config);

    loop {
        match rollup.next_input() {
            Ok(RollupRequest::Advance { metadata, payload }) => {
                // An application error leaves the input half done, with no
                // canonical successor: halt before any of its outputs leave.
                let advanced = runner
                    .process_input(l1_input(&metadata, payload))
                    .unwrap_or_else(|err| panic!("canonical application execution failed: {err}"));
                for output in &advanced.outputs {
                    emit_app_output(&mut rollup, output)
                        .unwrap_or_else(|err| panic!("scheduler failed to emit app output: {err}"));
                }
                if matches!(advanced.outcome, InputOutcome::BatchRejected(_)) {
                    rollup
                        .emit_report(INVALID_BATCH_REPORT)
                        .unwrap_or_else(|err| {
                            panic!("scheduler failed to emit invalid-batch report: {err}")
                        });
                }
            }
            Ok(RollupRequest::Inspect { payload }) => {
                // Inspect is a public, read-only endpoint: an unknown query or
                // a state-encode error gets a report and must not halt.
                let report = match runner.inspect(&payload) {
                    Ok(bytes) => bytes,
                    Err(InspectError::UnsupportedQuery) => UNSUPPORTED_QUERY_REPORT.to_vec(),
                    Err(InspectError::Application(reason)) => {
                        format!("inspect failed: {reason}").into_bytes()
                    }
                };
                rollup
                    .emit_report(&report)
                    .unwrap_or_else(|err| panic!("scheduler failed to emit inspect report: {err}"));
            }
            Err(err) => panic!("scheduler failed while reading next input: {err}"),
        }
    }
}

/// The scheduler's view of an `EvmAdvance`. The index and timestamp reach the
/// application through each direct; `prev_randao` is not part of the protocol.
fn l1_input(metadata: &InputMetadata, payload: Vec<u8>) -> L1Input {
    L1Input {
        index: narrow(metadata.index, "input index"),
        sender: metadata.msg_sender,
        block: narrow(metadata.block_number, "block number"),
        timestamp: narrow(metadata.block_timestamp, "block timestamp"),
        chain_id: narrow(metadata.chain_id, "chain id"),
        app_contract: metadata.app_contract,
        payload,
    }
}

/// Solidity hands these fields over as `uint256`. A value beyond `u64` has no
/// canonical successor state, so the guest halts on it, as a port must.
fn narrow(value: U256, field: &str) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{field} does not fit u64"))
}

fn emit_app_output<R: Rollup>(rollup: &mut R, output: &AppOutput) -> trolley::RollupResult<()> {
    match output {
        AppOutput::Notice(payload) => {
            let notice = Notice {
                payload: payload.clone().into(),
            };
            rollup.emit_notice(&notice)
        }
        AppOutput::Voucher {
            destination,
            value,
            payload,
        } => {
            let voucher = Voucher {
                destination: *destination,
                value: *value,
                payload: payload.clone().into(),
            };
            rollup.emit_voucher(&voucher)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{Address, address};
    use app_core::application::{
        DEVNET_SEQUENCER_ADDRESS, DepositNotice, Method, TransferNotice, WALLET_SCHEDULER_LIMITS,
        WalletApp, WalletConfig, Withdrawal,
    };
    use k256::ecdsa::SigningKey;
    use rollups_types::alloy_sol_types::SolCall;
    use rollups_types::{Erc20Deposit, Erc20Transfer};
    use sequencer_core::application::{Admission, AppError, AppOutputs, SchedulerLimits};
    use sequencer_core::batch::{Batch, Frame, WireUserOp};
    use sequencer_core::fee::fee_to_linear;
    use sequencer_core::history::L2TxCount;
    use sequencer_core::l2_tx::{Direct, UserOpTx};
    use sequencer_core::scheduler::SchedulerExport;
    use sequencer_core::user_op::UserOp;
    use trolley::RollupError;

    use super::*;

    const CHAIN_ID: u64 = 31337;
    const APP_CONTRACT: Address = address!("0xa5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5");
    const BOB: Address = address!("0x8888888888888888888888888888888888888888");
    /// The panic the loop ends with once the mock runs out of requests.
    const END_OF_INPUTS: &str = "scheduler failed while reading next input";

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Emitted {
        Notice(Vec<u8>),
        Voucher {
            destination: Address,
            value: U256,
            payload: Vec<u8>,
        },
        Report(Vec<u8>),
    }

    /// Serves scripted requests, then fails, which the loop treats as fatal.
    struct MockRollup {
        requests: VecDeque<RollupRequest>,
        emitted: Arc<Mutex<Vec<Emitted>>>,
    }

    impl MockRollup {
        fn push(&self, emitted: Emitted) -> trolley::RollupResult<()> {
            self.emitted
                .lock()
                .expect("poisoned emitted mutex")
                .push(emitted);
            Ok(())
        }
    }

    impl Rollup for MockRollup {
        fn next_input(&mut self) -> trolley::RollupResult<RollupRequest> {
            self.requests.pop_front().ok_or(RollupError::CmtCallFailed {
                operation: "next_input",
                code: -22,
            })
        }

        fn revert(&mut self) -> ! {
            panic!("the canonical guest never reverts");
        }

        fn gio(&mut self, _domain: u16, _id: &[u8]) -> trolley::RollupResult<(u16, Vec<u8>)> {
            panic!("the canonical guest never issues a GIO request");
        }

        fn emit_voucher(&mut self, voucher: &Voucher) -> trolley::RollupResult<()> {
            self.push(Emitted::Voucher {
                destination: voucher.destination,
                value: voucher.value,
                payload: voucher.payload.to_vec(),
            })
        }

        fn emit_notice(&mut self, notice: &Notice) -> trolley::RollupResult<()> {
            self.push(Emitted::Notice(notice.payload.to_vec()))
        }

        fn emit_report(&mut self, report: &[u8]) -> trolley::RollupResult<()> {
            self.push(Emitted::Report(report.to_vec()))
        }
    }

    /// Runs the loop over `requests` and returns what it emitted and the
    /// message it halted with.
    fn run<A: Application + CanonicalState>(
        app: A,
        config: SchedulerConfig,
        requests: Vec<RollupRequest>,
    ) -> (Vec<Emitted>, String) {
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let rollup = MockRollup {
            requests: VecDeque::from(requests),
            emitted: Arc::clone(&emitted),
        };
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_scheduler_forever(rollup, app, config)
        }))
        .expect_err("the loop only ever ends by halting");
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        let emitted = emitted.lock().expect("poisoned emitted mutex").clone();
        (emitted, message)
    }

    fn wallet_config() -> SchedulerConfig {
        SchedulerConfig::for_application::<WalletApp>(DEVNET_SEQUENCER_ADDRESS)
    }

    /// Runs the devnet wallet through every request; it must not halt early.
    fn run_wallet(script: Script) -> Vec<Emitted> {
        let (emitted, halt) = run(
            WalletApp::new(WalletConfig::devnet()),
            wallet_config(),
            script.requests,
        );
        assert!(halt.starts_with(END_OF_INPUTS), "halted early: {halt}");
        emitted
    }

    /// Requests in InputBox order: each advance gets the next index.
    #[derive(Default)]
    struct Script {
        requests: Vec<RollupRequest>,
        next_index: u64,
    }

    impl Script {
        fn advance(self, sender: Address, block: u64, payload: Vec<u8>) -> Self {
            self.advance_at(sender, block, 0, payload)
        }

        fn advance_at(
            mut self,
            sender: Address,
            block: u64,
            timestamp: u64,
            payload: Vec<u8>,
        ) -> Self {
            let metadata = metadata(self.next_index, sender, block, timestamp);
            self.next_index += 1;
            self.requests
                .push(RollupRequest::Advance { metadata, payload });
            self
        }

        fn deposit(self, block: u64, owner: Address, amount: u64) -> Self {
            self.deposit_with(block, owner, amount, Vec::new())
        }

        fn deposit_with(
            self,
            block: u64,
            owner: Address,
            amount: u64,
            exec_layer_data: Vec<u8>,
        ) -> Self {
            let wallet = WalletConfig::devnet();
            let payload = Erc20Deposit {
                token: wallet.supported_erc20_token,
                sender: owner,
                value: U256::from(amount),
                exec_layer_data,
            }
            .encode();
            self.advance(wallet.erc20_portal_address, block, payload)
        }

        fn batch(self, block: u64, batch: Batch) -> Self {
            self.advance(
                DEVNET_SEQUENCER_ADDRESS,
                block,
                ssz::Encode::as_ssz_bytes(&batch),
            )
        }

        fn inspect(mut self, query: &[u8]) -> Self {
            self.requests.push(RollupRequest::Inspect {
                payload: query.to_vec(),
            });
            self
        }
    }

    fn metadata(index: u64, sender: Address, block: u64, timestamp: u64) -> InputMetadata {
        InputMetadata {
            chain_id: U256::from(CHAIN_ID),
            app_contract: APP_CONTRACT,
            msg_sender: sender,
            block_number: U256::from(block),
            block_timestamp: U256::from(timestamp),
            prev_randao: U256::ZERO,
            index: U256::from(index),
        }
    }

    fn batch(nonce: u64, frames: Vec<Frame>) -> Batch {
        Batch { nonce, frames }
    }

    fn frame(safe_block: u64, fee_price: u16, user_ops: Vec<WireUserOp>) -> Frame {
        Frame {
            user_ops,
            safe_block,
            fee_price,
        }
    }

    fn key(byte: u8) -> SigningKey {
        SigningKey::from_slice(&[byte; 32]).expect("signing key")
    }

    fn address_of(key: &SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_raw_public_key(&point.as_bytes()[1..])
    }

    fn signed(key: &SigningKey, nonce: u32, max_fee: u16, method: &Method) -> WireUserOp {
        let data = ssz::Encode::as_ssz_bytes(method);
        let user_op = UserOp {
            nonce,
            max_fee,
            data: data.clone().into(),
        };
        let domain = sequencer_core::build_input_domain(CHAIN_ID, APP_CONTRACT);
        let signature = sequencer_core::test_support::sign_user_op(key, &domain, &user_op);
        WireUserOp {
            nonce,
            max_fee,
            data,
            signature: signature.as_bytes().to_vec(),
        }
    }

    fn transfer(amount: u64) -> Method {
        Method::Transfer(app_core::application::Transfer {
            amount: U256::from(amount),
            to: BOB,
        })
    }

    fn deposit_notice(owner: Address, amount: u64) -> Emitted {
        Emitted::Notice(
            DepositNotice {
                token: WalletConfig::devnet().supported_erc20_token,
                sender: owner,
                amount: U256::from(amount),
            }
            .abi_encode(),
        )
    }

    fn transfer_notice(sender: Address, amount: u64) -> Emitted {
        Emitted::Notice(
            TransferNotice {
                sender,
                recipient: BOB,
                amount: U256::from(amount),
            }
            .abi_encode(),
        )
    }

    fn invalid_batch() -> Emitted {
        Emitted::Report(INVALID_BATCH_REPORT.to_vec())
    }

    fn scheduler_report(export: [u64; 6]) -> Emitted {
        let [
            l1_input_count,
            l2_tx_count,
            next_batch_nonce,
            clock,
            first_queued_input,
            fridge_len,
        ] = export;
        Emitted::Report(
            SchedulerExport {
                l1_input_count,
                l2_tx_count,
                next_batch_nonce,
                clock,
                first_queued_input,
                fridge_len,
            }
            .encode()
            .to_vec(),
        )
    }

    /// The wallet the guest's `state` report holds, decoded.
    fn reported_wallet(report: &Emitted) -> WalletApp {
        let Emitted::Report(bytes) = report else {
            panic!("expected a state report, got {report:?}");
        };
        app_core::wallet_snapshot::decode(bytes).expect("decode the state report")
    }

    #[test]
    fn the_state_query_reports_the_application_snapshot() {
        let emitted = run_wallet(Script::default().inspect(STATE_INSPECT_QUERY).inspect(b""));
        let genesis = app_core::wallet_snapshot::encode(&WalletApp::new(WalletConfig::devnet()));
        assert_eq!(
            emitted,
            [Emitted::Report(genesis.clone()), Emitted::Report(genesis)]
        );
    }

    #[test]
    fn the_scheduler_query_reports_zeros_at_genesis() {
        let emitted = run_wallet(Script::default().inspect(SCHEDULER_INSPECT_QUERY));
        assert_eq!(
            emitted,
            [Emitted::Report(vec![0; SchedulerExport::ENCODED_LEN])]
        );
    }

    /// An unknown query gets a report and the guest keeps serving.
    #[test]
    fn an_unsupported_query_is_reported_without_halting() {
        let emitted = run_wallet(
            Script::default()
                .inspect(b"balances")
                .inspect(b"scheduler ")
                .inspect(SCHEDULER_INSPECT_QUERY),
        );
        let unsupported = Emitted::Report(UNSUPPORTED_QUERY_REPORT.to_vec());
        assert_eq!(
            emitted,
            [unsupported.clone(), unsupported, scheduler_report([0; 6])]
        );
    }

    #[test]
    fn a_failing_state_encode_is_reported_without_halting() {
        let probe = ProbeApp {
            snapshot_fails: true,
            ..ProbeApp::default()
        };
        let script = Script::default()
            .inspect(STATE_INSPECT_QUERY)
            .inspect(SCHEDULER_INSPECT_QUERY);
        let (emitted, halt) = run(probe, probe_config(), script.requests);
        assert!(halt.starts_with(END_OF_INPUTS), "halted early: {halt}");
        let reason = AppError::Internal {
            reason: "snapshot refused".to_string(),
        };
        assert_eq!(
            emitted,
            [
                Emitted::Report(format!("inspect failed: {reason}").into_bytes()),
                scheduler_report([0; 6]),
            ]
        );
    }

    #[test]
    fn a_queued_direct_shows_in_the_scheduler_report() {
        let alice = address_of(&key(1));
        let emitted = run_wallet(
            Script::default()
                .deposit(10, alice, 250)
                .inspect(SCHEDULER_INSPECT_QUERY),
        );
        // Queued, not executed: no notice, and the L2 tx count stays at zero.
        assert_eq!(emitted, [scheduler_report([1, 0, 0, 0, 0, 1])]);
    }

    /// A direct one byte over the cap never joins the fridge, so a drain
    /// passes it by; one exactly at the cap executes.
    #[test]
    fn a_direct_over_the_payload_cap_never_executes() {
        let alice = address_of(&key(1));
        let cap = usize::try_from(WALLET_SCHEDULER_LIMITS.max_direct_payload_bytes).unwrap();
        let at_cap = vec![0; cap - rollups_types::ERC20_DEPOSIT_PREFIX_BYTES];
        let over_cap = vec![0; at_cap.len() + 1];
        let emitted = run_wallet(
            Script::default()
                .deposit_with(10, alice, 7, over_cap)
                .inspect(SCHEDULER_INSPECT_QUERY)
                .deposit_with(10, alice, 5, at_cap)
                .batch(11, batch(0, vec![frame(10, 0, vec![])]))
                .inspect(SCHEDULER_INSPECT_QUERY)
                .inspect(STATE_INSPECT_QUERY),
        );
        assert_eq!(emitted.len(), 4, "{emitted:?}");
        assert_eq!(emitted[0], scheduler_report([1, 0, 0, 0, 1, 0]));
        assert_eq!(emitted[1], deposit_notice(alice, 5));
        assert_eq!(emitted[2], scheduler_report([3, 1, 1, 10, 3, 0]));
        assert_eq!(
            reported_wallet(&emitted[3]).current_user_balance(alice),
            U256::from(5)
        );
    }

    /// A frame drains the deposits of its own safe block, strictly below the
    /// batch's inclusion block, then runs its ops: outputs in that order.
    #[test]
    fn a_strict_drain_batch_executes_the_deposit_then_the_ops() {
        let alice_key = key(2);
        let alice = address_of(&alice_key);
        let withdrawal = Method::Withdrawal(Withdrawal {
            amount: U256::from(100),
        });
        let emitted = run_wallet(
            Script::default()
                .deposit(10, alice, 250)
                .batch(
                    11,
                    batch(
                        0,
                        vec![frame(10, 0, vec![signed(&alice_key, 0, 0, &withdrawal)])],
                    ),
                )
                .inspect(SCHEDULER_INSPECT_QUERY),
        );
        let voucher = Emitted::Voucher {
            destination: WalletConfig::devnet().supported_erc20_token,
            value: U256::ZERO,
            payload: Erc20Transfer {
                recipient: alice,
                amount: U256::from(100),
            }
            .abi_encode(),
        };
        assert_eq!(
            emitted,
            [
                deposit_notice(alice, 250),
                voucher,
                scheduler_report([2, 2, 1, 10, 2, 0]),
            ]
        );
    }

    #[test]
    fn an_undecodable_batch_is_reported() {
        let emitted = run_wallet(
            Script::default()
                .advance(DEVNET_SEQUENCER_ADDRESS, 10, vec![0xFF, 0xEE, 0xDD])
                .inspect(SCHEDULER_INSPECT_QUERY),
        );
        assert_eq!(
            emitted,
            [invalid_batch(), scheduler_report([1, 0, 0, 0, 1, 0])]
        );
    }

    /// The backstop drains an overdue deposit before the batch's gates run,
    /// so its notice leaves before the reject's report; the reject consumes
    /// no batch nonce.
    #[test]
    fn a_rejected_batch_reports_after_its_backstop_outputs() {
        let alice = address_of(&key(3));
        let overdue = 1 + WALLET_SCHEDULER_LIMITS.max_wait_blocks;
        let emitted = run_wallet(
            Script::default()
                .deposit(1, alice, 250)
                .batch(overdue, batch(0, vec![frame(overdue, 0, vec![])]))
                .batch(overdue, batch(0, vec![frame(overdue - 1, 0, vec![])]))
                .inspect(SCHEDULER_INSPECT_QUERY),
        );
        assert_eq!(
            emitted,
            [
                deposit_notice(alice, 250),
                invalid_batch(),
                scheduler_report([3, 1, 1, overdue - 1, 3, 0]),
            ]
        );
    }

    /// Gate f: once a frame raised the clock, a batch whose first frame lies
    /// below it is rejected, and a retry at the clock is accepted.
    #[test]
    fn a_first_frame_below_the_clock_is_rejected() {
        let alice = address_of(&key(4));
        let emitted = run_wallet(
            Script::default()
                .deposit(10, alice, 100)
                .batch(12, batch(0, vec![frame(11, 0, vec![])]))
                .deposit(12, alice, 50)
                .batch(13, batch(1, vec![frame(10, 0, vec![])]))
                .batch(13, batch(1, vec![frame(12, 0, vec![])])),
        );
        assert_eq!(
            emitted,
            [
                deposit_notice(alice, 100),
                invalid_batch(),
                deposit_notice(alice, 50),
            ]
        );
    }

    /// An op its sender cannot pay in full still executes: it pays what the
    /// sender has, consumes the nonce, and its method runs on what is left.
    /// A later op at the same nonce is skipped.
    #[test]
    fn an_underfunded_op_executes_and_consumes_its_nonce() {
        let alice_key = key(5);
        let alice = address_of(&alice_key);
        let fee_price = 200;
        let gas = u64::try_from(fee_to_linear(fee_price)).unwrap();
        let base_gas = u64::try_from(fee_to_linear(0)).unwrap();
        assert!(gas > 1, "the deposit must be nonzero yet below the fee");
        let emitted = run_wallet(
            Script::default()
                .deposit(10, alice, gas - 1)
                .batch(
                    11,
                    batch(
                        0,
                        vec![frame(
                            10,
                            fee_price,
                            vec![signed(&alice_key, 0, fee_price, &transfer(1))],
                        )],
                    ),
                )
                .deposit(12, alice, 50)
                .batch(
                    13,
                    batch(
                        1,
                        vec![frame(
                            12,
                            0,
                            vec![
                                signed(&alice_key, 0, 0, &transfer(1)),
                                signed(&alice_key, 1, 0, &transfer(1)),
                            ],
                        )],
                    ),
                )
                .inspect(SCHEDULER_INSPECT_QUERY)
                .inspect(STATE_INSPECT_QUERY),
        );
        assert_eq!(emitted.len(), 5, "{emitted:?}");
        assert_eq!(
            emitted[..3],
            [
                deposit_notice(alice, gas - 1),
                deposit_notice(alice, 50),
                transfer_notice(alice, 1),
            ]
        );
        // Four executed: two deposits, the underfunded op and the nonce-1 op.
        assert_eq!(emitted[3], scheduler_report([4, 4, 2, 12, 4, 0]));
        let wallet = reported_wallet(&emitted[4]);
        assert_eq!(wallet.current_user_nonce(alice), 2);
        assert_eq!(
            wallet.current_user_balance(alice),
            U256::from(50 - base_gas - 1)
        );
        assert_eq!(wallet.current_user_balance(BOB), U256::from(1));
        assert_eq!(
            wallet.current_user_balance(DEVNET_SEQUENCER_ADDRESS),
            U256::from(gas - 1 + base_gas)
        );
    }

    /// Each direct reaches the application with its own InputBox index and
    /// block timestamp, not the draining batch's.
    #[test]
    fn index_and_timestamp_reach_the_application() {
        let script = Script::default()
            .advance_at(BOB, 4, 1_000, b"a".to_vec())
            .inspect(SCHEDULER_INSPECT_QUERY)
            .advance_at(
                DEVNET_SEQUENCER_ADDRESS,
                5,
                1_500,
                ssz::Encode::as_ssz_bytes(&batch(0, vec![])),
            )
            .advance_at(BOB, 5, 2_000, b"b".to_vec())
            .advance_at(
                DEVNET_SEQUENCER_ADDRESS,
                7,
                3_000,
                ssz::Encode::as_ssz_bytes(&batch(1, vec![frame(5, 0, vec![])])),
            )
            .inspect(STATE_INSPECT_QUERY);
        let (emitted, halt) = run(ProbeApp::default(), probe_config(), script.requests);
        assert!(halt.starts_with(END_OF_INPUTS), "halted early: {halt}");
        assert_eq!(
            emitted,
            [
                scheduler_report([1, 0, 0, 0, 0, 1]),
                Emitted::Notice(b"a".to_vec()),
                Emitted::Notice(b"b".to_vec()),
                Emitted::Report(b"0@1000;2@2000;".to_vec()),
            ]
        );
    }

    /// An application error halts the guest before any output of that input
    /// leaves, including those of the transactions that ran before it.
    #[test]
    fn an_application_error_halts_before_the_input_emits() {
        let script = Script::default()
            .advance(BOB, 1, b"ok".to_vec())
            .advance(BOB, 1, b"boom".to_vec())
            .advance(
                DEVNET_SEQUENCER_ADDRESS,
                2,
                ssz::Encode::as_ssz_bytes(&batch(0, vec![frame(1, 0, vec![])])),
            );
        let (emitted, halt) = run(ProbeApp::default(), probe_config(), script.requests);
        assert_eq!(
            halt,
            "canonical application execution failed: internal: refusing boom"
        );
        assert!(emitted.is_empty(), "{emitted:?}");
    }

    /// No canonical successor exists for metadata beyond `u64`: the guest
    /// halts before the scheduler sees the input.
    #[test]
    fn metadata_beyond_u64_halts() {
        let too_big = U256::from(u64::MAX) + U256::from(1);
        /// A field's name, and how to set it.
        type Case = (&'static str, fn(&mut InputMetadata, U256));
        let cases: [Case; 4] = [
            ("input index", |m, v| m.index = v),
            ("block number", |m, v| m.block_number = v),
            ("block timestamp", |m, v| m.block_timestamp = v),
            ("chain id", |m, v| m.chain_id = v),
        ];
        for (field, set) in cases {
            let mut metadata = metadata(0, BOB, 1, 0);
            set(&mut metadata, too_big);
            let requests = vec![RollupRequest::Advance {
                metadata,
                payload: b"x".to_vec(),
            }];
            let (emitted, halt) = run(ProbeApp::default(), probe_config(), requests);
            assert_eq!(halt, format!("{field} does not fit u64"));
            assert!(emitted.is_empty(), "{field}: {emitted:?}");
        }
    }

    fn probe_config() -> SchedulerConfig {
        SchedulerConfig::for_application::<ProbeApp>(DEVNET_SEQUENCER_ADDRESS)
    }

    /// Records each direct's index and timestamp, which the wallet ignores,
    /// and fails on the payload `boom`.
    #[derive(Default)]
    struct ProbeApp {
        seen: Vec<(u64, u64)>,
        count: L2TxCount,
        snapshot_fails: bool,
    }

    impl Application for ProbeApp {
        fn user_nonce(&self, _sender: Address) -> Result<u32, AppError> {
            Ok(0)
        }

        fn apply_user_op(&mut self, _op: &UserOpTx) -> Result<AppOutputs, AppError> {
            unreachable!("the probe's scripts carry no user ops")
        }

        fn apply_direct(&mut self, direct: &Direct) -> Result<AppOutputs, AppError> {
            if direct.payload == b"boom" {
                return Err(AppError::Internal {
                    reason: "refusing boom".to_string(),
                });
            }
            self.seen.push((direct.input_index, direct.timestamp));
            self.count.advance();
            Ok(vec![AppOutput::Notice(direct.payload.clone())])
        }

        fn l2_tx_count(&self) -> L2TxCount {
            self.count
        }

        fn max_method_payload_bytes() -> usize {
            0
        }

        fn scheduler_limits() -> SchedulerLimits {
            WALLET_SCHEDULER_LIMITS
        }

        fn admit_user_op(
            &self,
            _sender: Address,
            _op: &UserOp,
            _fee: u16,
        ) -> Result<Admission, AppError> {
            Ok(Admission::Admit)
        }

        fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
            unreachable!("the guest never loads a dump")
        }

        fn create_dump(&mut self, _prefix: &Path) -> Result<(), AppError> {
            unreachable!("the guest never writes a dump")
        }

        fn state_file_in_dump(prefix: &Path) -> PathBuf {
            prefix.to_path_buf()
        }

        fn from_canonical_state(
            _state_file: &Path,
            _l2_tx_count: L2TxCount,
        ) -> Result<Self, AppError> {
            unreachable!("the guest starts from genesis")
        }
    }

    impl CanonicalState for ProbeApp {
        fn canonical_snapshot_bytes(&self) -> Result<Vec<u8>, AppError> {
            if self.snapshot_fails {
                return Err(AppError::Internal {
                    reason: "snapshot refused".to_string(),
                });
            }
            Ok(self
                .seen
                .iter()
                .map(|(index, timestamp)| format!("{index}@{timestamp};"))
                .collect::<String>()
                .into_bytes())
        }
    }
}
