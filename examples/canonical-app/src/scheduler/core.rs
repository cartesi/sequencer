// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, Signature, U256, address};
use alloy_sol_types::Eip712Domain;
use alloy_sol_types::SolStruct;
use sequencer_core::application::{AppOutputs, Application};
use sequencer_core::batch::{Batch, Frame, WireUserOp};
use sequencer_core::l2_tx::DirectInput;
use std::collections::VecDeque;

pub const DEVNET_SEQUENCER_ADDRESS: Address =
    address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
pub const SEPOLIA_SEQUENCER_ADDRESS: Address =
    address!("0x16d5FF3Fdd14e2a86FBA77cbcE6B3Cd9C32b8Ff3");
pub const MAX_WAIT_BLOCKS: u64 = 1200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub sequencer_address: Address,
    pub max_wait_blocks: u64,
}

impl SchedulerConfig {
    pub const fn devnet() -> Self {
        Self {
            sequencer_address: DEVNET_SEQUENCER_ADDRESS,
            max_wait_blocks: MAX_WAIT_BLOCKS,
        }
    }

    pub const fn sepolia() -> Self {
        Self {
            sequencer_address: SEPOLIA_SEQUENCER_ADDRESS,
            max_wait_blocks: MAX_WAIT_BLOCKS,
        }
    }
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self::sepolia()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SchedulerInput {
    pub sender: Address,
    pub inclusion_block: u64,
    pub domain: Eip712Domain,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessOutcome {
    DirectEnqueued,
    BatchExecuted,
    BatchSkippedStale,
    BatchRejected(BatchRejectReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProcessResult {
    pub outcome: ProcessOutcome,
    pub outputs: AppOutputs,
}

impl ProcessResult {
    fn new(outcome: ProcessOutcome, outputs: AppOutputs) -> Self {
        Self { outcome, outputs }
    }

    fn without_outputs(outcome: ProcessOutcome) -> Self {
        Self::new(outcome, Vec::new())
    }
}

impl PartialEq<ProcessOutcome> for ProcessResult {
    fn eq(&self, other: &ProcessOutcome) -> bool {
        self.outcome == *other
    }
}

impl PartialEq<ProcessResult> for ProcessOutcome {
    fn eq(&self, other: &ProcessResult) -> bool {
        *self == other.outcome
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BatchRejectReason {
    DecodeFailed,
    WrongNonce { expected: u64, got: u64 },
    SafeBlockAboveInclusionBlock,
    NonMonotonicSafeBlocks,
}

#[derive(Debug)]
pub struct Scheduler<A: Application> {
    app: A,
    config: SchedulerConfig,
    direct_q: VecDeque<QueuedDirectInput>,
    next_expected_batch_nonce: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedDirectInput {
    sender: Address,
    payload: Vec<u8>,
    inclusion_block: u64,
}

impl<A: Application> Scheduler<A> {
    pub fn new(app: A, config: SchedulerConfig) -> Self {
        Self {
            app,
            config,
            direct_q: VecDeque::new(),
            next_expected_batch_nonce: 0,
        }
    }

    #[cfg(test)]
    pub fn queued_direct_len(&self) -> usize {
        self.direct_q.len()
    }

    #[cfg(test)]
    pub fn next_expected_batch_nonce(&self) -> u64 {
        self.next_expected_batch_nonce
    }

    pub(super) fn process_input(&mut self, input: SchedulerInput) -> ProcessResult {
        // Execute overdue directs before any input to keep backstop semantics explicit.
        let mut outputs = Vec::new();
        self.force_execute_overdue(input.inclusion_block, &mut outputs);

        if input.sender != self.config.sequencer_address {
            self.direct_q.push_back(QueuedDirectInput {
                sender: input.sender,
                payload: input.payload,
                inclusion_block: input.inclusion_block,
            });
            ProcessResult::new(ProcessOutcome::DirectEnqueued, outputs)
        } else {
            let batch_result =
                self.process_batch_payload(input.inclusion_block, &input.domain, &input.payload);
            outputs.extend(batch_result.outputs);
            ProcessResult::new(batch_result.outcome, outputs)
        }
    }

    fn process_batch_payload(
        &mut self,
        inclusion_block: u64,
        domain: &Eip712Domain,
        payload: &[u8],
    ) -> ProcessResult {
        let Ok(batch): Result<Batch, _> = ssz::Decode::from_ssz_bytes(payload) else {
            return ProcessResult::without_outputs(ProcessOutcome::BatchRejected(
                BatchRejectReason::DecodeFailed,
            ));
        };

        if batch.nonce != self.next_expected_batch_nonce {
            return ProcessResult::without_outputs(ProcessOutcome::BatchRejected(
                BatchRejectReason::WrongNonce {
                    expected: self.next_expected_batch_nonce,
                    got: batch.nonce,
                },
            ));
        }

        let Some((frame_head, frame_tail)) = batch.frames.split_first() else {
            self.advance_expected_batch_nonce();
            return ProcessResult::without_outputs(ProcessOutcome::BatchExecuted);
        };

        if let Some(reason) =
            self.batch_reject_reason_for_block(inclusion_block, frame_head, frame_tail)
        {
            return ProcessResult::without_outputs(ProcessOutcome::BatchRejected(reason));
        }

        if has_elapsed_since(
            frame_head.safe_block,
            self.config.max_wait_blocks,
            inclusion_block,
        ) {
            self.advance_expected_batch_nonce();
            return ProcessResult::without_outputs(ProcessOutcome::BatchSkippedStale);
        }

        let mut outputs = Vec::new();
        for frame in &batch.frames {
            self.drain_directs_safe_at(frame.safe_block, &mut outputs);
            self.execute_frame_user_ops(domain, frame, &mut outputs);
        }

        self.advance_expected_batch_nonce();
        ProcessResult::new(ProcessOutcome::BatchExecuted, outputs)
    }

    fn advance_expected_batch_nonce(&mut self) {
        self.next_expected_batch_nonce = self
            .next_expected_batch_nonce
            .checked_add(1)
            .expect("batch nonce overflow");
    }

    fn batch_reject_reason_for_block(
        &self,
        inclusion_block: u64,
        head: &Frame,
        tail: &[Frame],
    ) -> Option<BatchRejectReason> {
        if head.safe_block > inclusion_block {
            return Some(BatchRejectReason::SafeBlockAboveInclusionBlock);
        }

        let mut previous_safe_block = head.safe_block;
        for frame in tail {
            if frame.safe_block > inclusion_block {
                return Some(BatchRejectReason::SafeBlockAboveInclusionBlock);
            } else if frame.safe_block < previous_safe_block {
                return Some(BatchRejectReason::NonMonotonicSafeBlocks);
            } else {
                previous_safe_block = frame.safe_block;
            }
        }

        None
    }

    /// Execute user-ops in a frame, skipping any whose `max_fee` is below the frame's `fee_price`.
    ///
    /// `fee_price` is in "L2 smallest-token-unit per user-op-byte", derived from the sequencer's
    /// `gas_price` DB knob.  That knob is "L2 smallest-token-unit per L1 gas unit" — whoever
    /// feeds it must convert `L1_gas_price_in_wei × exchange_rate` into this integer.
    ///
    /// For native tokens with few decimals (e.g. USDC with 6 decimals), the feeder should
    /// pre-scale `gas_price` by multiplying by 10^k before writing it to the DB, so that
    /// sub-unit precision is not lost to integer truncation.  The scheduler itself does not
    /// need to know about the scaling — it simply compares `max_fee` against `fee_price`.
    fn execute_frame_user_ops(
        &mut self,
        domain: &Eip712Domain,
        frame: &Frame,
        outputs: &mut AppOutputs,
    ) {
        for user_op in &frame.user_ops {
            if let Some(sender) = self.recover_sender(domain, user_op) {
                let plain = user_op.to_user_op();
                if u64::from(plain.max_fee) < frame.fee_price {
                    eprintln!("scheduler skipped frame user-op due to max_fee < fee_price");
                    continue;
                }
                match self
                    .app
                    .validate_and_execute_user_op(sender, &plain, frame.fee_price)
                {
                    Ok(sequencer_core::application::ExecutionOutcome::Included {
                        outputs: user_op_outputs,
                    }) => outputs.extend(user_op_outputs),
                    Ok(sequencer_core::application::ExecutionOutcome::Invalid(_)) => {}
                    Err(err) => {
                        eprintln!("scheduler skipped frame user-op due to app error: {err}");
                    }
                }
            } else {
                eprintln!("scheduler skipped frame user-op due to invalid signature");
            }
        }
    }

    fn recover_sender(&self, domain: &Eip712Domain, wire_user_op: &WireUserOp) -> Option<Address> {
        if wire_user_op.signature.len() != WireUserOp::SIGNATURE_BYTES {
            return None;
        }
        let signature = Signature::from_raw(wire_user_op.signature.as_slice()).ok()?;
        let user_op = wire_user_op.to_user_op();
        let signing_hash = user_op.eip712_signing_hash(domain);
        signature.recover_address_from_prehash(&signing_hash).ok()
    }

    fn drain_directs_safe_at(&mut self, safe_block: u64, outputs: &mut AppOutputs) {
        while let Some(front) = self.direct_q.front() {
            if front.inclusion_block > safe_block {
                break;
            }
            let queued = self.direct_q.pop_front().expect("queue front must exist");
            let input = DirectInput {
                sender: queued.sender,
                block_number: queued.inclusion_block,
                payload: queued.payload,
            };
            match self.app.execute_direct_input(&input) {
                Ok(direct_outputs) => outputs.extend(direct_outputs),
                Err(err) => eprintln!("scheduler failed to execute drained direct input: {err}"),
            }
        }
    }

    fn force_execute_overdue(&mut self, current_block: u64, outputs: &mut AppOutputs) {
        while let Some(front) = self.direct_q.front() {
            if has_elapsed_since(
                front.inclusion_block,
                self.config.max_wait_blocks,
                current_block,
            ) {
                let input = DirectInput {
                    sender: front.sender,
                    block_number: front.inclusion_block,
                    payload: front.payload.clone(),
                };
                match self.app.execute_direct_input(&input) {
                    Ok(direct_outputs) => outputs.extend(direct_outputs),
                    Err(err) => {
                        eprintln!("scheduler failed to execute overdue direct input: {err}")
                    }
                }

                self.direct_q.pop_front().expect("queue front must exist");
            } else {
                break;
            }
        }
    }
}

fn has_elapsed_since(start_block: u64, wait_blocks: u64, current_block: u64) -> bool {
    current_block.saturating_sub(start_block) >= wait_blocks
}

pub(super) fn input_domain(chain_id: u64, verifying_contract: Address) -> Eip712Domain {
    Eip712Domain {
        name: None,
        version: None,
        chain_id: Some(U256::from(chain_id)),
        verifying_contract: Some(verifying_contract),
        salt: None,
    }
}

pub(super) fn block_to_u64(block: U256) -> u64 {
    // Solidity ABI exposes block numbers as uint256, but scheduler semantics use u64.
    // A value that does not fit u64 is a malformed host input for this prototype.
    u64::try_from(block).expect("block number does not fit u64")
}

pub(super) fn chain_id_to_u64(chain_id: U256) -> u64 {
    u64::try_from(chain_id).expect("chain id does not fit u64")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use k256::ecdsa::SigningKey;
    use k256::ecdsa::signature::hazmat::PrehashSigner;
    use sequencer_core::user_op::UserOp;

    #[cfg(test)]
    #[derive(Default)]
    struct RecordingApp {
        executed: Vec<RecordedTx>,
        balances: std::collections::HashMap<Address, U256>,
        nonces: std::collections::HashMap<Address, u32>,
    }

    #[cfg(test)]
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordedTx {
        UserOp(u8),
        Direct(u8),
    }

    #[cfg(test)]
    impl RecordingApp {
        fn events(&self) -> &[RecordedTx] {
            self.executed.as_slice()
        }

        fn balance_of(&self, sender: Address) -> U256 {
            *self.balances.get(&sender).unwrap_or(&U256::ZERO)
        }

        fn nonce_of(&self, sender: Address) -> u32 {
            self.nonces.get(&sender).copied().unwrap_or(0)
        }

        fn credit(&mut self, sender: Address, amount: u64) {
            let current = self.balance_of(sender);
            self.balances
                .insert(sender, current.saturating_add(U256::from(amount)));
        }
    }

    #[cfg(test)]
    impl Application for RecordingApp {
        const MAX_METHOD_PAYLOAD_BYTES: usize = app_core::application::MAX_METHOD_PAYLOAD_BYTES;

        fn current_user_nonce(&self, _sender: Address) -> u32 {
            self.nonce_of(_sender)
        }

        fn current_user_balance(&self, _sender: Address) -> U256 {
            self.balance_of(_sender)
        }

        fn validate_user_op(
            &self,
            sender: Address,
            user_op: &sequencer_core::user_op::UserOp,
            current_fee: u64,
        ) -> Result<(), sequencer_core::application::InvalidReason> {
            let expected_nonce = self.nonce_of(sender);
            if user_op.nonce != expected_nonce {
                return Err(sequencer_core::application::InvalidReason::InvalidNonce {
                    expected: expected_nonce,
                    got: user_op.nonce,
                });
            }
            if u64::from(user_op.max_fee) < current_fee {
                return Err(sequencer_core::application::InvalidReason::InvalidMaxFee {
                    max_fee: user_op.max_fee,
                    base_fee: current_fee,
                });
            }
            let required = U256::from(current_fee);
            let balance = self.balance_of(sender);
            if balance < required {
                return Err(
                    sequencer_core::application::InvalidReason::InsufficientGasBalance {
                        required,
                        available: balance,
                    },
                );
            }
            Ok(())
        }

        fn execute_valid_user_op(
            &mut self,
            user_op: &sequencer_core::l2_tx::ValidUserOp,
        ) -> Result<sequencer_core::application::AppOutputs, sequencer_core::application::AppError>
        {
            let sender = user_op.sender;
            let fee = U256::from(user_op.fee);
            let balance = self.balance_of(sender);
            if balance < fee {
                return Err(sequencer_core::application::AppError::Internal {
                    reason: "validated user op cannot pay fee".to_string(),
                });
            }
            self.balances.insert(sender, balance - fee);
            let next_nonce = self.nonce_of(sender).wrapping_add(1);
            self.nonces.insert(sender, next_nonce);

            let marker = user_op.data.first().copied().unwrap_or_default();
            self.executed.push(RecordedTx::UserOp(marker));
            Ok(Vec::new())
        }

        fn execute_direct_input(
            &mut self,
            input: &DirectInput,
        ) -> Result<sequencer_core::application::AppOutputs, sequencer_core::application::AppError>
        {
            let marker = input.payload.first().copied().unwrap_or(0);
            self.executed.push(RecordedTx::Direct(marker));
            Ok(Vec::new())
        }
    }

    const SEQUENCER: Address = address!("0x1111111111111111111111111111111111111111");
    const DIRECT_SENDER: Address = address!("0x2222222222222222222222222222222222222222");
    const TEST_CHAIN_ID: u64 = 1;
    const TEST_VERIFYING_CONTRACT: Address = Address::ZERO;

    fn test_domain() -> Eip712Domain {
        input_domain(TEST_CHAIN_ID, TEST_VERIFYING_CONTRACT)
    }

    fn direct_input(block: u64, marker: u8) -> SchedulerInput {
        SchedulerInput {
            sender: DIRECT_SENDER,
            inclusion_block: block,
            domain: test_domain(),
            payload: vec![marker],
        }
    }

    fn batch_input(block: u64, batch: Batch) -> SchedulerInput {
        SchedulerInput {
            sender: SEQUENCER,
            inclusion_block: block,
            domain: test_domain(),
            payload: ssz::Encode::as_ssz_bytes(&batch),
        }
    }

    fn address_from_signing_key(signing_key: &SigningKey) -> Address {
        let verifying = signing_key.verifying_key().to_encoded_point(false);
        Address::from_raw_public_key(&verifying.as_bytes()[1..])
    }

    fn sign_wire_user_op(
        domain: &Eip712Domain,
        signing_key: &SigningKey,
        nonce: u32,
        max_fee: u32,
        data: Vec<u8>,
    ) -> WireUserOp {
        let user_op = UserOp {
            nonce,
            max_fee,
            data: data.clone().into(),
        };
        let hash = user_op.eip712_signing_hash(domain);
        let k256_sig = signing_key
            .sign_prehash(hash.as_slice())
            .expect("sign user op hash");

        let sender = address_from_signing_key(signing_key);
        let signature = [false, true]
            .into_iter()
            .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
            .find(|candidate| {
                candidate
                    .recover_address_from_prehash(&hash)
                    .ok()
                    .map(|value| value == sender)
                    .unwrap_or(false)
            })
            .expect("recoverable parity for signature");

        WireUserOp {
            nonce,
            max_fee,
            data,
            signature: signature.as_bytes().to_vec(),
        }
    }

    #[test]
    fn batch_drains_safe_inputs_before_executing_user_ops() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        assert_eq!(
            scheduler.process_input(direct_input(10, 1)),
            ProcessOutcome::DirectEnqueued
        );

        let signing_key = SigningKey::from_bytes((&[1_u8; 32]).into()).expect("signing key");

        let batch = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![sign_wire_user_op(
                    &test_domain(),
                    &signing_key,
                    0,
                    1,
                    vec![2],
                )],
                safe_block: 10,
                fee_price: 0,
            }],
        };

        assert_eq!(
            scheduler.process_input(batch_input(20, batch)),
            ProcessOutcome::BatchExecuted
        );
        assert_eq!(
            scheduler.app.events(),
            [RecordedTx::Direct(1), RecordedTx::UserOp(2)]
        );
        assert_eq!(scheduler.queued_direct_len(), 0);
    }

    #[test]
    fn pre_batch_backstop_executes_overdue_directs_before_user_ops() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 5,
            },
        );

        scheduler.process_input(direct_input(1, 1));
        let signing_key = SigningKey::from_bytes((&[2_u8; 32]).into()).expect("signing key");
        let batch = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![sign_wire_user_op(
                    &test_domain(),
                    &signing_key,
                    0,
                    1,
                    vec![2],
                )],
                safe_block: 2,
                fee_price: 0,
            }],
        };

        scheduler.process_input(batch_input(6, batch));
        assert_eq!(
            scheduler.app.events(),
            [RecordedTx::Direct(1), RecordedTx::UserOp(2)]
        );
    }

    #[test]
    fn stale_batch_is_skipped_and_consumes_nonce() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 5,
            },
        );

        scheduler.process_input(direct_input(1, 9));
        let signing_key = SigningKey::from_bytes((&[3_u8; 32]).into()).expect("signing key");
        let stale_batch = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![sign_wire_user_op(
                    &test_domain(),
                    &signing_key,
                    0,
                    1,
                    vec![7],
                )],
                safe_block: 4,
                fee_price: 0,
            }],
        };

        let outcome = scheduler.process_input(batch_input(10, stale_batch));
        assert_eq!(outcome, ProcessOutcome::BatchSkippedStale);
        assert_eq!(scheduler.app.events(), [RecordedTx::Direct(9)]);
        assert_eq!(scheduler.next_expected_batch_nonce(), 1);

        let fresh_signing_key =
            SigningKey::from_bytes((&[13_u8; 32]).into()).expect("fresh signing key");
        let fresh_batch = Batch {
            nonce: 1,
            frames: vec![Frame {
                user_ops: vec![sign_wire_user_op(
                    &test_domain(),
                    &fresh_signing_key,
                    0,
                    1,
                    vec![8],
                )],
                safe_block: 10,
                fee_price: 0,
            }],
        };

        assert_eq!(
            scheduler.process_input(batch_input(10, fresh_batch)),
            ProcessOutcome::BatchExecuted
        );
    }

    #[test]
    fn non_monotonic_safe_blocks_invalidate_batch() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        let signing_key_a = SigningKey::from_bytes((&[4_u8; 32]).into()).expect("signing key a");
        let signing_key_b = SigningKey::from_bytes((&[5_u8; 32]).into()).expect("signing key b");
        let invalid = Batch {
            nonce: 0,
            frames: vec![
                Frame {
                    user_ops: vec![sign_wire_user_op(
                        &test_domain(),
                        &signing_key_a,
                        0,
                        1,
                        vec![1],
                    )],
                    safe_block: 8,
                    fee_price: 0,
                },
                Frame {
                    user_ops: vec![sign_wire_user_op(
                        &test_domain(),
                        &signing_key_b,
                        0,
                        1,
                        vec![2],
                    )],
                    safe_block: 7,
                    fee_price: 0,
                },
            ],
        };

        assert_eq!(
            scheduler.process_input(batch_input(10, invalid)),
            ProcessOutcome::BatchRejected(BatchRejectReason::NonMonotonicSafeBlocks)
        );
        assert!(scheduler.app.events().is_empty());
        assert_eq!(scheduler.next_expected_batch_nonce(), 0);
    }

    #[test]
    fn frame_safe_block_above_inclusion_block_invalidates_batch() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        let signing_key = SigningKey::from_bytes((&[6_u8; 32]).into()).expect("signing key");
        let invalid = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![sign_wire_user_op(
                    &test_domain(),
                    &signing_key,
                    0,
                    1,
                    vec![9],
                )],
                safe_block: 11,
                fee_price: 0,
            }],
        };

        assert_eq!(
            scheduler.process_input(batch_input(10, invalid)),
            ProcessOutcome::BatchRejected(BatchRejectReason::SafeBlockAboveInclusionBlock)
        );
        assert!(scheduler.app.events().is_empty());
        assert_eq!(scheduler.next_expected_batch_nonce(), 0);
    }

    #[test]
    fn frame_drain_uses_consistent_inclusive_safe_block_rule() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        scheduler.process_input(direct_input(10, 1));
        scheduler.process_input(direct_input(11, 2));
        let batch = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![],
                safe_block: 10,
                fee_price: 0,
            }],
        };

        scheduler.process_input(batch_input(12, batch));
        assert_eq!(scheduler.app.events(), [RecordedTx::Direct(1)]);
        assert_eq!(scheduler.queued_direct_len(), 1);
    }

    #[test]
    fn decode_failure_invalidates_batch_and_keeps_running() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        let bad_batch = SchedulerInput {
            sender: SEQUENCER,
            inclusion_block: 10,
            domain: test_domain(),
            payload: vec![0xFF, 0xEE, 0xDD],
        };
        assert_eq!(
            scheduler.process_input(bad_batch),
            ProcessOutcome::BatchRejected(BatchRejectReason::DecodeFailed)
        );
        assert_eq!(scheduler.next_expected_batch_nonce(), 0);

        assert_eq!(
            scheduler.process_input(direct_input(11, 3)),
            ProcessOutcome::DirectEnqueued
        );
        assert_eq!(scheduler.queued_direct_len(), 1);
    }

    #[test]
    fn backstop_drains_all_overdue_directs() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 5,
            },
        );

        scheduler.process_input(direct_input(1, 1));
        scheduler.process_input(direct_input(2, 2));
        scheduler.process_input(direct_input(8, 3));

        assert_eq!(
            scheduler.app.events(),
            [RecordedTx::Direct(1), RecordedTx::Direct(2)]
        );
        assert_eq!(scheduler.queued_direct_len(), 1);
    }

    #[test]
    fn invalid_signature_is_skipped() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        let batch = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![WireUserOp {
                    nonce: 0,
                    max_fee: 0,
                    data: vec![7],
                    signature: vec![0_u8; WireUserOp::SIGNATURE_BYTES],
                }],
                safe_block: 1,
                fee_price: 0,
            }],
        };

        assert_eq!(
            scheduler.process_input(batch_input(1, batch)),
            ProcessOutcome::BatchExecuted
        );
        assert!(scheduler.app.events().is_empty());
        assert_eq!(scheduler.next_expected_batch_nonce(), 1);
    }

    #[test]
    fn invalid_nonce_max_fee_or_balance_is_skipped() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );
        let signing_key = SigningKey::from_bytes((&[9_u8; 32]).into()).expect("signing key");
        let sender = address_from_signing_key(&signing_key);
        scheduler.app.credit(sender, 1);

        let bad_nonce = sign_wire_user_op(&test_domain(), &signing_key, 1, 10, vec![1]);
        let bad_max_fee = sign_wire_user_op(&test_domain(), &signing_key, 0, 0, vec![2]);
        let insufficient = sign_wire_user_op(&test_domain(), &signing_key, 0, 10, vec![3]);
        let valid = sign_wire_user_op(&test_domain(), &signing_key, 0, 10, vec![4]);

        let batch = Batch {
            nonce: 0,
            frames: vec![
                Frame {
                    user_ops: vec![bad_nonce],
                    safe_block: 1,
                    fee_price: 1,
                },
                Frame {
                    user_ops: vec![bad_max_fee],
                    safe_block: 1,
                    fee_price: 5,
                },
                Frame {
                    user_ops: vec![insufficient],
                    safe_block: 1,
                    fee_price: 2,
                },
                Frame {
                    user_ops: vec![valid],
                    safe_block: 1,
                    fee_price: 1,
                },
            ],
        };

        assert_eq!(
            scheduler.process_input(batch_input(1, batch)),
            ProcessOutcome::BatchExecuted
        );
        assert_eq!(scheduler.app.events(), [RecordedTx::UserOp(4)]);
    }

    #[test]
    fn empty_batches_are_valid_noops() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        let batch = Batch {
            nonce: 0,
            frames: vec![],
        };

        assert_eq!(
            scheduler.process_input(batch_input(10, batch)),
            ProcessOutcome::BatchExecuted
        );
        assert!(scheduler.app.events().is_empty());
        assert_eq!(scheduler.next_expected_batch_nonce(), 1);
    }

    #[test]
    fn batch_uses_input_domain_for_signature_recovery() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );
        let signing_key = SigningKey::from_bytes((&[10_u8; 32]).into()).expect("signing key");
        let sender = address_from_signing_key(&signing_key);
        scheduler.app.credit(sender, 1);
        let batch_domain = input_domain(
            TEST_CHAIN_ID + 7,
            address!("0x3333333333333333333333333333333333333333"),
        );
        let batch = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![sign_wire_user_op(
                    &batch_domain,
                    &signing_key,
                    0,
                    1,
                    vec![9],
                )],
                safe_block: 1,
                fee_price: 0,
            }],
        };

        let input = SchedulerInput {
            sender: SEQUENCER,
            inclusion_block: 1,
            domain: batch_domain,
            payload: ssz::Encode::as_ssz_bytes(&batch),
        };

        assert_eq!(
            scheduler.process_input(input),
            ProcessOutcome::BatchExecuted
        );
        assert_eq!(scheduler.app.events(), [RecordedTx::UserOp(9)]);
    }

    #[test]
    fn wrong_batch_nonce_is_rejected_without_consuming_nonce() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        let batch = Batch {
            nonce: 1,
            frames: vec![Frame {
                user_ops: vec![],
                safe_block: 1,
                fee_price: 0,
            }],
        };

        assert_eq!(
            scheduler.process_input(batch_input(1, batch)),
            ProcessOutcome::BatchRejected(BatchRejectReason::WrongNonce {
                expected: 0,
                got: 1,
            })
        );
        assert_eq!(scheduler.next_expected_batch_nonce(), 0);
        assert!(scheduler.app.events().is_empty());
    }
}
