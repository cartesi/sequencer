// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

pub mod fold;

pub use fold::{FoldInput, fold_replay};

use crate::application::{AppError, AppOutputs, Application, ExecutionOutcome};
use crate::batch::{Batch, Frame, WireUserOp};
use crate::history::ExecutedInputCount;
use crate::l2_tx::DirectInput;
use alloy_primitives::{Address, Signature};
use alloy_sol_types::Eip712Domain;
use alloy_sol_types::SolStruct;
use std::collections::VecDeque;

pub const MAX_WAIT_BLOCKS: u64 = crate::MAX_WAIT_BLOCKS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// L1 address whose inputs are trusted as sequencer batches; every other
    /// sender is a direct input. This is deployment/app data (which key the
    /// sequencer submits batches from), supplied by the app binary — the
    /// protocol library never hardcodes a concrete sequencer address.
    pub sequencer_address: Address,
    pub max_wait_blocks: u64,
}

impl SchedulerConfig {
    /// Production config for `sequencer_address`, pinning the staleness window
    /// to the protocol's [`MAX_WAIT_BLOCKS`]. Tests that need a custom window
    /// construct the struct directly.
    pub const fn new(sequencer_address: Address) -> Self {
        Self {
            sequencer_address,
            max_wait_blocks: MAX_WAIT_BLOCKS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerInput {
    pub sender: Address,
    pub inclusion_block: u64,
    pub domain: Eip712Domain,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOutcome {
    DirectEnqueued,
    BatchExecuted,
    BatchSkippedStale,
    BatchRejected(BatchRejectReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessResult {
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

// Test-assertion sugar only: asymmetric cross-type equality in a public
// consensus API makes `a == b` type-directed and non-obvious, so it stays out
// of the production surface.
#[cfg(test)]
impl PartialEq<ProcessOutcome> for ProcessResult {
    fn eq(&self, other: &ProcessOutcome) -> bool {
        self.outcome == *other
    }
}

#[cfg(test)]
impl PartialEq<ProcessResult> for ProcessOutcome {
    fn eq(&self, other: &ProcessResult) -> bool {
        *self == other.outcome
    }
}

/// Inspect query accepted by the scheduler's state export endpoint.
pub const STATE_INSPECT_QUERY: &[u8] = b"state";

#[derive(Debug, PartialEq, Eq)]
pub enum InspectError {
    UnsupportedQuery,
    Application(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchRejectReason {
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
    /// Construct a scheduler that begins expecting batch nonce
    /// `next_expected_batch_nonce`. The genesis scheduler starts at 0
    /// ([`Scheduler::new`]); the recovery fold engine resumes at the
    /// checkpoint's batch nonce, which the bare-metal app cannot recompute — it
    /// rides as checkpoint metadata.
    /// Sets the nonce field directly (not via the private advance).
    pub fn resume_at(app: A, config: SchedulerConfig, next_expected_batch_nonce: u64) -> Self {
        Self {
            app,
            config,
            direct_q: VecDeque::new(),
            next_expected_batch_nonce,
        }
    }

    pub fn new(app: A, config: SchedulerConfig) -> Self {
        assert_eq!(
            app.executed_input_count(),
            ExecutedInputCount::ZERO,
            "a genesis scheduler application must start at executed_input_count = 0"
        );
        Self::resume_at(app, config, 0)
    }

    /// Number of directs still queued in the fridge. The recovery fold asserts
    /// this is zero after draining at the stop block `C`: any leftover direct
    /// means an input arrived with `inclusion_block > C` (a caller contract
    /// violation), which would otherwise be silently dropped by [`finish`].
    pub fn queued_direct_len(&self) -> usize {
        self.direct_q.len()
    }

    /// The batch nonce the scheduler next expects (and that a resumed sequencer
    /// submits at). The recovery fold reads it via [`Scheduler::finish`] as the
    /// resume nonce `N`.
    pub fn next_expected_batch_nonce(&self) -> u64 {
        self.next_expected_batch_nonce
    }

    /// Seed the fridge (direct-input queue) with a reconstructed direct. The
    /// recovery fold uses this to rebuild the scheduler's `(A, B]` fridge before
    /// replaying `(B, C]`. Callers MUST enqueue in ascending
    /// L1 order (`inclusion_block`) so the FIFO drain matches the on-chain order.
    pub fn enqueue_direct(&mut self, sender: Address, inclusion_block: u64, payload: Vec<u8>) {
        self.direct_q.push_back(QueuedDirectInput {
            sender,
            payload,
            inclusion_block,
        });
    }

    /// Execute every fridge direct covered by `safe_block` (those with
    /// `inclusion_block <= safe_block`), returning their outputs. The recovery
    /// fold calls this once at the stopping block `C`: every
    /// still-queued direct has `inclusion_block <= C`, so this drains them all —
    /// exactly what the booting run's first frame at a safe block `>= C` would
    /// do, except the booting run (bare-metal, no fridge) never sees them.
    pub fn drain_covered_at(&mut self, safe_block: u64) -> Result<AppOutputs, AppError> {
        let mut outputs = Vec::new();
        self.drain_directs_safe_at(safe_block, &mut outputs)?;
        Ok(outputs)
    }

    /// Consume the scheduler, returning the advanced application state `S'` and
    /// the resume batch nonce `N`: `S'` is the folded app state, `N`
    /// the nonce the new sequencer resumes submitting at.
    pub fn finish(self) -> (A, u64) {
        (self.app, self.next_expected_batch_nonce)
    }

    /// Watchdog / CM `inspect_state` hook: the app's canonical snapshot bytes
    /// for the `/finalized_state` byte-compare. `pub` (not `pub(super)`) because
    /// the canonical-app harness is now a separate crate over this library.
    pub fn inspect_state(&self, query: &[u8]) -> Result<Vec<u8>, InspectError> {
        if !query.is_empty() && query != STATE_INSPECT_QUERY {
            return Err(InspectError::UnsupportedQuery);
        }

        self.app
            .canonical_snapshot_bytes()
            .map_err(|err| InspectError::Application(err.to_string()))
    }

    pub fn process_input(&mut self, input: SchedulerInput) -> Result<ProcessResult, AppError> {
        // Execute overdue directs before any input to keep backstop semantics explicit.
        let mut outputs = Vec::new();
        self.force_execute_overdue(input.inclusion_block, &mut outputs)?;

        if input.sender != self.config.sequencer_address {
            self.direct_q.push_back(QueuedDirectInput {
                sender: input.sender,
                payload: input.payload,
                inclusion_block: input.inclusion_block,
            });
            Ok(ProcessResult::new(ProcessOutcome::DirectEnqueued, outputs))
        } else {
            let batch_result =
                self.process_batch_payload(input.inclusion_block, &input.domain, &input.payload)?;
            outputs.extend(batch_result.outputs);
            Ok(ProcessResult::new(batch_result.outcome, outputs))
        }
    }

    fn process_batch_payload(
        &mut self,
        inclusion_block: u64,
        domain: &Eip712Domain,
        payload: &[u8],
    ) -> Result<ProcessResult, AppError> {
        let Ok(batch): Result<Batch, _> = ssz::Decode::from_ssz_bytes(payload) else {
            return Ok(ProcessResult::without_outputs(
                ProcessOutcome::BatchRejected(BatchRejectReason::DecodeFailed),
            ));
        };

        if batch.nonce != self.next_expected_batch_nonce {
            return Ok(ProcessResult::without_outputs(
                ProcessOutcome::BatchRejected(BatchRejectReason::WrongNonce {
                    expected: self.next_expected_batch_nonce,
                    got: batch.nonce,
                }),
            ));
        }

        let Some((frame_head, frame_tail)) = batch.frames.split_first() else {
            let next_nonce = self.checked_next_batch_nonce();
            self.next_expected_batch_nonce = next_nonce;
            return Ok(ProcessResult::without_outputs(
                ProcessOutcome::BatchExecuted,
            ));
        };

        if let Some(reason) =
            self.batch_reject_reason_for_block(inclusion_block, frame_head, frame_tail)
        {
            return Ok(ProcessResult::without_outputs(
                ProcessOutcome::BatchRejected(reason),
            ));
        }

        if has_elapsed_since(
            frame_head.safe_block,
            self.config.max_wait_blocks,
            inclusion_block,
        ) {
            return Ok(ProcessResult::without_outputs(
                ProcessOutcome::BatchSkippedStale,
            ));
        }

        // Preflight nonce exhaustion before any frame can mutate application
        // state. A batch at `u64::MAX` has no canonical successor.
        let next_nonce = self.checked_next_batch_nonce();
        let mut outputs = Vec::new();
        for frame in &batch.frames {
            self.drain_directs_safe_at(frame.safe_block, &mut outputs)?;
            self.execute_frame_user_ops(domain, frame, &mut outputs)?;
        }

        self.next_expected_batch_nonce = next_nonce;
        Ok(ProcessResult::new(ProcessOutcome::BatchExecuted, outputs))
    }

    fn checked_next_batch_nonce(&self) -> u64 {
        self.next_expected_batch_nonce
            .checked_add(1)
            .expect("batch nonce overflow: no canonical successor")
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
    /// Both `max_fee` and `fee_price` are log-space exponents (base 129/128).
    /// See [`crate::fee`] for conversion to linear amounts.
    fn execute_frame_user_ops(
        &mut self,
        domain: &Eip712Domain,
        frame: &Frame,
        outputs: &mut AppOutputs,
    ) -> Result<(), AppError> {
        for user_op in &frame.user_ops {
            // An unrecoverable signature is dropped silently (the scheduler is a
            // pure deterministic fold; diagnostics would be a nondeterministic
            // side effect at the library seam).
            if let Some(sender) = self.recover_sender(domain, user_op) {
                let plain = user_op.to_user_op();
                match crate::application::validate_and_execute_user_op(
                    &mut self.app,
                    sender,
                    &plain,
                    frame.fee_price,
                    frame.safe_block,
                )? {
                    ExecutionOutcome::Included(executed) => outputs.extend(executed.outputs),
                    ExecutionOutcome::Invalid(_) => {}
                }
            }
        }
        Ok(())
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

    fn drain_directs_safe_at(
        &mut self,
        safe_block: u64,
        outputs: &mut AppOutputs,
    ) -> Result<(), AppError> {
        while let Some(front) = self.direct_q.front() {
            if front.inclusion_block > safe_block {
                break;
            }
            let input = DirectInput {
                sender: front.sender,
                block_number: front.inclusion_block,
                payload: front.payload.clone(),
            };
            let executed = crate::application::execute_direct_input(&mut self.app, &input)?;
            outputs.extend(executed.outputs);
            self.direct_q.pop_front().expect("queue front must exist");
        }
        Ok(())
    }

    fn force_execute_overdue(
        &mut self,
        current_block: u64,
        outputs: &mut AppOutputs,
    ) -> Result<(), AppError> {
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
                let executed = crate::application::execute_direct_input(&mut self.app, &input)?;
                outputs.extend(executed.outputs);

                self.direct_q.pop_front().expect("queue front must exist");
            } else {
                break;
            }
        }
        Ok(())
    }
}

/// Scheduler-local spelling of the one staleness predicate. Delegates to
/// [`crate::protocol::age_exceeds`] so the fold and the off-chain protocol
/// module cannot drift. Note the argument order differs:
/// `has_elapsed_since(start, wait, current) == age_exceeds(current, start, wait)`.
fn has_elapsed_since(start_block: u64, wait_blocks: u64, current_block: u64) -> bool {
    crate::protocol::age_exceeds(current_block, start_block, wait_blocks)
}

pub fn input_domain(chain_id: u64, verifying_contract: Address) -> Eip712Domain {
    crate::build_input_domain(chain_id, verifying_contract)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{ApplicationProgress, ApplyInputCapability, ProgressCommitCapability};
    use crate::user_op::UserOp;
    use alloy_primitives::{U256, address};
    use k256::ecdsa::SigningKey;
    use k256::ecdsa::signature::hazmat::PrehashSigner;

    #[cfg(test)]
    #[derive(Default)]
    struct RecordingApp {
        executed: Vec<RecordedTx>,
        balances: std::collections::HashMap<Address, U256>,
        nonces: std::collections::HashMap<Address, u32>,
        progress: ApplicationProgress,
        fail_on: Option<RecordedTx>,
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

        fn with_progress(executed_input_count: u64, last_executed_safe_block: u64) -> Self {
            Self {
                progress: ApplicationProgress::try_new(
                    ExecutedInputCount::new(executed_input_count),
                    last_executed_safe_block,
                )
                .expect("coherent progress"),
                ..Self::default()
            }
        }
    }

    #[cfg(test)]
    impl Application for RecordingApp {
        // Mirrors the wallet app's method-payload cap (selector + amount +
        // address). A local literal keeps sequencer-core free of an app-core
        // dependency (which would invert the crate graph).
        const MAX_METHOD_PAYLOAD_BYTES: usize = 1 + 32 + 20;

        fn validate_user_op(
            &self,
            sender: Address,
            user_op: &crate::user_op::UserOp,
            current_fee: u16,
        ) -> Result<(), crate::application::InvalidReason> {
            let expected_nonce = self.nonce_of(sender);
            if user_op.nonce != expected_nonce {
                return Err(crate::application::InvalidReason::InvalidNonce {
                    expected: expected_nonce,
                    got: user_op.nonce,
                });
            }
            if user_op.max_fee < current_fee {
                return Err(crate::application::InvalidReason::InvalidMaxFee {
                    max_fee: user_op.max_fee,
                    base_fee: current_fee,
                });
            }
            let required = crate::fee::fee_to_linear(current_fee);
            let balance = self.balance_of(sender);
            if balance < required {
                return Err(crate::application::InvalidReason::InsufficientFeeBalance {
                    required,
                    available: balance,
                });
            }
            Ok(())
        }

        fn apply_valid_user_op(
            &mut self,
            _capability: ApplyInputCapability<'_>,
            user_op: &crate::l2_tx::ValidUserOp,
            _safe_block: u64,
        ) -> Result<crate::application::AppOutputs, crate::application::AppError> {
            let marker = user_op.data.first().copied().unwrap_or_default();
            let event = RecordedTx::UserOp(marker);
            if self.fail_on.as_ref() == Some(&event) {
                return Err(AppError::Internal {
                    reason: format!("refusing {event:?}"),
                });
            }

            let sender = user_op.sender;
            let fee = crate::fee::fee_to_linear(user_op.fee);
            let balance = self.balance_of(sender);
            if balance < fee {
                return Err(crate::application::AppError::Internal {
                    reason: "validated user op cannot pay fee".to_string(),
                });
            }
            self.balances.insert(sender, balance - fee);
            let next_nonce = self.nonce_of(sender).wrapping_add(1);
            self.nonces.insert(sender, next_nonce);

            self.executed.push(event);
            Ok(Vec::new())
        }

        fn apply_direct_input(
            &mut self,
            _capability: ApplyInputCapability<'_>,
            input: &DirectInput,
        ) -> Result<crate::application::AppOutputs, crate::application::AppError> {
            let marker = input.payload.first().copied().unwrap_or(0);
            let event = RecordedTx::Direct(marker);
            if self.fail_on.as_ref() == Some(&event) {
                return Err(AppError::Internal {
                    reason: format!("refusing {event:?}"),
                });
            }
            self.executed.push(event);
            Ok(Vec::new())
        }

        fn execution_progress(&self) -> &ApplicationProgress {
            &self.progress
        }

        fn execution_progress_mut(
            &mut self,
            _capability: ProgressCommitCapability<'_>,
        ) -> &mut ApplicationProgress {
            &mut self.progress
        }

        fn from_dump(_prefix: &std::path::Path) -> Result<Self, crate::application::AppError> {
            unimplemented!("RecordingApp does not participate in snapshot lifecycle")
        }

        fn create_dump(
            &self,
            _prefix: &std::path::Path,
        ) -> Result<(), crate::application::AppError> {
            unimplemented!("RecordingApp does not participate in snapshot lifecycle")
        }

        fn delete_dump(_prefix: &std::path::Path) -> Result<(), crate::application::AppError> {
            unimplemented!("RecordingApp does not participate in snapshot lifecycle")
        }

        fn state_file_in_dump(_prefix: &std::path::Path) -> std::path::PathBuf {
            unimplemented!("RecordingApp does not participate in snapshot lifecycle")
        }

        fn canonical_snapshot_bytes(&self) -> Result<Vec<u8>, crate::application::AppError> {
            Ok(format!("events:{}", self.executed.len()).into_bytes())
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

    fn process_ok<A: Application>(
        scheduler: &mut Scheduler<A>,
        input: SchedulerInput,
    ) -> ProcessResult {
        scheduler
            .process_input(input)
            .expect("test application execution must succeed")
    }

    fn address_from_signing_key(signing_key: &SigningKey) -> Address {
        let verifying = signing_key.verifying_key().to_encoded_point(false);
        Address::from_raw_public_key(&verifying.as_bytes()[1..])
    }

    fn sign_wire_user_op(
        domain: &Eip712Domain,
        signing_key: &SigningKey,
        nonce: u32,
        max_fee: u16,
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
            process_ok(&mut scheduler, direct_input(10, 1)),
            ProcessOutcome::DirectEnqueued
        );

        let signing_key = SigningKey::from_bytes((&[1_u8; 32]).into()).expect("signing key");
        let sender = address_from_signing_key(&signing_key);
        scheduler.app.credit(sender, 1);

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
            process_ok(&mut scheduler, batch_input(20, batch)),
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

        process_ok(&mut scheduler, direct_input(1, 1));
        let signing_key = SigningKey::from_bytes((&[2_u8; 32]).into()).expect("signing key");
        let sender = address_from_signing_key(&signing_key);
        scheduler.app.credit(sender, 1);
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

        process_ok(&mut scheduler, batch_input(6, batch));
        assert_eq!(
            scheduler.app.events(),
            [RecordedTx::Direct(1), RecordedTx::UserOp(2)]
        );
    }

    #[test]
    fn stale_batch_is_skipped_without_consuming_nonce() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 5,
            },
        );

        process_ok(&mut scheduler, direct_input(1, 9));
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

        let outcome = process_ok(&mut scheduler, batch_input(10, stale_batch));
        assert_eq!(outcome, ProcessOutcome::BatchSkippedStale);
        assert_eq!(scheduler.app.events(), [RecordedTx::Direct(9)]);
        // Stale batches do NOT consume the nonce — they are true no-ops in nonce space.
        assert_eq!(scheduler.next_expected_batch_nonce(), 0);

        // The next valid batch reuses nonce 0.
        let fresh_signing_key =
            SigningKey::from_bytes((&[13_u8; 32]).into()).expect("fresh signing key");
        let fresh_sender = address_from_signing_key(&fresh_signing_key);
        scheduler.app.credit(fresh_sender, 1);
        let fresh_batch = Batch {
            nonce: 0,
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
            process_ok(&mut scheduler, batch_input(10, fresh_batch)),
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
            process_ok(&mut scheduler, batch_input(10, invalid)),
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
            process_ok(&mut scheduler, batch_input(10, invalid)),
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

        process_ok(&mut scheduler, direct_input(10, 1));
        process_ok(&mut scheduler, direct_input(11, 2));
        let batch = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![],
                safe_block: 10,
                fee_price: 0,
            }],
        };

        process_ok(&mut scheduler, batch_input(12, batch));
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
            process_ok(&mut scheduler, bad_batch),
            ProcessOutcome::BatchRejected(BatchRejectReason::DecodeFailed)
        );
        assert_eq!(scheduler.next_expected_batch_nonce(), 0);

        assert_eq!(
            process_ok(&mut scheduler, direct_input(11, 3)),
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

        process_ok(&mut scheduler, direct_input(1, 1));
        process_ok(&mut scheduler, direct_input(2, 2));
        process_ok(&mut scheduler, direct_input(8, 3));

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
            process_ok(&mut scheduler, batch_input(1, batch)),
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
        // fee_to_linear(0) = 1, so credit 1 unit — just enough for the cheapest fee.
        scheduler.app.credit(sender, 1);

        let bad_nonce = sign_wire_user_op(&test_domain(), &signing_key, 1, 10, vec![1]);
        let bad_max_fee = sign_wire_user_op(&test_domain(), &signing_key, 0, 0, vec![2]);
        // max_fee=1000 is high enough, but fee_to_linear(1000) ≈ 2397 > balance of 1.
        let insufficient = sign_wire_user_op(&test_domain(), &signing_key, 0, 1000, vec![3]);
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
                    fee_price: 1000,
                },
                Frame {
                    user_ops: vec![valid],
                    safe_block: 1,
                    fee_price: 0,
                },
            ],
        };

        assert_eq!(
            process_ok(&mut scheduler, batch_input(1, batch)),
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
            process_ok(&mut scheduler, batch_input(10, batch)),
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
            process_ok(&mut scheduler, input),
            ProcessOutcome::BatchExecuted
        );
        assert_eq!(scheduler.app.events(), [RecordedTx::UserOp(9)]);
    }

    #[test]
    fn inspect_exports_application_state_for_state_query() {
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );
        assert_eq!(
            process_ok(&mut scheduler, direct_input(1, 7)),
            ProcessOutcome::DirectEnqueued
        );
        // Inspect reflects executed app state, not the direct-input queue.
        let state = scheduler
            .inspect_state(STATE_INSPECT_QUERY)
            .expect("inspect state");
        assert_eq!(state, b"events:0");

        let batch = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![],
                safe_block: 1,
                fee_price: 0,
            }],
        };
        assert_eq!(
            process_ok(&mut scheduler, batch_input(2, batch)),
            ProcessOutcome::BatchExecuted
        );

        let state = scheduler
            .inspect_state(STATE_INSPECT_QUERY)
            .expect("inspect state after drain");
        assert_eq!(state, b"events:1");
    }

    #[test]
    fn inspect_rejects_unsupported_query() {
        let scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        assert_eq!(
            scheduler.inspect_state(b"balances"),
            Err(InspectError::UnsupportedQuery)
        );
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
            process_ok(&mut scheduler, batch_input(1, batch)),
            ProcessOutcome::BatchRejected(BatchRejectReason::WrongNonce {
                expected: 0,
                got: 1,
            })
        );
        assert_eq!(scheduler.next_expected_batch_nonce(), 0);
        assert!(scheduler.app.events().is_empty());
    }

    // ── I1 duality: off-chain predicate vs canonical fold ─────────────────
    //
    // `ProtocolTiming::scheduler_accepts` (the off-chain gold-frontier predicate)
    // and `Scheduler::process_input` (the canonical fold) are hand-maintained in
    // separate files and MUST agree on accept-vs-reject for every input — the
    // system's most load-bearing invariant (I1). Nothing exercised both until
    // now. The one *documented* divergence is the predicate's structural-reject
    // omission (it trusts the sequencer to emit well-formed batches), pinned
    // explicitly at the end so any *other* drift fails this test.

    #[test]
    #[should_panic(
        expected = "a genesis scheduler application must start at executed_input_count = 0"
    )]
    fn genesis_scheduler_rejects_nonzero_application_progress() {
        let _ = Scheduler::new(
            RecordingApp::with_progress(1, 0),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );
    }

    #[test]
    fn application_error_is_fatal_before_later_user_ops_and_nonce_commit() {
        let app = RecordingApp {
            fail_on: Some(RecordedTx::UserOp(1)),
            ..RecordingApp::default()
        };
        let mut scheduler = Scheduler::new(
            app,
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );

        let signing_key = SigningKey::from_bytes((&[17_u8; 32]).into()).expect("signing key");
        let sender = address_from_signing_key(&signing_key);
        scheduler.app.credit(sender, 2);
        let batch = Batch {
            nonce: 0,
            frames: vec![Frame {
                user_ops: vec![
                    sign_wire_user_op(&test_domain(), &signing_key, 0, 1, vec![1]),
                    sign_wire_user_op(&test_domain(), &signing_key, 1, 1, vec![2]),
                ],
                safe_block: 1,
                fee_price: 0,
            }],
        };

        let error = scheduler
            .process_input(batch_input(1, batch))
            .expect_err("application error must abort scheduler processing");
        let AppError::Internal { reason } = error else {
            panic!("unexpected application error: {error}");
        };
        assert_eq!(reason, "refusing UserOp(1)");
        assert!(scheduler.app.events().is_empty());
        assert_eq!(
            scheduler.app.executed_input_count(),
            ExecutedInputCount::ZERO
        );
        assert_eq!(scheduler.next_expected_batch_nonce(), 0);
    }

    #[test]
    fn overdue_direct_error_is_fatal_before_current_input_is_classified() {
        let app = RecordingApp {
            fail_on: Some(RecordedTx::Direct(1)),
            ..RecordingApp::default()
        };
        let mut scheduler = Scheduler::new(
            app,
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 5,
            },
        );

        assert_eq!(
            process_ok(&mut scheduler, direct_input(1, 1)),
            ProcessOutcome::DirectEnqueued
        );
        let error = scheduler
            .process_input(direct_input(6, 2))
            .expect_err("overdue direct failure must abort scheduler processing");
        let AppError::Internal { reason } = error else {
            panic!("unexpected application error: {error}");
        };
        assert_eq!(reason, "refusing Direct(1)");
        assert!(scheduler.app.events().is_empty());
        assert_eq!(
            scheduler.app.executed_input_count(),
            ExecutedInputCount::ZERO
        );
        assert_eq!(scheduler.queued_direct_len(), 1);
    }

    #[test]
    fn batch_nonce_overflow_panics_before_application_mutation() {
        let mut scheduler = Scheduler::resume_at(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
            u64::MAX,
        );
        let signing_key = SigningKey::from_bytes((&[18_u8; 32]).into()).expect("signing key");
        let sender = address_from_signing_key(&signing_key);
        scheduler.app.credit(sender, 1);
        let batch = Batch {
            nonce: u64::MAX,
            frames: vec![Frame {
                user_ops: vec![sign_wire_user_op(
                    &test_domain(),
                    &signing_key,
                    0,
                    1,
                    vec![7],
                )],
                safe_block: 1,
                fee_price: 0,
            }],
        };

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = scheduler.process_input(batch_input(1, batch));
        }));
        assert!(panic.is_err(), "nonce exhaustion must fail loud");
        assert!(scheduler.app.events().is_empty());
        assert_eq!(
            scheduler.app.executed_input_count(),
            ExecutedInputCount::ZERO
        );
        assert_eq!(scheduler.next_expected_batch_nonce(), u64::MAX);
    }

    #[test]
    fn executed_input_count_overflow_panics_before_application_mutation() {
        let mut scheduler = Scheduler::resume_at(
            RecordingApp::with_progress(u64::MAX, 0),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
            0,
        );
        assert_eq!(
            process_ok(&mut scheduler, direct_input(1, 9)),
            ProcessOutcome::DirectEnqueued
        );

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = scheduler.drain_covered_at(1);
        }));
        assert!(panic.is_err(), "input-count exhaustion must fail loud");
        assert!(scheduler.app.events().is_empty());
        assert_eq!(
            scheduler.app.executed_input_count(),
            ExecutedInputCount::new(u64::MAX)
        );
        assert_eq!(scheduler.queued_direct_len(), 1);
    }

    const DUALITY_MAX_WAIT: u64 = 5;

    fn duality_timing() -> crate::protocol::ProtocolTiming {
        crate::protocol::ProtocolTiming {
            max_wait_blocks: DUALITY_MAX_WAIT,
            preemptive_margin_blocks: 1,
            l1_read_stale_after_blocks: 1,
            seconds_per_block: 12,
        }
    }

    // A frame with no user ops — accept/reject is then purely about
    // sender/nonce/structure/staleness, no app credit needed.
    fn empty_frame(safe_block: u64) -> Frame {
        Frame {
            user_ops: vec![],
            safe_block,
            fee_price: 0,
        }
    }

    /// Run one input through BOTH sides at `expected_nonce`; return whether each
    /// *accepted* it (canonical `BatchExecuted` / predicate `Some`).
    fn duality_run(
        sender: Address,
        inclusion: u64,
        expected_nonce: u64,
        payload: &[u8],
    ) -> (bool, bool) {
        use crate::protocol::SafeInputView;
        let mut scheduler = Scheduler::resume_at(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: DUALITY_MAX_WAIT,
            },
            expected_nonce,
        );
        let canonical_executed = process_ok(
            &mut scheduler,
            SchedulerInput {
                sender,
                inclusion_block: inclusion,
                domain: test_domain(),
                payload: payload.to_vec(),
            },
        ) == ProcessOutcome::BatchExecuted;
        let offchain_accepted = duality_timing()
            .scheduler_accepts(
                SEQUENCER,
                SafeInputView {
                    safe_input_index: 0,
                    sender,
                    payload,
                    inclusion_block: inclusion,
                },
                expected_nonce,
            )
            .is_some();
        (canonical_executed, offchain_accepted)
    }

    fn ssz(batch: &Batch) -> Vec<u8> {
        ssz::Encode::as_ssz_bytes(batch)
    }

    #[test]
    fn scheduler_accepts_agrees_with_canonical_on_accept_reject() {
        // (label, sender, inclusion, expected_nonce, payload) — structurally
        // valid inputs where the two sides MUST agree.
        let valid_fresh = ssz(&Batch {
            nonce: 0,
            frames: vec![empty_frame(10)],
        });
        let empty_frames = ssz(&Batch {
            nonce: 0,
            frames: vec![],
        });
        let wrong_nonce = ssz(&Batch {
            nonce: 1,
            frames: vec![empty_frame(10)],
        });
        let stale = ssz(&Batch {
            nonce: 0,
            frames: vec![empty_frame(1)],
        });
        let fresh_at_boundary = ssz(&Batch {
            nonce: 0,
            frames: vec![empty_frame(10)],
        });

        let agree_cases: &[(&str, Address, u64, u64, &[u8])] = &[
            ("valid fresh, right nonce", SEQUENCER, 12, 0, &valid_fresh),
            (
                "empty frames (no-op batch)",
                SEQUENCER,
                100,
                0,
                &empty_frames,
            ),
            ("wrong nonce", SEQUENCER, 12, 0, &wrong_nonce),
            // age = 10 - 1 = 9 >= MAX_WAIT(5): stale on both sides.
            ("stale by first frame", SEQUENCER, 10, 0, &stale),
            // wrong sender: canonical enqueues a direct, predicate rejects sender.
            ("wrong sender", DIRECT_SENDER, 12, 0, &valid_fresh),
            // garbage payload: DecodeFailed / decode error.
            ("garbage payload", SEQUENCER, 1, 0, &[0xFF, 0xEE, 0xDD]),
            // staleness boundary: age = 14 - 10 = 4 < 5 accepted.
            (
                "fresh just under stale",
                SEQUENCER,
                14,
                0,
                &fresh_at_boundary,
            ),
            // age = 15 - 10 = 5 >= 5: stale.
            (
                "stale just at boundary",
                SEQUENCER,
                15,
                0,
                &fresh_at_boundary,
            ),
        ];

        for (label, sender, inclusion, expected, payload) in agree_cases {
            let (canonical, offchain) = duality_run(*sender, *inclusion, *expected, payload);
            assert_eq!(
                canonical, offchain,
                "I1 disagreement on `{label}`: canonical_executed={canonical}, \
                 offchain_accepted={offchain} — the predicate and the canonical \
                 fold must agree on accept/reject"
            );
        }

        // Documented exception — the predicate's structural-reject omission. The
        // canonical fold rejects these (it checks frame structure); the predicate
        // accepts them (self-trust: the sequencer never emits such batches). If
        // either side changes, these asserts flip and force a deliberate update.
        let non_monotonic = ssz(&Batch {
            nonce: 0,
            frames: vec![empty_frame(8), empty_frame(7)],
        });
        let (canonical, offchain) = duality_run(SEQUENCER, 12, 0, &non_monotonic);
        assert!(
            !canonical && offchain,
            "non-monotonic safe_blocks: expected canonical reject + predicate \
             accept (documented structural omission), got canonical={canonical} \
             offchain={offchain}"
        );

        let frame_above_inclusion = ssz(&Batch {
            nonce: 0,
            frames: vec![empty_frame(20)],
        });
        let (canonical, offchain) = duality_run(SEQUENCER, 12, 0, &frame_above_inclusion);
        assert!(
            !canonical && offchain,
            "frame safe_block > inclusion: expected canonical reject + predicate \
             accept (documented structural omission), got canonical={canonical} \
             offchain={offchain}"
        );
    }

    #[test]
    fn force_execute_overdue_runs_before_a_rejected_or_stale_batch() {
        // The backstop force-executes overdue fridge directs at the START of
        // process_input — before the batch is even classified. So an overdue
        // direct is drained even when the same tick's batch is rejected or
        // skipped (a danger-zone shape). Previously tested only alongside an
        // ACCEPTED batch.
        for label in ["wrong-nonce", "stale"] {
            let mut scheduler = Scheduler::new(
                RecordingApp::default(),
                SchedulerConfig {
                    sequencer_address: SEQUENCER,
                    max_wait_blocks: 5,
                },
            );
            // A direct at block 1; by inclusion block 8 it is overdue (age 7 >= 5).
            process_ok(&mut scheduler, direct_input(1, 1));

            let batch = if label == "wrong-nonce" {
                // expected 0, got 9 → BatchRejected(WrongNonce).
                Batch {
                    nonce: 9,
                    frames: vec![Frame {
                        user_ops: vec![],
                        safe_block: 8,
                        fee_price: 0,
                    }],
                }
            } else {
                // right nonce but a stale first frame (age 8 - 1 = 7 >= 5).
                Batch {
                    nonce: 0,
                    frames: vec![Frame {
                        user_ops: vec![],
                        safe_block: 1,
                        fee_price: 0,
                    }],
                }
            };

            let outcome = process_ok(&mut scheduler, batch_input(8, batch)).outcome;
            assert!(
                matches!(
                    outcome,
                    ProcessOutcome::BatchRejected(_) | ProcessOutcome::BatchSkippedStale
                ),
                "{label}: batch should be rejected/skipped, got {outcome:?}"
            );
            assert_eq!(
                scheduler.app.events(),
                [RecordedTx::Direct(1)],
                "{label}: the overdue direct must be force-executed before the rejected batch"
            );
            assert_eq!(
                scheduler.next_expected_batch_nonce(),
                0,
                "{label}: a rejected/stale batch consumes no nonce"
            );
        }
    }

    #[test]
    fn multiframe_overheight_in_tail_frame_rejected_by_scheduler() {
        // `batch_reject_reason_for_block` checks `safe_block <= inclusion` in
        // BOTH the head and the tail frames; only the head case was tested. A
        // tail frame above the inclusion block must reject the whole batch.
        let mut scheduler = Scheduler::new(
            RecordingApp::default(),
            SchedulerConfig {
                sequencer_address: SEQUENCER,
                max_wait_blocks: 100,
            },
        );
        let batch = Batch {
            nonce: 0,
            frames: vec![
                // head: 5 <= 10, valid.
                Frame {
                    user_ops: vec![],
                    safe_block: 5,
                    fee_price: 0,
                },
                // tail: 11 > 10 (the inclusion block) → reject.
                Frame {
                    user_ops: vec![],
                    safe_block: 11,
                    fee_price: 0,
                },
            ],
        };
        assert_eq!(
            process_ok(&mut scheduler, batch_input(10, batch)),
            ProcessOutcome::BatchRejected(BatchRejectReason::SafeBlockAboveInclusionBlock)
        );
        assert_eq!(scheduler.next_expected_batch_nonce(), 0);
        assert!(scheduler.app.events().is_empty());
    }
}
