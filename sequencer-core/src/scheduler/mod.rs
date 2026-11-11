// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The canonical scheduler: it turns an application's L1 inputs, in InputBox
//! order, into the one sequence of L2 transactions the application executes.
//!
//! It reads nothing from the application, neither its state nor its code: the
//! fridge admits by sender and payload length, and a user op is handed out
//! once its sender recovers and its cap covers the frame's price. That is what
//! lets a recovery schedule a whole range before any application runs. The
//! nonce is the executor's check ([`crate::application::execute`]);
//! [`driver::CanonicalRunner`] joins the two for the guest and the tests.
//!
//! The rules are those of the scheduler contract
//! (docs/protocol/scheduler-semantics.md), in its order.

pub mod driver;

pub use driver::{
    Advanced, CanonicalRunner, InspectError, SCHEDULER_INSPECT_QUERY, STATE_INSPECT_QUERY, TxFate,
    TxRecord,
};

use crate::application::Application;
use crate::batch::{Batch, Frame, WireUserOp};
use crate::history::L2TxCount;
use crate::l2_tx::{Direct, L2Tx, UserOpTx};
use crate::protocol::age_exceeds;
use alloy_primitives::{Address, Signature};
use alloy_sol_types::{Eip712Domain, SolStruct};
use std::collections::VecDeque;
use thiserror::Error;

/// Deployment constants, compiled into the template. Every field is explicit:
/// a default would let a build silently run another deployment's rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// The batch submitter: its inputs are batches, every other sender's are
    /// directs. Deployment data, never hardcoded in the protocol library.
    pub sequencer_address: Address,
    /// At least 1, which genesis and resume assert: a zero window would let
    /// the backstop drain the arriving block and queue a direct at the clock.
    pub max_wait_blocks: u64,
    /// The fridge is full at this many queued directs.
    pub fridge_bound: u64,
    /// A direct with a longer payload never joins the fridge.
    pub max_direct_payload_bytes: u64,
}

impl SchedulerConfig {
    /// The production configuration: the submitter, and the constants the
    /// application's template compiles in.
    pub fn for_application<A: Application>(sequencer_address: Address) -> Self {
        let limits = A::scheduler_limits();
        Self {
            sequencer_address,
            max_wait_blocks: limits.max_wait_blocks,
            fridge_bound: limits.fridge_bound,
            max_direct_payload_bytes: limits.max_direct_payload_bytes,
        }
    }
}

/// One InputBox input, as `EvmAdvance` delivers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L1Input {
    /// Per-application InputBox index.
    pub index: u64,
    pub sender: Address,
    /// Its inclusion block.
    pub block: u64,
    pub timestamp: u64,
    /// With `app_contract`, the EIP-712 domain a batch's ops are signed under.
    pub chain_id: u64,
    pub app_contract: Address,
    pub payload: Vec<u8>,
}

/// All that carries over between inputs, except the queued directs' bytes,
/// which L1 holds. It is the scheduler's part of the machine's export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerState {
    /// Inputs consumed: the scheduler's position in the InputBox.
    pub l1_input_count: u64,
    pub next_batch_nonce: u64,
    /// The latest block the scheduler has executed at.
    pub clock: u64,
    /// Index of the oldest queued direct; `l1_input_count` when none is queued.
    pub first_queued_input: u64,
    /// How many directs are queued, to check a re-supplied range against.
    pub fridge_len: u64,
}

/// What became of one input. The reject reason is diagnostic: every rejected
/// or stale batch has the same effect, none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    DirectQueued,
    /// Its payload is over the cap: it never executes and never counts.
    DirectDropped,
    BatchAccepted,
    BatchStale,
    BatchRejected(BatchRejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchRejectReason {
    /// Gate a.
    DecodeFailed,
    /// Gate b.
    WrongNonce { expected: u64, got: u64 },
    /// Gate d: a frame's safe block must lie strictly below the inclusion block.
    SafeBlockNotBelowInclusionBlock,
    /// Gate d.
    NonMonotonicSafeBlocks,
    /// Gate d.
    FeePriceOutOfRange,
    /// Gate f: the first frame lies below the scheduler clock.
    FirstFrameBelowClock { safe_block: u64, clock: u64 },
}

/// One handed-out transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheduled {
    /// The scheduler's next batch nonce when it handed this out: the batch
    /// whose frame holds or drained it, or the next batch to come. An accepted
    /// batch's nonce advances only after its last transaction.
    pub batch_nonce: u64,
    pub tx: L2Tx,
}

/// Why [`Scheduler::resume`] refused a state and its re-supplied inputs.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResumeError {
    #[error("first queued input {first_queued_input} is past the L1 input count {l1_input_count}")]
    FirstQueuedAfterCount {
        first_queued_input: u64,
        l1_input_count: u64,
    },
    #[error("re-supplied input out of sequence: expected index {expected}, got {got}")]
    OutOfSequence { expected: u64, got: u64 },
    #[error("re-supplied input {index} is at or past the L1 input count {l1_input_count}")]
    BeyondInputCount { index: u64, l1_input_count: u64 },
    #[error("re-supplied inputs stop at {next}, before the L1 input count {l1_input_count}")]
    MissingInputs { next: u64, l1_input_count: u64 },
    #[error(
        "re-supplied input {index} at block {block} is below the previous input's block {previous_block}"
    )]
    BlockDecreases {
        index: u64,
        block: u64,
        previous_block: u64,
    },
    #[error("first queued input {index} is not an admitted direct")]
    FirstQueuedNotADirect { index: u64 },
    #[error("queued direct {index} at block {block} is not above the clock {clock}")]
    DirectNotAboveClock { index: u64, block: u64, clock: u64 },
    #[error("the state queues {expected} directs, but the range admits {admitted}")]
    FridgeLenMismatch { expected: u64, admitted: u64 },
}

/// The machine's `scheduler` inspect report: the scheduler's state and the
/// application's count, as six little-endian `u64` in field order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerExport {
    /// InputBox inputs the machine has consumed: its position.
    pub l1_input_count: u64,
    /// L2 transactions it has executed: the base count of a recovery from it.
    pub l2_tx_count: u64,
    pub next_batch_nonce: u64,
    /// The latest block the scheduler has executed at.
    pub clock: u64,
    /// Index of the oldest queued direct; the L1 input count when none is.
    pub first_queued_input: u64,
    /// How many directs are queued.
    pub fridge_len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a scheduler export is {expected} bytes, got {got}", expected = SchedulerExport::ENCODED_LEN)]
pub struct SchedulerExportLengthError {
    pub got: usize,
}

impl SchedulerExport {
    pub const ENCODED_LEN: usize = 48;

    pub fn new(state: SchedulerState, l2_tx_count: L2TxCount) -> Self {
        Self {
            l1_input_count: state.l1_input_count,
            l2_tx_count: l2_tx_count.get(),
            next_batch_nonce: state.next_batch_nonce,
            clock: state.clock,
            first_queued_input: state.first_queued_input,
            fridge_len: state.fridge_len,
        }
    }

    /// The scheduler's part, as [`Scheduler::resume`] takes it.
    pub fn scheduler_state(&self) -> SchedulerState {
        SchedulerState {
            l1_input_count: self.l1_input_count,
            next_batch_nonce: self.next_batch_nonce,
            clock: self.clock,
            first_queued_input: self.first_queued_input,
            fridge_len: self.fridge_len,
        }
    }

    fn fields(&self) -> [u64; 6] {
        [
            self.l1_input_count,
            self.l2_tx_count,
            self.next_batch_nonce,
            self.clock,
            self.first_queued_input,
            self.fridge_len,
        ]
    }

    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut bytes = [0_u8; Self::ENCODED_LEN];
        let (chunks, _) = bytes.as_chunks_mut::<8>();
        for (chunk, field) in chunks.iter_mut().zip(self.fields()) {
            *chunk = field.to_le_bytes();
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SchedulerExportLengthError> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(SchedulerExportLengthError { got: bytes.len() });
        }
        let (chunks, _) = bytes.as_chunks::<8>();
        let mut fields = chunks.iter().map(|chunk| u64::from_le_bytes(*chunk));
        let mut next = || fields.next().expect("48 bytes hold six fields");
        Ok(Self {
            l1_input_count: next(),
            l2_tx_count: next(),
            next_batch_nonce: next(),
            clock: next(),
            first_queued_input: next(),
            fridge_len: next(),
        })
    }
}

/// The free-standing canonical scheduler. Feed it one input, then pull that
/// input's transactions with [`Scheduler::next_tx`] until it returns `None`.
///
/// `feed` decides everything eagerly (gates, drains, the clock, the nonce),
/// so [`Scheduler::state`] already describes the scheduler after the input;
/// only signature recovery waits for `next_tx`, one op at a time.
#[derive(Debug, Clone)]
pub struct Scheduler {
    config: SchedulerConfig,
    /// Admitted directs not yet handed out, oldest first.
    fridge: VecDeque<Direct>,
    l1_input_count: u64,
    next_batch_nonce: u64,
    clock: u64,
    cursor: Cursor,
}

/// The current input's transactions not yet handed out, in execution order.
#[derive(Debug, Clone, Default)]
struct Cursor {
    batch_nonce: u64,
    /// The accepted batch's signature domain; `None` for any other input.
    domain: Option<Eip712Domain>,
    steps: VecDeque<Step>,
}

#[derive(Debug, Clone)]
enum Step {
    Direct(Direct),
    /// An op of an accepted frame, still to pass the cap and signature checks.
    UserOp {
        op: WireUserOp,
        fee_price: u16,
        safe_block: u64,
    },
}

impl Scheduler {
    pub fn genesis(config: SchedulerConfig) -> Self {
        assert_window(&config);
        Self {
            config,
            fridge: VecDeque::new(),
            l1_input_count: 0,
            next_batch_nonce: 0,
            clock: 0,
            cursor: Cursor::default(),
        }
    }

    /// Resume from an exported state. `queued` re-supplies every input from
    /// `first_queued_input` up to `l1_input_count`, in order. The fridge is a
    /// queue, so re-admitting the directs among them by the same rule rebuilds
    /// it; the checks refuse a range that does not match the state.
    pub fn resume(
        config: SchedulerConfig,
        state: SchedulerState,
        queued: impl IntoIterator<Item = L1Input>,
    ) -> Result<Self, ResumeError> {
        assert_window(&config);
        if state.first_queued_input > state.l1_input_count {
            return Err(ResumeError::FirstQueuedAfterCount {
                first_queued_input: state.first_queued_input,
                l1_input_count: state.l1_input_count,
            });
        }
        let mut scheduler = Self {
            config,
            fridge: VecDeque::new(),
            l1_input_count: state.l1_input_count,
            next_batch_nonce: state.next_batch_nonce,
            clock: state.clock,
            cursor: Cursor::default(),
        };
        let mut next = state.first_queued_input;
        // InputBox order never lowers the block. A range that does would put a
        // newer direct before an older one, and drains stop at the front.
        let mut previous_block = 0;
        for input in queued {
            if input.index != next {
                return Err(ResumeError::OutOfSequence {
                    expected: next,
                    got: input.index,
                });
            }
            if input.index >= state.l1_input_count {
                return Err(ResumeError::BeyondInputCount {
                    index: input.index,
                    l1_input_count: state.l1_input_count,
                });
            }
            if input.block < previous_block {
                return Err(ResumeError::BlockDecreases {
                    index: input.index,
                    block: input.block,
                    previous_block,
                });
            }
            previous_block = input.block;
            let admitted = scheduler.admits(&input);
            if input.index == state.first_queued_input && !admitted {
                return Err(ResumeError::FirstQueuedNotADirect { index: input.index });
            }
            // Below the count, so the successor exists.
            next = input.index + 1;
            if admitted {
                if input.block <= state.clock {
                    return Err(ResumeError::DirectNotAboveClock {
                        index: input.index,
                        block: input.block,
                        clock: state.clock,
                    });
                }
                scheduler.fridge.push_back(into_direct(input));
            }
        }
        if next != state.l1_input_count {
            return Err(ResumeError::MissingInputs {
                next,
                l1_input_count: state.l1_input_count,
            });
        }
        let admitted = scheduler.fridge_len();
        if admitted != state.fridge_len {
            return Err(ResumeError::FridgeLenMismatch {
                expected: state.fridge_len,
                admitted,
            });
        }
        Ok(scheduler)
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// The scheduler once the current input's transactions are all handed out.
    pub fn state(&self) -> SchedulerState {
        SchedulerState {
            l1_input_count: self.l1_input_count,
            next_batch_nonce: self.next_batch_nonce,
            clock: self.clock,
            first_queued_input: self
                .fridge
                .front()
                .map_or(self.l1_input_count, |direct| direct.input_index),
            fridge_len: self.fridge_len(),
        }
    }

    /// Take the next input and say what became of it. Its transactions follow
    /// from [`Scheduler::next_tx`]. Panics on misuse: an index other than
    /// `l1_input_count`, or a previous input with transactions left.
    pub fn feed(&mut self, input: L1Input) -> InputOutcome {
        assert_eq!(
            input.index, self.l1_input_count,
            "inputs must arrive in InputBox order"
        );
        assert!(
            self.cursor.steps.is_empty(),
            "the previous input still has transactions to hand out"
        );
        self.l1_input_count = self
            .l1_input_count
            .checked_add(1)
            .expect("L1 input count overflow: no canonical successor");
        self.cursor = Cursor {
            batch_nonce: self.next_batch_nonce,
            domain: None,
            steps: VecDeque::new(),
        };

        let max_wait_blocks = self.config.max_wait_blocks;
        self.drain_front_while(|direct| age_exceeds(input.block, direct.block, max_wait_blocks));

        if input.sender == self.config.sequencer_address {
            self.feed_batch(&input)
        } else {
            self.feed_direct(input)
        }
    }

    /// The current input's next transaction, in execution order.
    pub fn next_tx(&mut self) -> Option<Scheduled> {
        while let Some(step) = self.cursor.steps.pop_front() {
            let tx = match step {
                Step::Direct(direct) => L2Tx::Direct(direct),
                Step::UserOp {
                    op,
                    fee_price,
                    safe_block,
                } => {
                    // Both checks drop the op without a trace, so their order
                    // is unobservable; the cheap one goes first.
                    if op.max_fee < fee_price {
                        continue;
                    }
                    let domain = self
                        .cursor
                        .domain
                        .as_ref()
                        .expect("a user op is queued only with its batch's domain");
                    let Some(sender) = recover_sender(domain, &op) else {
                        continue;
                    };
                    L2Tx::UserOp(UserOpTx {
                        sender,
                        nonce: op.nonce,
                        fee: fee_price,
                        block: safe_block,
                        data: op.data,
                    })
                }
            };
            return Some(Scheduled {
                batch_nonce: self.cursor.batch_nonce,
                tx,
            });
        }
        None
    }

    fn feed_direct(&mut self, input: L1Input) -> InputOutcome {
        // The cap comes first: a direct that never joins drains nothing.
        if !self.admits(&input) {
            return InputOutcome::DirectDropped;
        }
        // Whole blocks, oldest first, so no direct at or below the clock stays
        // queued; never the arriving block, so the bound is soft by one block.
        while self.fridge_len() >= self.config.fridge_bound {
            let Some(front_block) = self.fridge.front().map(|direct| direct.block) else {
                break;
            };
            if front_block >= input.block {
                break;
            }
            self.drain_front_while(|direct| direct.block == front_block);
        }
        self.fridge.push_back(into_direct(input));
        InputOutcome::DirectQueued
    }

    fn feed_batch(&mut self, input: &L1Input) -> InputOutcome {
        let Ok(batch) = <Batch as ssz::Decode>::from_ssz_bytes(&input.payload) else {
            return InputOutcome::BatchRejected(BatchRejectReason::DecodeFailed);
        };
        if batch.nonce != self.next_batch_nonce {
            return InputOutcome::BatchRejected(BatchRejectReason::WrongNonce {
                expected: self.next_batch_nonce,
                got: batch.nonce,
            });
        }
        let Some(first_safe_block) = batch.frames.first().map(|frame| frame.safe_block) else {
            self.next_batch_nonce = self.successor_batch_nonce();
            return InputOutcome::BatchAccepted;
        };
        if let Some(reason) = frame_structure_reject(input.block, &batch.frames) {
            return InputOutcome::BatchRejected(reason);
        }
        if age_exceeds(input.block, first_safe_block, self.config.max_wait_blocks) {
            return InputOutcome::BatchStale;
        }
        if first_safe_block < self.clock {
            return InputOutcome::BatchRejected(BatchRejectReason::FirstFrameBelowClock {
                safe_block: first_safe_block,
                clock: self.clock,
            });
        }

        // Preflight before any frame is handed out: a batch at u64::MAX has no
        // canonical successor.
        let next_nonce = self.successor_batch_nonce();
        self.cursor.domain = Some(crate::build_input_domain(
            input.chain_id,
            input.app_contract,
        ));
        for frame in batch.frames {
            let safe_block = frame.safe_block;
            self.drain_front_while(|direct| direct.block <= safe_block);
            self.raise_clock(safe_block);
            for op in frame.user_ops {
                self.cursor.steps.push_back(Step::UserOp {
                    op,
                    fee_price: frame.fee_price,
                    safe_block,
                });
            }
        }
        self.next_batch_nonce = next_nonce;
        InputOutcome::BatchAccepted
    }

    fn successor_batch_nonce(&self) -> u64 {
        self.next_batch_nonce
            .checked_add(1)
            .expect("batch nonce overflow: no canonical successor")
    }

    /// Whether a direct joins the fridge: by sender and payload length only,
    /// never by content, so no application code runs.
    fn admits(&self, input: &L1Input) -> bool {
        input.sender != self.config.sequencer_address
            && u64::try_from(input.payload.len())
                .is_ok_and(|len| len <= self.config.max_direct_payload_bytes)
    }

    /// Hand out queued directs from the front while `due` holds, raising the
    /// clock to each one's block, as every drain does.
    fn drain_front_while(&mut self, due: impl Fn(&Direct) -> bool) {
        while self.fridge.front().is_some_and(&due) {
            let direct = self.fridge.pop_front().expect("the front was just seen");
            self.raise_clock(direct.block);
            self.cursor.steps.push_back(Step::Direct(direct));
        }
    }

    fn raise_clock(&mut self, block: u64) {
        self.clock = self.clock.max(block);
    }

    fn fridge_len(&self) -> u64 {
        u64::try_from(self.fridge.len()).expect("a fridge length fits in u64")
    }
}

/// With a window of at least one block the backstop only reaches blocks below
/// the arriving input's, so it never raises the clock to a block that can
/// still queue a direct. A zero window is a build error, so it fails loud.
fn assert_window(config: &SchedulerConfig) {
    assert!(
        config.max_wait_blocks >= 1,
        "max_wait_blocks must be at least 1"
    );
}

/// Gate d, over every frame before any executes. Strict frames let a frame
/// drain every direct of its own safe block: all of them landed before its
/// batch.
fn frame_structure_reject(inclusion_block: u64, frames: &[Frame]) -> Option<BatchRejectReason> {
    let mut previous_safe_block = 0;
    for frame in frames {
        if frame.safe_block >= inclusion_block {
            return Some(BatchRejectReason::SafeBlockNotBelowInclusionBlock);
        }
        if frame.safe_block < previous_safe_block {
            return Some(BatchRejectReason::NonMonotonicSafeBlocks);
        }
        previous_safe_block = frame.safe_block;
    }
    if frames
        .iter()
        .any(|frame| frame.fee_price > crate::fee::MAX_EXPONENT)
    {
        return Some(BatchRejectReason::FeePriceOutOfRange);
    }
    None
}

/// The op's signer, or `None` when the op must be dropped: only a 65-byte
/// signature recovers, with alloy's acceptance set (see the scheduler page).
pub(crate) fn recover_sender(domain: &Eip712Domain, wire_user_op: &WireUserOp) -> Option<Address> {
    if wire_user_op.signature.len() != WireUserOp::SIGNATURE_BYTES {
        return None;
    }
    let signature = Signature::from_raw(wire_user_op.signature.as_slice()).ok()?;
    let signing_hash = wire_user_op.to_user_op().eip712_signing_hash(domain);
    signature.recover_address_from_prehash(&signing_hash).ok()
}

fn into_direct(input: L1Input) -> Direct {
    Direct {
        input_index: input.index,
        sender: input.sender,
        block: input.block,
        timestamp: input.timestamp,
        payload: input.payload,
    }
}

/// Builders shared by the scheduler's and the driver's tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::user_op::UserOp;
    use alloy_primitives::address;
    use k256::ecdsa::SigningKey;

    pub(crate) const SEQUENCER: Address = address!("0x1111111111111111111111111111111111111111");
    pub(crate) const DIRECT_SENDER: Address =
        address!("0x2222222222222222222222222222222222222222");
    pub(crate) const CHAIN_ID: u64 = 1;
    pub(crate) const APP_CONTRACT: Address = Address::ZERO;

    /// A configuration whose bound and cap stay out of the way.
    pub(crate) fn config(max_wait_blocks: u64) -> SchedulerConfig {
        SchedulerConfig {
            sequencer_address: SEQUENCER,
            max_wait_blocks,
            fridge_bound: 1_000,
            max_direct_payload_bytes: 1_000,
        }
    }

    pub(crate) fn domain() -> Eip712Domain {
        crate::build_input_domain(CHAIN_ID, APP_CONTRACT)
    }

    pub(crate) fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes((&[seed; 32]).into()).expect("signing key")
    }

    pub(crate) fn address_of(key: &SigningKey) -> Address {
        let verifying = key.verifying_key().to_encoded_point(false);
        Address::from_raw_public_key(&verifying.as_bytes()[1..])
    }

    pub(crate) fn sign_in(
        domain: &Eip712Domain,
        key: &SigningKey,
        nonce: u32,
        max_fee: u16,
        data: Vec<u8>,
    ) -> WireUserOp {
        let user_op = UserOp {
            nonce,
            max_fee,
            data: data.clone().into(),
        };
        let signature = crate::test_support::sign_user_op(key, domain, &user_op);
        WireUserOp {
            nonce,
            max_fee,
            data,
            signature: signature.as_bytes().to_vec(),
        }
    }

    pub(crate) fn signed(key: &SigningKey, nonce: u32, max_fee: u16, data: Vec<u8>) -> WireUserOp {
        sign_in(&domain(), key, nonce, max_fee, data)
    }

    pub(crate) fn frame(safe_block: u64, fee_price: u16, user_ops: Vec<WireUserOp>) -> Frame {
        Frame {
            user_ops,
            safe_block,
            fee_price,
        }
    }

    pub(crate) fn empty_frame(safe_block: u64) -> Frame {
        frame(safe_block, 0, Vec::new())
    }

    /// Inputs carry index 0 until fed; whoever feeds them assigns the index.
    pub(crate) fn direct_from(sender: Address, block: u64, payload: Vec<u8>) -> L1Input {
        L1Input {
            index: 0,
            sender,
            block,
            timestamp: block.saturating_mul(12),
            chain_id: CHAIN_ID,
            app_contract: APP_CONTRACT,
            payload,
        }
    }

    pub(crate) fn direct(block: u64, marker: u8) -> L1Input {
        direct_from(DIRECT_SENDER, block, vec![marker])
    }

    pub(crate) fn raw_batch(block: u64, payload: Vec<u8>) -> L1Input {
        direct_from(SEQUENCER, block, payload)
    }

    pub(crate) fn batch(block: u64, nonce: u64, frames: Vec<Frame>) -> L1Input {
        raw_batch(block, ssz::Encode::as_ssz_bytes(&Batch { nonce, frames }))
    }

    /// A handed-out transaction by its marker: a direct's first payload byte
    /// or an op's first data byte.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Tx {
        D(u8),
        Op(u8),
    }

    pub(crate) fn short(tx: &L2Tx) -> Tx {
        match tx {
            L2Tx::Direct(direct) => Tx::D(direct.payload.first().copied().unwrap_or(0)),
            L2Tx::UserOp(op) => Tx::Op(op.data.first().copied().unwrap_or(0)),
        }
    }

    pub(crate) fn tx_block(tx: &L2Tx) -> u64 {
        match tx {
            L2Tx::Direct(direct) => direct.block,
            L2Tx::UserOp(op) => op.block,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// Drives a scheduler and, after every input, asserts the clock's split:
    /// an admitted direct has been handed out exactly when its block is at or
    /// below the clock, and the fridge and `state()` agree. It also asserts
    /// that blocks never go backwards along the hand-out order.
    struct Harness {
        scheduler: Scheduler,
        /// Every input it knows, by index, to re-supply on a resume.
        inputs: BTreeMap<u64, L1Input>,
        /// `(index, block)` of every direct admitted since the harness began.
        admitted: Vec<(u64, u64)>,
        handed_out: BTreeSet<u64>,
        last_block: u64,
    }

    impl Harness {
        fn new(config: SchedulerConfig) -> Self {
            Self::wrap(Scheduler::genesis(config), BTreeMap::new())
        }

        fn resumed(
            config: SchedulerConfig,
            state: SchedulerState,
            queued: Vec<L1Input>,
        ) -> Result<Self, ResumeError> {
            let scheduler = Scheduler::resume(config, state, queued.clone())?;
            let inputs = queued.into_iter().map(|input| (input.index, input));
            Ok(Self::wrap(scheduler, inputs.collect()))
        }

        fn wrap(scheduler: Scheduler, inputs: BTreeMap<u64, L1Input>) -> Self {
            let admitted = scheduler
                .fridge
                .iter()
                .map(|direct| (direct.input_index, direct.block))
                .collect();
            let last_block = scheduler.clock;
            let harness = Self {
                scheduler,
                inputs,
                admitted,
                handed_out: BTreeSet::new(),
                last_block,
            };
            harness.assert_split();
            harness
        }

        fn state(&self) -> SchedulerState {
            self.scheduler.state()
        }

        fn feed(&mut self, mut input: L1Input) -> (InputOutcome, Vec<Scheduled>) {
            input.index = self.state().l1_input_count;
            let (index, block) = (input.index, input.block);
            self.inputs.insert(index, input.clone());
            let outcome = self.scheduler.feed(input);
            if outcome == InputOutcome::DirectQueued {
                self.admitted.push((index, block));
            }
            let txs: Vec<Scheduled> = std::iter::from_fn(|| self.scheduler.next_tx()).collect();
            for scheduled in &txs {
                let block = tx_block(&scheduled.tx);
                assert!(
                    block >= self.last_block,
                    "blocks went backwards: {block} after {}",
                    self.last_block
                );
                self.last_block = block;
                if let L2Tx::Direct(direct) = &scheduled.tx {
                    assert!(
                        self.handed_out.insert(direct.input_index),
                        "direct {} handed out twice",
                        direct.input_index
                    );
                }
            }
            self.assert_split();
            (outcome, txs)
        }

        /// Feed and keep only the outcome and the markers.
        fn run(&mut self, input: L1Input) -> (InputOutcome, Vec<Tx>) {
            let (outcome, txs) = self.feed(input);
            (outcome, txs.iter().map(|s| short(&s.tx)).collect())
        }

        fn assert_split(&self) {
            let state = self.state();
            for &(index, block) in &self.admitted {
                assert_eq!(
                    self.handed_out.contains(&index),
                    block <= state.clock,
                    "direct {index} at block {block} is on the wrong side of clock {}",
                    state.clock
                );
            }
            let queued: Vec<u64> = self
                .admitted
                .iter()
                .map(|&(index, _)| index)
                .filter(|index| !self.handed_out.contains(index))
                .collect();
            let fridge: Vec<u64> = self
                .scheduler
                .fridge
                .iter()
                .map(|d| d.input_index)
                .collect();
            assert_eq!(
                queued, fridge,
                "the fridge holds the admitted, unexecuted directs"
            );
            assert_eq!(state.fridge_len, fridge.len() as u64);
            assert_eq!(
                state.first_queued_input,
                fridge.first().copied().unwrap_or(state.l1_input_count)
            );
        }

        /// A scheduler resumed from this one's state and its re-supplied inputs.
        fn resume_here(&self) -> Self {
            let state = self.state();
            let queued = (state.first_queued_input..state.l1_input_count)
                .map(|index| self.inputs[&index].clone())
                .collect();
            Self::resumed(*self.scheduler.config(), state, queued)
                .expect("a scheduler's own state resumes")
        }
    }

    fn bounded(
        max_wait_blocks: u64,
        fridge_bound: u64,
        max_direct_payload_bytes: u64,
    ) -> SchedulerConfig {
        SchedulerConfig {
            fridge_bound,
            max_direct_payload_bytes,
            ..config(max_wait_blocks)
        }
    }

    fn at_state(max_wait_blocks: u64, next_batch_nonce: u64, clock: u64) -> Harness {
        let state = SchedulerState {
            l1_input_count: 0,
            next_batch_nonce,
            clock,
            first_queued_input: 0,
            fridge_len: 0,
        };
        Harness::resumed(config(max_wait_blocks), state, Vec::new()).expect("an empty resume")
    }

    fn rejected(reason: BatchRejectReason) -> InputOutcome {
        InputOutcome::BatchRejected(reason)
    }

    // ── Batches and frames ────────────────────────────────────────────────

    #[test]
    fn batch_drains_safe_directs_before_handing_out_user_ops() {
        let mut h = Harness::new(config(100));
        assert_eq!(h.run(direct(10, 1)), (InputOutcome::DirectQueued, vec![]));
        let op = signed(&key(1), 0, 1, vec![2]);
        assert_eq!(
            h.run(batch(20, 0, vec![frame(10, 0, vec![op])])),
            (InputOutcome::BatchAccepted, vec![Tx::D(1), Tx::Op(2)])
        );
        assert_eq!(h.state().fridge_len, 0);
        assert_eq!(h.state().next_batch_nonce, 1);
    }

    #[test]
    fn handed_out_ops_carry_the_frame_and_the_recovered_sender() {
        let mut h = Harness::new(config(100));
        let signer = key(7);
        let op = signed(&signer, 4, 9, vec![0xAB, 0xCD]);
        let (_, txs) = h.feed(batch(20, 0, vec![frame(15, 6, vec![op])]));
        assert_eq!(
            txs,
            [Scheduled {
                batch_nonce: 0,
                tx: L2Tx::UserOp(UserOpTx {
                    sender: address_of(&signer),
                    nonce: 4,
                    fee: 6,
                    block: 15,
                    data: vec![0xAB, 0xCD],
                }),
            }]
        );
    }

    #[test]
    fn handed_out_directs_carry_their_input_context() {
        let mut h = Harness::new(config(100));
        h.feed(batch(5, 0, vec![]));
        h.feed(direct_from(DIRECT_SENDER, 10, vec![3, 4]));
        let (_, txs) = h.feed(batch(11, 1, vec![empty_frame(10)]));
        assert_eq!(
            txs,
            [Scheduled {
                batch_nonce: 1,
                tx: L2Tx::Direct(Direct {
                    input_index: 1,
                    sender: DIRECT_SENDER,
                    block: 10,
                    timestamp: 120,
                    payload: vec![3, 4],
                }),
            }]
        );
    }

    #[test]
    fn pre_batch_backstop_hands_out_overdue_directs_before_user_ops() {
        let mut h = Harness::new(config(5));
        h.feed(direct(1, 1));
        let op = signed(&key(2), 0, 1, vec![2]);
        assert_eq!(
            h.run(batch(6, 0, vec![frame(2, 0, vec![op])])),
            (InputOutcome::BatchAccepted, vec![Tx::D(1), Tx::Op(2)])
        );
    }

    #[test]
    fn stale_batch_is_skipped_without_consuming_the_nonce() {
        let mut h = Harness::new(config(5));
        h.feed(direct(1, 9));
        let stale = batch(
            10,
            0,
            vec![frame(4, 0, vec![signed(&key(3), 0, 1, vec![7])])],
        );
        // The backstop still runs: 10 - 1 >= 5.
        assert_eq!(h.run(stale), (InputOutcome::BatchStale, vec![Tx::D(9)]));
        assert_eq!(h.state().next_batch_nonce, 0);

        let fresh = batch(
            10,
            0,
            vec![frame(9, 0, vec![signed(&key(13), 0, 1, vec![8])])],
        );
        assert_eq!(h.run(fresh), (InputOutcome::BatchAccepted, vec![Tx::Op(8)]));
        assert_eq!(h.state().next_batch_nonce, 1);
    }

    #[test]
    fn fee_bounds_reject_the_whole_batch_and_preserve_the_backstop() {
        use crate::fee::MAX_EXPONENT;
        for fee in [MAX_EXPONENT + 1, u16::MAX] {
            for invalid_position in [0, 1] {
                let mut h = Harness::new(config(10));
                h.feed(direct(1, 1));
                h.feed(direct(8, 2));
                let op = signed(&key(1), 0, u16::MAX, vec![3]);
                let mut frames = vec![frame(8, 0, vec![op.clone()]), frame(8, 0, vec![op])];
                frames[invalid_position].fee_price = fee;
                // Only the overdue direct: no frame drain, no op.
                assert_eq!(
                    h.run(batch(11, 0, frames)),
                    (
                        rejected(BatchRejectReason::FeePriceOutOfRange),
                        vec![Tx::D(1)]
                    )
                );
                assert_eq!(h.state().fridge_len, 1);
                assert_eq!(h.state().next_batch_nonce, 0);

                assert_eq!(
                    h.run(batch(11, 0, vec![frame(8, MAX_EXPONENT, vec![])])),
                    (InputOutcome::BatchAccepted, vec![Tx::D(2)])
                );
                assert_eq!(h.state().next_batch_nonce, 1);
            }
        }
    }

    #[test]
    fn non_monotonic_safe_blocks_invalidate_the_batch() {
        let mut h = Harness::new(config(100));
        let frames = vec![
            frame(8, 0, vec![signed(&key(4), 0, 1, vec![1])]),
            frame(7, 0, vec![signed(&key(5), 0, 1, vec![2])]),
        ];
        assert_eq!(
            h.run(batch(10, 0, frames)),
            (rejected(BatchRejectReason::NonMonotonicSafeBlocks), vec![])
        );
        assert_eq!(h.state().next_batch_nonce, 0);
    }

    #[test]
    fn a_first_frame_not_below_the_inclusion_block_invalidates_the_batch() {
        for safe_block in [11, 10] {
            let mut h = Harness::new(config(100));
            let op = signed(&key(6), 0, 1, vec![9]);
            assert_eq!(
                h.run(batch(10, 0, vec![frame(safe_block, 0, vec![op])])),
                (
                    rejected(BatchRejectReason::SafeBlockNotBelowInclusionBlock),
                    vec![]
                ),
                "safe block {safe_block} at inclusion 10"
            );
            assert_eq!(h.state().next_batch_nonce, 0);
        }
    }

    #[test]
    fn a_tail_frame_not_below_the_inclusion_block_invalidates_the_batch() {
        for safe_block in [11, 10] {
            let mut h = Harness::new(config(100));
            h.feed(direct(5, 1));
            assert_eq!(
                h.run(batch(10, 0, vec![empty_frame(5), empty_frame(safe_block)])),
                (
                    rejected(BatchRejectReason::SafeBlockNotBelowInclusionBlock),
                    vec![]
                ),
                "tail safe block {safe_block} at inclusion 10: nothing drains"
            );
            assert_eq!(h.state().next_batch_nonce, 0);
            assert_eq!(h.state().fridge_len, 1);
        }
    }

    #[test]
    fn frame_drain_includes_its_safe_block_and_nothing_after() {
        let mut h = Harness::new(config(100));
        h.feed(direct(10, 1));
        h.feed(direct(11, 2));
        assert_eq!(
            h.run(batch(12, 0, vec![empty_frame(10)])),
            (InputOutcome::BatchAccepted, vec![Tx::D(1)])
        );
        assert_eq!(h.state().fridge_len, 1);
    }

    #[test]
    fn decode_failure_invalidates_the_batch_and_keeps_running() {
        let mut h = Harness::new(config(100));
        assert_eq!(
            h.run(raw_batch(10, vec![0xFF, 0xEE, 0xDD])),
            (rejected(BatchRejectReason::DecodeFailed), vec![])
        );
        assert_eq!(h.state().next_batch_nonce, 0);
        assert_eq!(h.run(direct(11, 3)), (InputOutcome::DirectQueued, vec![]));
        assert_eq!(h.state().fridge_len, 1);
    }

    #[test]
    fn wrong_batch_nonce_is_rejected_without_consuming_the_nonce() {
        let mut h = Harness::new(config(100));
        assert_eq!(
            h.run(batch(2, 1, vec![empty_frame(1)])),
            (
                rejected(BatchRejectReason::WrongNonce {
                    expected: 0,
                    got: 1
                }),
                vec![]
            )
        );
        assert_eq!(h.state().next_batch_nonce, 0);
    }

    #[test]
    fn empty_batches_are_accepted_at_any_age_and_hand_out_nothing() {
        let mut h = Harness::new(config(100));
        h.feed(direct(10, 1));
        assert_eq!(
            h.run(batch(10, 0, vec![])),
            (InputOutcome::BatchAccepted, vec![])
        );
        assert_eq!(h.state().next_batch_nonce, 1);
        assert_eq!(h.state().clock, 0, "an empty batch raises nothing");
        // Far past the window: the backstop drains, and the batch is still accepted.
        assert_eq!(
            h.run(batch(10_000, 1, vec![])),
            (InputOutcome::BatchAccepted, vec![Tx::D(1)])
        );
        assert_eq!(h.state().next_batch_nonce, 2);
    }

    #[test]
    fn backstop_drains_all_overdue_directs() {
        let mut h = Harness::new(config(5));
        h.feed(direct(1, 1));
        h.feed(direct(2, 2));
        assert_eq!(
            h.run(direct(8, 3)),
            (InputOutcome::DirectQueued, vec![Tx::D(1), Tx::D(2)])
        );
        assert_eq!(h.state().fridge_len, 1);
        assert_eq!(h.state().clock, 2);
    }

    #[test]
    fn backstop_runs_before_a_rejected_or_stale_batch() {
        let wrong_nonce = batch(8, 9, vec![empty_frame(7)]);
        let stale = batch(8, 0, vec![empty_frame(1)]);
        for (label, input) in [("wrong nonce", wrong_nonce), ("stale", stale)] {
            let mut h = Harness::new(config(5));
            h.feed(direct(1, 1));
            let (outcome, txs) = h.run(input);
            assert!(
                matches!(
                    outcome,
                    InputOutcome::BatchRejected(_) | InputOutcome::BatchStale
                ),
                "{label}: {outcome:?}"
            );
            assert_eq!(
                txs,
                [Tx::D(1)],
                "{label}: the overdue direct still executes"
            );
            assert_eq!(h.state().next_batch_nonce, 0, "{label}");
            assert_eq!(h.state().clock, 1, "{label}: the backstop raises the clock");
        }
    }

    #[test]
    fn invalid_signatures_are_not_handed_out() {
        let mut h = Harness::new(config(100));
        let good = signed(&key(8), 0, 0, vec![3]);
        let zero = WireUserOp {
            signature: vec![0; WireUserOp::SIGNATURE_BYTES],
            data: vec![1],
            ..good.clone()
        };
        let short_signature = WireUserOp {
            signature: good.signature[..64].to_vec(),
            data: vec![2],
            ..good.clone()
        };
        assert_eq!(
            h.run(batch(
                2,
                0,
                vec![frame(1, 0, vec![zero, short_signature, good])]
            )),
            (InputOutcome::BatchAccepted, vec![Tx::Op(3)])
        );
        assert_eq!(h.state().next_batch_nonce, 1);
    }

    #[test]
    fn a_fee_cap_below_the_price_is_not_handed_out() {
        let mut h = Harness::new(config(100));
        let signer = key(9);
        let frames = vec![
            frame(
                1,
                5,
                vec![
                    signed(&signer, 0, 4, vec![1]),
                    signed(&signer, 0, 5, vec![2]),
                ],
            ),
            frame(1, 0, vec![signed(&signer, 0, 0, vec![3])]),
        ];
        // The nonce is not the scheduler's check: all three carry nonce 0.
        assert_eq!(
            h.run(batch(2, 0, frames)),
            (InputOutcome::BatchAccepted, vec![Tx::Op(2), Tx::Op(3)])
        );
    }

    #[test]
    fn batch_uses_the_input_domain_for_signature_recovery() {
        let mut h = Harness::new(config(100));
        let signer = key(10);
        let app_contract = alloy_primitives::address!("0x3333333333333333333333333333333333333333");
        let batch_domain = crate::build_input_domain(CHAIN_ID + 7, app_contract);
        let op = sign_in(&batch_domain, &signer, 0, 1, vec![9]);
        let mut input = batch(2, 0, vec![frame(1, 0, vec![op.clone()])]);
        input.chain_id = CHAIN_ID + 7;
        input.app_contract = app_contract;
        let (outcome, txs) = h.feed(input);
        assert_eq!(outcome, InputOutcome::BatchAccepted);
        let [
            Scheduled {
                tx: L2Tx::UserOp(tx),
                ..
            },
        ] = txs.as_slice()
        else {
            panic!("one op: {txs:?}")
        };
        assert_eq!(tx.sender, address_of(&signer));

        // The default domain recovers some other address from the same op.
        let (_, txs) = h.feed(batch(3, 1, vec![frame(2, 0, vec![op])]));
        let [
            Scheduled {
                tx: L2Tx::UserOp(tx),
                ..
            },
        ] = txs.as_slice()
        else {
            panic!("one op: {txs:?}")
        };
        assert_ne!(tx.sender, address_of(&signer));
    }

    #[test]
    fn batch_nonce_overflow_panics_before_any_frame_is_handed_out() {
        let full = batch(
            2,
            u64::MAX,
            vec![frame(1, 0, vec![signed(&key(18), 0, 1, vec![7])])],
        );
        let empty = batch(2, u64::MAX, vec![]);
        for input in [full, empty] {
            let mut h = at_state(100, u64::MAX, 0);
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                h.scheduler.feed(input);
            }))
            .expect_err("nonce exhaustion must fail loud");
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());
            assert_eq!(
                message,
                Some("batch nonce overflow: no canonical successor")
            );
            assert_eq!(
                h.scheduler.next_tx(),
                None,
                "the preflight runs before the frame's op is queued"
            );
            assert_eq!(h.state().next_batch_nonce, u64::MAX);
        }
    }

    // ── The clock and gate f ──────────────────────────────────────────────

    #[test]
    fn gate_f_rejects_a_first_frame_below_the_clock_and_accepts_at_it() {
        let mut h = Harness::new(config(100));
        h.feed(batch(12, 0, vec![empty_frame(10)]));
        assert_eq!(h.state().clock, 10);
        h.feed(direct(13, 1));
        assert_eq!(
            h.run(batch(
                14,
                1,
                vec![frame(9, 0, vec![signed(&key(1), 0, 0, vec![2])])]
            )),
            (
                rejected(BatchRejectReason::FirstFrameBelowClock {
                    safe_block: 9,
                    clock: 10
                }),
                vec![]
            )
        );
        assert_eq!(h.state().next_batch_nonce, 1);
        assert_eq!(h.state().fridge_len, 1);

        assert_eq!(
            h.run(batch(
                14,
                1,
                vec![
                    frame(10, 0, vec![signed(&key(1), 0, 0, vec![2])]),
                    empty_frame(13)
                ]
            )),
            (InputOutcome::BatchAccepted, vec![Tx::Op(2), Tx::D(1)])
        );
        assert_eq!(h.state().clock, 13);
    }

    /// A batch that fails several gates gets the first one's outcome. Stale
    /// versus rejected is what the reference reports, so the order is pinned.
    #[test]
    fn the_first_failing_gate_decides_the_outcome() {
        let wrong_nonce = rejected(BatchRejectReason::WrongNonce {
            expected: 0,
            got: 1,
        });
        let out_of_range = Frame {
            fee_price: crate::fee::MAX_EXPONENT + 1,
            ..empty_frame(4)
        };
        // (label, clock, input, outcome), at max_wait 5.
        let cases = [
            ("b before c", 0, batch(20, 1, vec![]), wrong_nonce),
            (
                "b before d",
                0,
                batch(12, 1, vec![empty_frame(12)]),
                wrong_nonce,
            ),
            (
                "b before e",
                0,
                batch(20, 1, vec![empty_frame(4)]),
                wrong_nonce,
            ),
            (
                "b before f",
                10,
                batch(12, 1, vec![empty_frame(9)]),
                wrong_nonce,
            ),
            (
                "d before e: a tail frame at inclusion",
                0,
                batch(20, 0, vec![empty_frame(4), empty_frame(20)]),
                rejected(BatchRejectReason::SafeBlockNotBelowInclusionBlock),
            ),
            (
                "d before e: decreasing frames",
                0,
                batch(20, 0, vec![empty_frame(4), empty_frame(3)]),
                rejected(BatchRejectReason::NonMonotonicSafeBlocks),
            ),
            (
                "d before e: fee out of range",
                0,
                batch(20, 0, vec![out_of_range]),
                rejected(BatchRejectReason::FeePriceOutOfRange),
            ),
            (
                "d before f",
                10,
                batch(12, 0, vec![empty_frame(9), empty_frame(8)]),
                rejected(BatchRejectReason::NonMonotonicSafeBlocks),
            ),
            (
                "e before f",
                10,
                batch(20, 0, vec![empty_frame(4)]),
                InputOutcome::BatchStale,
            ),
        ];
        for (label, clock, input, outcome) in cases {
            let mut h = at_state(5, 0, clock);
            assert_eq!(h.run(input), (outcome, vec![]), "{label}");
            assert_eq!(h.state().next_batch_nonce, 0, "{label}");
        }
    }

    #[test]
    fn the_clock_rises_with_frames_and_every_drain() {
        let mut h = Harness::new(bounded(5, 100, 100));
        // A frame raises it, with or without directs.
        h.feed(batch(12, 0, vec![empty_frame(10)]));
        assert_eq!(h.state().clock, 10);
        // A backstop drain raises it to the drained direct's block; the bound
        // is out of reach, so no overflow drain could have done it.
        h.feed(direct(20, 1));
        assert_eq!(
            h.run(direct(25, 2)),
            (InputOutcome::DirectQueued, vec![Tx::D(1)])
        );
        assert_eq!(h.state().clock, 20);
        // An overflow drain raises it to the drained block: bound 1, and block
        // 25 is one block old, far inside the window.
        h.scheduler.config.fridge_bound = 1;
        assert_eq!(
            h.run(direct(26, 3)),
            (InputOutcome::DirectQueued, vec![Tx::D(2)])
        );
        assert_eq!(h.state().clock, 25);
        // A later frame raises it past the drained block.
        h.feed(batch(28, 1, vec![empty_frame(27)]));
        assert_eq!(h.state().clock, 27);
    }

    // ── The fridge ────────────────────────────────────────────────────────

    #[test]
    fn the_payload_cap_drops_a_direct_that_then_drains_nothing() {
        let mut h = Harness::new(bounded(100, 2, 2));
        assert_eq!(
            h.run(direct_from(DIRECT_SENDER, 1, vec![1, 1])),
            (InputOutcome::DirectQueued, vec![])
        );
        assert_eq!(h.run(direct(2, 2)), (InputOutcome::DirectQueued, vec![]));
        let full = h.state();
        assert_eq!(full.fridge_len, 2, "the fridge is full");

        assert_eq!(
            h.run(direct_from(DIRECT_SENDER, 3, vec![3, 3, 3])),
            (InputOutcome::DirectDropped, vec![])
        );
        assert_eq!(
            h.state(),
            SchedulerState {
                l1_input_count: full.l1_input_count + 1,
                ..full
            },
            "a dropped direct counts as an input and changes nothing else"
        );
        // An empty payload is within any cap.
        assert_eq!(
            h.run(direct_from(DIRECT_SENDER, 4, vec![])),
            (InputOutcome::DirectQueued, vec![Tx::D(1)])
        );
    }

    #[test]
    fn overflow_drains_whole_blocks_oldest_first() {
        let mut h = Harness::new(bounded(100, 3, 100));
        h.feed(direct(1, 1));
        h.feed(direct(1, 2));
        h.feed(direct(2, 3));
        // One entry would make room, but block 1 leaves whole.
        assert_eq!(
            h.run(direct(3, 4)),
            (InputOutcome::DirectQueued, vec![Tx::D(1), Tx::D(2)])
        );
        assert_eq!(h.state().clock, 1);
        assert_eq!(h.state().fridge_len, 2);
        assert_eq!(h.state().first_queued_input, 2);
    }

    #[test]
    fn overflow_never_drains_the_arriving_block() {
        let mut h = Harness::new(bounded(100, 2, 100));
        h.feed(direct(5, 1));
        h.feed(direct(5, 2));
        // Only the arriving block remains: the direct joins over the bound.
        assert_eq!(h.run(direct(5, 3)), (InputOutcome::DirectQueued, vec![]));
        assert_eq!(h.state().fridge_len, 3);
        assert_eq!(h.state().clock, 0);
        // The next block drains all of block 5 at once.
        assert_eq!(
            h.run(direct(6, 4)),
            (
                InputOutcome::DirectQueued,
                vec![Tx::D(1), Tx::D(2), Tx::D(3)]
            )
        );
        assert_eq!(h.state().fridge_len, 1);
        assert_eq!(h.state().clock, 5);
    }

    #[test]
    fn a_zero_bound_drains_every_older_block_on_arrival() {
        let mut h = Harness::new(bounded(100, 0, 100));
        assert_eq!(h.run(direct(1, 1)), (InputOutcome::DirectQueued, vec![]));
        assert_eq!(h.run(direct(1, 2)), (InputOutcome::DirectQueued, vec![]));
        assert_eq!(
            h.run(direct(2, 3)),
            (InputOutcome::DirectQueued, vec![Tx::D(1), Tx::D(2)])
        );
    }

    /// A batch sealed before an overflow it could not predict: accepted with
    /// the hand-out order the sequencer predicted when its first frame is at or
    /// above the drained block, rejected by gate f when below.
    #[test]
    fn an_overflow_keeps_the_order_of_a_later_batch_or_gets_it_rejected() {
        let directs = [direct(1, 1), direct(2, 2), direct(3, 3), direct(4, 4)];
        let run_all = |fridge_bound: u64, safe_block: u64| {
            let mut h = Harness::new(bounded(100, fridge_bound, 100));
            let mut stream = Vec::new();
            for input in directs.iter().cloned() {
                stream.extend(h.feed(input).1);
            }
            let (outcome, txs) = h.feed(batch(
                6,
                0,
                vec![frame(safe_block, 0, vec![signed(&key(3), 0, 0, vec![9])])],
            ));
            stream.extend(txs);
            (outcome, stream)
        };

        // Bound 3: d@4 drains block 1 alone, clock 1.
        let (outcome, overflowed) = run_all(3, 2);
        assert_eq!(outcome, InputOutcome::BatchAccepted);
        let (_, unbounded) = run_all(100, 2);
        assert_eq!(
            overflowed, unbounded,
            "the same transactions, the same labels"
        );
        assert_eq!(
            overflowed.iter().map(|s| short(&s.tx)).collect::<Vec<_>>(),
            [Tx::D(1), Tx::D(2), Tx::Op(9)]
        );

        // Bound 2: d@3 drains block 1 and d@4 drains block 2, clock 2.
        let (outcome, _) = run_all(2, 1);
        assert_eq!(
            outcome,
            rejected(BatchRejectReason::FirstFrameBelowClock {
                safe_block: 1,
                clock: 2
            })
        );
        assert_eq!(run_all(100, 1).0, InputOutcome::BatchAccepted);
    }

    #[test]
    fn batch_nonce_labels_name_the_nonce_at_hand_out() {
        let mut h = at_state(5, 3, 0);
        h.scheduler.config.fridge_bound = 2;
        let labels = |txs: Vec<Scheduled>| -> Vec<(Tx, u64)> {
            txs.iter().map(|s| (short(&s.tx), s.batch_nonce)).collect()
        };
        h.feed(direct(10, 1));
        h.feed(direct(11, 2));
        // Overflow drain on a direct: the next batch's nonce.
        assert_eq!(labels(h.feed(direct(12, 3)).1), [(Tx::D(1), 3)]);
        // Frame drain and op: the accepted batch's own nonce.
        let op = signed(&key(4), 0, 0, vec![7]);
        assert_eq!(
            labels(h.feed(batch(13, 3, vec![frame(11, 0, vec![op])])).1),
            [(Tx::D(2), 3), (Tx::Op(7), 3)]
        );
        assert_eq!(h.state().next_batch_nonce, 4);
        // Backstop drain on a direct: the next batch's nonce.
        assert_eq!(labels(h.feed(direct(20, 4)).1), [(Tx::D(3), 4)]);
        // Backstop drain on an accepted batch: that batch's nonce.
        assert_eq!(
            labels(h.feed(batch(26, 4, vec![empty_frame(22)])).1),
            [(Tx::D(4), 4)]
        );
        // Backstop drain on a rejected batch: the nonce the next batch needs.
        h.feed(direct(30, 5));
        assert_eq!(labels(h.feed(batch(36, 9, vec![])).1), [(Tx::D(5), 5)]);
        assert_eq!(h.state().next_batch_nonce, 5);
    }

    // ── State and resume ──────────────────────────────────────────────────

    #[test]
    fn state_names_the_first_queued_input() {
        let mut h = Harness::new(config(100));
        let genesis = SchedulerState {
            l1_input_count: 0,
            next_batch_nonce: 0,
            clock: 0,
            first_queued_input: 0,
            fridge_len: 0,
        };
        assert_eq!(h.state(), genesis);
        h.feed(batch(3, 0, vec![empty_frame(2)]));
        h.feed(direct(4, 1));
        h.feed(direct(5, 2));
        assert_eq!(
            h.state(),
            SchedulerState {
                l1_input_count: 3,
                next_batch_nonce: 1,
                clock: 2,
                first_queued_input: 1,
                fridge_len: 2,
            }
        );
        h.feed(batch(6, 1, vec![empty_frame(5)]));
        assert_eq!(
            h.state(),
            SchedulerState {
                l1_input_count: 4,
                next_batch_nonce: 2,
                clock: 5,
                first_queued_input: 4,
                fridge_len: 0,
            },
            "with nothing queued, the first queued input is the input count"
        );
    }

    #[test]
    #[should_panic(expected = "inputs must arrive in InputBox order")]
    fn feed_panics_on_an_index_gap() {
        let mut scheduler = Scheduler::genesis(config(100));
        let mut input = direct(1, 1);
        input.index = 1;
        scheduler.feed(input);
    }

    #[test]
    #[should_panic(expected = "the previous input still has transactions to hand out")]
    fn feed_panics_while_transactions_are_left() {
        let mut scheduler = Scheduler::genesis(config(100));
        scheduler.feed(direct(1, 1));
        let mut input = batch(3, 0, vec![empty_frame(2)]);
        input.index = 1;
        scheduler.feed(input);
        let mut next = direct(4, 2);
        next.index = 2;
        scheduler.feed(next);
    }

    /// With a zero window the backstop would drain the arriving block, raise
    /// the clock to it, and then queue a same-block direct at the clock.
    #[test]
    fn a_zero_window_is_refused_at_genesis_and_resume() {
        let genesis = std::panic::catch_unwind(|| Scheduler::genesis(config(0)));
        let resume = std::panic::catch_unwind(|| {
            let state = Scheduler::genesis(config(1)).state();
            Scheduler::resume(config(0), state, Vec::new())
        });
        for (label, result) in [
            ("genesis", genesis.map(|_| ())),
            ("resume", resume.map(|_| ())),
        ] {
            let panic = result.expect_err(label);
            assert_eq!(
                panic.downcast_ref::<&str>().copied(),
                Some("max_wait_blocks must be at least 1"),
                "{label}"
            );
        }
        // One block is enough: the arriving block never drains.
        let mut h = Harness::new(config(1));
        h.feed(direct(10, 1));
        assert_eq!(h.run(direct(10, 2)), (InputOutcome::DirectQueued, vec![]));
        assert_eq!(
            h.run(direct(11, 3)),
            (InputOutcome::DirectQueued, vec![Tx::D(1), Tx::D(2)])
        );
        h.resume_here();
    }

    #[test]
    fn resume_continues_the_batch_nonce() {
        let mut h = at_state(100, 5, 0);
        for (block, nonce) in [(100, 5), (110, 6), (120, 7)] {
            assert_eq!(
                h.run(batch(block, nonce, vec![])).0,
                InputOutcome::BatchAccepted
            );
        }
        assert_eq!(h.state().next_batch_nonce, 8);
    }

    #[test]
    fn resumed_queued_directs_drain_on_the_first_covering_frame() {
        let mut first = direct(10, 0xAA);
        first.index = 3;
        let mut gap = batch(15, 0, vec![]);
        gap.index = 4;
        let mut second = direct(20, 0xBB);
        second.index = 5;
        let state = SchedulerState {
            l1_input_count: 6,
            next_batch_nonce: 2,
            clock: 9,
            first_queued_input: 3,
            fridge_len: 2,
        };
        let mut h = Harness::resumed(config(100), state, vec![first, gap, second]).expect("resume");
        assert_eq!(h.state(), state);
        assert_eq!(
            h.run(batch(30, 2, vec![empty_frame(20)])),
            (InputOutcome::BatchAccepted, vec![Tx::D(0xAA), Tx::D(0xBB)])
        );
        assert_eq!(h.state().next_batch_nonce, 3);
    }

    #[test]
    fn resume_refuses_a_range_that_does_not_match_the_state() {
        let indexed = |mut input: L1Input, index: u64| {
            input.index = index;
            input
        };
        // Inputs 2..5: a direct, a batch, a dropped direct (cap 2), a direct.
        let range = || {
            vec![
                indexed(direct(10, 1), 2),
                indexed(batch(11, 0, vec![]), 3),
                indexed(direct_from(DIRECT_SENDER, 12, vec![0; 3]), 4),
                indexed(direct(13, 2), 5),
            ]
        };
        let state = SchedulerState {
            l1_input_count: 6,
            next_batch_nonce: 0,
            clock: 9,
            first_queued_input: 2,
            fridge_len: 2,
        };
        let cfg = bounded(100, 10, 2);
        let resume = |state: SchedulerState, queued: Vec<L1Input>| {
            Scheduler::resume(cfg, state, queued).map(|s| s.state())
        };
        assert_eq!(resume(state, range()), Ok(state));

        assert_eq!(
            resume(
                SchedulerState {
                    first_queued_input: 7,
                    ..state
                },
                Vec::new()
            ),
            Err(ResumeError::FirstQueuedAfterCount {
                first_queued_input: 7,
                l1_input_count: 6
            })
        );
        let mut gapped = range();
        gapped.remove(1);
        assert_eq!(
            resume(state, gapped),
            Err(ResumeError::OutOfSequence {
                expected: 3,
                got: 4
            })
        );
        let mut long = range();
        long.push(indexed(direct(14, 3), 6));
        assert_eq!(
            resume(state, long),
            Err(ResumeError::BeyondInputCount {
                index: 6,
                l1_input_count: 6
            })
        );
        let mut short_range = range();
        short_range.pop();
        assert_eq!(
            resume(state, short_range),
            Err(ResumeError::MissingInputs {
                next: 5,
                l1_input_count: 6
            })
        );
        assert_eq!(
            resume(
                SchedulerState {
                    first_queued_input: 3,
                    fridge_len: 1,
                    ..state
                },
                range().split_off(1)
            ),
            Err(ResumeError::FirstQueuedNotADirect { index: 3 })
        );
        assert_eq!(
            resume(
                SchedulerState {
                    first_queued_input: 4,
                    fridge_len: 1,
                    ..state
                },
                range().split_off(2)
            ),
            Err(ResumeError::FirstQueuedNotADirect { index: 4 }),
            "a direct over the cap is not admitted"
        );
        // Nothing queued, yet the state claims input 5: only an admitted
        // direct may be the first queued input, so `state()` round-trips.
        assert_eq!(
            resume(
                SchedulerState {
                    first_queued_input: 5,
                    fridge_len: 0,
                    ..state
                },
                vec![indexed(batch(13, 0, vec![]), 5)]
            ),
            Err(ResumeError::FirstQueuedNotADirect { index: 5 })
        );
        let mut lowered = range();
        lowered[1].block = 9;
        assert_eq!(
            resume(state, lowered),
            Err(ResumeError::BlockDecreases {
                index: 3,
                block: 9,
                previous_block: 10
            }),
            "any input, admitted or not, keeps the InputBox's block order"
        );
        // Admitted directs out of block order: drains stop at the newer front.
        assert_eq!(
            resume(
                SchedulerState {
                    l1_input_count: 2,
                    first_queued_input: 0,
                    ..state
                },
                vec![indexed(direct(20, 1), 0), indexed(direct(15, 2), 1)]
            ),
            Err(ResumeError::BlockDecreases {
                index: 1,
                block: 15,
                previous_block: 20
            })
        );
        assert_eq!(
            resume(SchedulerState { clock: 10, ..state }, range()),
            Err(ResumeError::DirectNotAboveClock {
                index: 2,
                block: 10,
                clock: 10
            })
        );
        assert_eq!(
            resume(
                SchedulerState {
                    fridge_len: 3,
                    ..state
                },
                range()
            ),
            Err(ResumeError::FridgeLenMismatch {
                expected: 3,
                admitted: 2
            })
        );
        assert_eq!(
            resume(
                SchedulerState {
                    first_queued_input: 6,
                    ..state
                },
                Vec::new()
            ),
            Err(ResumeError::FridgeLenMismatch {
                expected: 2,
                admitted: 0
            })
        );
    }

    /// A long scenario touching every rule: overflow, the cap, gate f, the
    /// backstop, staleness, a soft bound, a decode failure, a wrong nonce.
    fn mixed_scenario() -> (SchedulerConfig, Vec<L1Input>) {
        let (k1, k2) = (key(21), key(22));
        let bad = WireUserOp {
            signature: vec![0; WireUserOp::SIGNATURE_BYTES],
            ..signed(&k2, 5, 0, vec![99])
        };
        let inputs = vec![
            direct(10, 1),
            direct(10, 2),
            direct_from(DIRECT_SENDER, 11, vec![3, 0, 0]),
            batch(
                12,
                0,
                vec![
                    frame(10, 1, vec![signed(&k1, 0, 2, vec![10])]),
                    frame(11, 0, vec![signed(&k2, 0, 0, vec![11]), bad]),
                ],
            ),
            direct(12, 4),
            direct(13, 5),
            direct(13, 6),
            direct(14, 7),
            batch(15, 1, vec![empty_frame(11)]),
            batch(15, 1, vec![frame(13, 0, vec![signed(&k1, 1, 0, vec![12])])]),
            raw_batch(15, vec![0xFF]),
            direct(16, 8),
            batch(16, 2, vec![]),
            direct(20, 9),
            direct(23, 10),
            batch(30, 3, vec![empty_frame(21)]),
            batch(31, 3, vec![frame(26, 0, vec![signed(&k2, 1, 0, vec![13])])]),
            direct(32, 11),
            batch(33, 9, vec![empty_frame(32)]),
            direct(33, 12),
            batch(
                34,
                4,
                vec![
                    frame(32, 0, vec![signed(&k1, 2, 0, vec![14])]),
                    empty_frame(33),
                ],
            ),
            direct(35, 13),
            direct(35, 14),
            direct(35, 15),
            direct(35, 16),
            direct(36, 17),
            batch(37, 5, vec![empty_frame(36)]),
        ];
        (bounded(6, 3, 2), inputs)
    }

    #[test]
    fn the_mixed_scenario_hands_out_what_the_rules_say() {
        let (cfg, inputs) = mixed_scenario();
        let mut h = Harness::new(cfg);
        let runs: Vec<(InputOutcome, Vec<Tx>)> = inputs.into_iter().map(|i| h.run(i)).collect();
        use InputOutcome::*;
        use Tx::*;
        let expected: Vec<(InputOutcome, Vec<Tx>)> = vec![
            (DirectQueued, vec![]),
            (DirectQueued, vec![]),
            (DirectDropped, vec![]),
            (BatchAccepted, vec![D(1), D(2), Op(10), Op(11)]),
            (DirectQueued, vec![]),
            (DirectQueued, vec![]),
            (DirectQueued, vec![]),
            (DirectQueued, vec![D(4)]),
            (
                rejected(BatchRejectReason::FirstFrameBelowClock {
                    safe_block: 11,
                    clock: 12,
                }),
                vec![],
            ),
            (BatchAccepted, vec![D(5), D(6), Op(12)]),
            (rejected(BatchRejectReason::DecodeFailed), vec![]),
            (DirectQueued, vec![]),
            (BatchAccepted, vec![]),
            (DirectQueued, vec![D(7)]),
            (DirectQueued, vec![D(8)]),
            (BatchStale, vec![D(9), D(10)]),
            (BatchAccepted, vec![Op(13)]),
            (DirectQueued, vec![]),
            (
                rejected(BatchRejectReason::WrongNonce {
                    expected: 4,
                    got: 9,
                }),
                vec![],
            ),
            (DirectQueued, vec![]),
            (BatchAccepted, vec![D(11), Op(14), D(12)]),
            (DirectQueued, vec![]),
            (DirectQueued, vec![]),
            (DirectQueued, vec![]),
            (DirectQueued, vec![]),
            (DirectQueued, vec![D(13), D(14), D(15), D(16)]),
            (BatchAccepted, vec![D(17)]),
        ];
        assert_eq!(runs, expected);
        assert_eq!(
            h.state(),
            SchedulerState {
                l1_input_count: 27,
                next_batch_nonce: 6,
                clock: 36,
                first_queued_input: 27,
                fridge_len: 0,
            }
        );
    }

    #[test]
    fn a_resumed_scheduler_hands_out_what_the_original_does_at_every_point() {
        let (cfg, inputs) = mixed_scenario();
        let mut original = Harness::new(cfg);
        for split in 0..=inputs.len() {
            let mut resumed = original.resume_here();
            assert_eq!(resumed.state(), original.state(), "split {split}");
            let mut reference = Harness {
                scheduler: original.scheduler.clone(),
                inputs: original.inputs.clone(),
                admitted: original.admitted.clone(),
                handed_out: original.handed_out.clone(),
                last_block: original.last_block,
            };
            for input in &inputs[split..] {
                assert_eq!(
                    resumed.feed(input.clone()),
                    reference.feed(input.clone()),
                    "split {split}"
                );
            }
            assert_eq!(resumed.state(), reference.state(), "split {split}");
            if let Some(input) = inputs.get(split) {
                original.feed(input.clone());
            }
        }
    }

    /// A fixed-seed linear congruential generator: reproducible, no dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.below(100) < percent
        }
    }

    #[test]
    fn a_pseudo_random_run_keeps_the_split_and_resumes_equivalently() {
        const INPUTS: usize = 3_000;
        const RESUME_EVERY: usize = 97;
        let max_wait = 8;
        let cfg = bounded(max_wait, 4, 2);
        let keys = [key(31), key(32)];
        let mut pool = Vec::new();
        for (k, signer) in keys.iter().enumerate() {
            for nonce in 0..3 {
                for max_fee in [0, 3] {
                    let marker = u8::try_from(pool.len()).unwrap() + 100 * u8::try_from(k).unwrap();
                    pool.push(signed(signer, nonce, max_fee, vec![marker]));
                }
            }
        }
        pool.push(WireUserOp {
            signature: vec![0; WireUserOp::SIGNATURE_BYTES],
            ..pool[0].clone()
        });

        let mut rng = Lcg(0x5EED_CAFE);
        let mut main = Harness::new(cfg);
        let mut shadow = main.resume_here();
        let mut block = 10_u64;
        let mut outcomes: BTreeMap<String, u64> = BTreeMap::new();
        let mut ops_handed_out = 0;
        let mut over_bound = false;
        let (mut backstop_drains, mut overflow_drains) = (0_u64, 0_u64);
        for step in 0..INPUTS {
            block += rng.below(3);
            let input = if rng.chance(55) {
                let sender = if rng.chance(85) {
                    DIRECT_SENDER
                } else {
                    Address::repeat_byte(0x44)
                };
                let len = rng.below(4) as usize;
                let marker = u8::try_from(step % 256).unwrap();
                direct_from(sender, block, vec![marker; len])
            } else if rng.chance(4) {
                raw_batch(block, vec![0xFF; rng.below(5) as usize])
            } else {
                let expected = main.state().next_batch_nonce;
                let nonce = if rng.chance(90) {
                    expected
                } else {
                    expected + 1 + rng.below(2)
                };
                let mut frames = Vec::new();
                let mut safe_block = if rng.chance(70) {
                    block.saturating_sub(1 + rng.below(3))
                } else {
                    block.saturating_sub(1 + rng.below(max_wait + 3))
                };
                for _ in 0..rng.below(4) {
                    if rng.chance(3) {
                        safe_block = block;
                    } else if rng.chance(3) {
                        safe_block = safe_block.saturating_sub(1);
                    }
                    let fee_price = if rng.chance(2) {
                        crate::fee::MAX_EXPONENT + 1
                    } else {
                        u16::try_from(rng.below(4)).unwrap()
                    };
                    let ops = (0..rng.below(3))
                        .map(|_| pool[rng.below(pool.len() as u64) as usize].clone())
                        .collect();
                    frames.push(frame(safe_block, fee_price, ops));
                    safe_block = (safe_block + rng.below(3)).min(block.saturating_sub(1));
                }
                batch(block, nonce, frames)
            };

            let (input_block, is_direct) = (input.block, input.sender != SEQUENCER);
            let (outcome, txs) = main.feed(input.clone());
            assert_eq!(shadow.feed(input), (outcome, txs.clone()), "step {step}");
            // Overdue directs sit at the front and the backstop runs first, so
            // an overdue drain is the backstop's; on a direct, any other drain
            // is the overflow's.
            for scheduled in &txs {
                if let L2Tx::Direct(drained) = &scheduled.tx {
                    if age_exceeds(input_block, drained.block, max_wait) {
                        backstop_drains += 1;
                    } else if is_direct {
                        overflow_drains += 1;
                    }
                }
            }
            assert_eq!(shadow.state(), main.state(), "step {step}");
            let kind = format!("{outcome:?}");
            let kind = kind
                .split([' ', '(', ')', '{'])
                .filter(|word| !word.is_empty())
                .take(2)
                .collect::<Vec<_>>()
                .join(" ");
            *outcomes.entry(kind).or_default() += 1;
            ops_handed_out += txs
                .iter()
                .filter(|s| matches!(s.tx, L2Tx::UserOp(_)))
                .count();
            over_bound |= main.state().fridge_len > cfg.fridge_bound;
            if step % RESUME_EVERY == 0 {
                shadow = main.resume_here();
            }
        }

        // The run must have reached every rule it exists to exercise.
        for kind in [
            "DirectQueued",
            "DirectDropped",
            "BatchAccepted",
            "BatchStale",
            "BatchRejected DecodeFailed",
            "BatchRejected WrongNonce",
            "BatchRejected SafeBlockNotBelowInclusionBlock",
            "BatchRejected NonMonotonicSafeBlocks",
            "BatchRejected FeePriceOutOfRange",
            "BatchRejected FirstFrameBelowClock",
        ] {
            assert!(
                outcomes.get(kind).is_some_and(|n| *n > 0),
                "never saw {kind}: {outcomes:?}"
            );
        }
        assert!(ops_handed_out > 100, "{ops_handed_out} ops handed out");
        assert!(over_bound, "the soft bound never showed");
        assert!(backstop_drains > 0, "the backstop never drained");
        assert!(overflow_drains > 0, "the overflow never drained");
    }

    // ── Export ────────────────────────────────────────────────────────────

    #[test]
    fn the_export_is_six_little_endian_fields_in_order() {
        let export = SchedulerExport {
            l1_input_count: 1,
            l2_tx_count: 2,
            next_batch_nonce: 3,
            clock: 0x0102_0304_0506_0708,
            first_queued_input: 5,
            fridge_len: u64::MAX,
        };
        let bytes = export.encode();
        let mut expected = Vec::new();
        for field in [1_u64, 2, 3, 0x0102_0304_0506_0708, 5, u64::MAX] {
            expected.extend_from_slice(&field.to_le_bytes());
        }
        assert_eq!(bytes.as_slice(), expected.as_slice());
        assert_eq!(&bytes[24..32], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(SchedulerExport::decode(&bytes), Ok(export));
        for len in [0, 47, 49] {
            assert_eq!(
                SchedulerExport::decode(&vec![0; len]),
                Err(SchedulerExportLengthError { got: len })
            );
        }
        let state = export.scheduler_state();
        assert_eq!(SchedulerExport::new(state, L2TxCount::new(2)), export);
    }

    // ── Signatures and the off-chain predicate ────────────────────────────

    /// What the guest's recovery makes of each pinned signature variant: the
    /// sender it hands the op out as, or `None` when the op is dropped. The
    /// ingress side of the same table is pinned in `api.rs`.
    #[test]
    fn signature_variants_recover_the_pinned_guest_sender() {
        let vectors = crate::consensus_vectors::vectors();
        let pinned = &vectors.signature;
        let domain = vectors.domain(&pinned.domain);
        for case in &pinned.variants {
            let wire_user_op = WireUserOp {
                nonce: pinned.user_op.nonce,
                max_fee: pinned.user_op.max_fee,
                data: pinned.user_op.data.to_vec(),
                signature: case.signature.to_vec(),
            };
            assert_eq!(
                recover_sender(&domain, &wire_user_op),
                case.guest,
                "{}",
                case.name
            );
        }
    }

    // `ProtocolTiming::scheduler_accepts` (the off-chain predicate) and the
    // scheduler are maintained apart and must agree on accept-vs-reject (I1).
    // The documented self-trust omissions are pinned at the end, so any other
    // drift fails this test.

    const DUALITY_MAX_WAIT: u64 = 5;

    /// Whether each side accepts one input at `expected_nonce` and `clock`.
    fn duality_run(
        sender: Address,
        inclusion: u64,
        expected_nonce: u64,
        clock: u64,
        payload: &[u8],
    ) -> (bool, bool) {
        let mut h = at_state(DUALITY_MAX_WAIT, expected_nonce, clock);
        let mut input = raw_batch(inclusion, payload.to_vec());
        input.sender = sender;
        let canonical = h.feed(input).0 == InputOutcome::BatchAccepted;
        let timing = crate::protocol::ProtocolTiming::try_new(DUALITY_MAX_WAIT, 1, 1, 12)
            .expect("valid duality timing");
        let offchain = timing
            .scheduler_accepts(
                SEQUENCER,
                crate::protocol::SafeInputView {
                    safe_input_index: 0,
                    sender,
                    payload,
                    inclusion_block: inclusion,
                },
                expected_nonce,
            )
            .is_some();
        (canonical, offchain)
    }

    fn ssz(batch: &Batch) -> Vec<u8> {
        ssz::Encode::as_ssz_bytes(batch)
    }

    #[test]
    fn scheduler_accepts_agrees_with_canonical_on_accept_reject() {
        let one_frame = |nonce: u64, safe_block: u64| {
            ssz(&Batch {
                nonce,
                frames: vec![empty_frame(safe_block)],
            })
        };
        let empty = ssz(&Batch {
            nonce: 0,
            frames: vec![],
        });
        let agree: &[(&str, Address, u64, u64, Vec<u8>)] = &[
            (
                "valid fresh, right nonce",
                SEQUENCER,
                12,
                0,
                one_frame(0, 10),
            ),
            ("empty frames", SEQUENCER, 100, 0, empty.clone()),
            ("wrong nonce", SEQUENCER, 12, 0, one_frame(1, 10)),
            ("stale by first frame", SEQUENCER, 10, 0, one_frame(0, 1)),
            ("wrong sender", DIRECT_SENDER, 12, 0, one_frame(0, 10)),
            ("garbage payload", SEQUENCER, 1, 0, vec![0xFF, 0xEE, 0xDD]),
            ("fresh just under stale", SEQUENCER, 14, 0, one_frame(0, 10)),
            ("stale at the boundary", SEQUENCER, 15, 0, one_frame(0, 10)),
            (
                "first frame at the clock",
                SEQUENCER,
                12,
                10,
                one_frame(0, 10),
            ),
        ];
        for (label, sender, inclusion, clock, payload) in agree {
            let (canonical, offchain) = duality_run(*sender, *inclusion, 0, *clock, payload);
            assert_eq!(
                canonical, offchain,
                "I1 disagreement on `{label}`: canonical={canonical}, offchain={offchain}"
            );
        }

        // The documented self-trust omissions: the scheduler rejects, the
        // predicate accepts. If either side changes, these flip and force a
        // deliberate update.
        let omitted: &[(&str, u64, Vec<u8>)] = &[
            (
                "non-monotonic safe blocks",
                0,
                ssz(&Batch {
                    nonce: 0,
                    frames: vec![empty_frame(8), empty_frame(7)],
                }),
            ),
            (
                "fee price out of range",
                0,
                ssz(&Batch {
                    nonce: 0,
                    frames: vec![Frame {
                        fee_price: crate::fee::MAX_EXPONENT + 1,
                        ..empty_frame(10)
                    }],
                }),
            ),
            ("first frame above inclusion", 0, one_frame(0, 20)),
            ("first frame at inclusion", 0, one_frame(0, 12)),
            (
                "tail frame at inclusion",
                0,
                ssz(&Batch {
                    nonce: 0,
                    frames: vec![empty_frame(10), empty_frame(12)],
                }),
            ),
            ("first frame below the clock", 11, one_frame(0, 10)),
        ];
        for (label, clock, payload) in omitted {
            let (canonical, offchain) = duality_run(SEQUENCER, 12, 0, *clock, payload);
            assert!(
                !canonical && offchain,
                "`{label}`: expected canonical reject and predicate accept \
                 (documented omission), got canonical={canonical} offchain={offchain}"
            );
        }
    }

    // ── The scheduler-only conformance vectors ────────────────────────────

    /// Replays the vector file's scheduler-only section: the overflow drain and
    /// resume, at a small fridge bound the wallet's template never reaches.
    /// c-scheduler's conformance test holds scheduler.h to the same section.
    mod scheduler_vectors {
        use super::*;
        use serde_json::{Value, json};

        fn text(value: &Value) -> &str {
            value.as_str().expect("pinned string")
        }

        fn number(value: &Value) -> u64 {
            value.as_u64().expect("pinned number")
        }

        fn address(value: &Value) -> Address {
            text(value).parse().expect("pinned address")
        }

        /// The file's inputs: senders and payloads by name from the guest section.
        struct Inputs<'a> {
            guest: &'a Value,
            chain_id: u64,
            app_contract: Address,
        }

        impl Inputs<'_> {
            fn input(&self, index: u64, record: &Value) -> L1Input {
                L1Input {
                    index,
                    sender: address(&self.guest["senders"][text(&record["from"])]),
                    block: number(&record["block"]),
                    timestamp: number(&record["timestamp"]),
                    chain_id: self.chain_id,
                    app_contract: self.app_contract,
                    payload: alloy_primitives::hex::decode(text(
                        &self.guest["payloads"][text(&record["payload"])],
                    ))
                    .expect("pinned hex bytes"),
                }
            }
        }

        fn rendered_state(state: SchedulerState) -> Value {
            json!({
                "l1_input_count": state.l1_input_count,
                "next_batch_nonce": state.next_batch_nonce,
                "clock": state.clock,
                "first_queued_input": state.first_queued_input,
                "fridge_len": state.fridge_len,
            })
        }

        fn pinned_state(pinned: &Value) -> SchedulerState {
            SchedulerState {
                l1_input_count: number(&pinned["l1_input_count"]),
                next_batch_nonce: number(&pinned["next_batch_nonce"]),
                clock: number(&pinned["clock"]),
                first_queued_input: number(&pinned["first_queued_input"]),
                fridge_len: number(&pinned["fridge_len"]),
            }
        }

        /// One fed input in the file's form: outcome, reject, txs and state.
        fn rendered_step(h: &mut Harness, input: L1Input) -> Value {
            let (outcome, txs) = h.feed(input);
            let (name, reject) = match outcome {
                InputOutcome::DirectQueued => ("direct_queued", None),
                InputOutcome::DirectDropped => ("direct_dropped", None),
                InputOutcome::BatchAccepted => ("batch_accepted", None),
                InputOutcome::BatchStale => ("batch_stale", None),
                InputOutcome::BatchRejected(reason) => (
                    "batch_rejected",
                    Some(match reason {
                        BatchRejectReason::DecodeFailed => json!({"reason": "decode_failed"}),
                        BatchRejectReason::WrongNonce { expected, got } => {
                            json!({"reason": "wrong_nonce", "expected": expected, "got": got})
                        }
                        BatchRejectReason::SafeBlockNotBelowInclusionBlock => {
                            json!({"reason": "safe_block_not_below_inclusion_block"})
                        }
                        BatchRejectReason::NonMonotonicSafeBlocks => {
                            json!({"reason": "non_monotonic_safe_blocks"})
                        }
                        BatchRejectReason::FeePriceOutOfRange => {
                            json!({"reason": "fee_price_out_of_range"})
                        }
                        BatchRejectReason::FirstFrameBelowClock { safe_block, clock } => json!({
                            "reason": "first_frame_below_clock",
                            "safe_block": safe_block,
                            "clock": clock,
                        }),
                    }),
                ),
            };
            let txs: Vec<Value> = txs
                .iter()
                .map(|scheduled| match &scheduled.tx {
                    L2Tx::Direct(direct) => {
                        json!({"direct": direct.input_index, "batch_nonce": scheduled.batch_nonce})
                    }
                    L2Tx::UserOp(_) => panic!("the scheduler-only scenarios carry no user ops"),
                })
                .collect();
            let mut step = json!({
                "outcome": name,
                "txs": txs,
                "scheduler": rendered_state(h.state()),
            });
            if let Some(reject) = reject {
                step["reject"] = reject;
            }
            step
        }

        fn expected_step(step: &Value) -> Value {
            let mut expected = json!({
                "outcome": step["outcome"].clone(),
                "txs": step["txs"].clone(),
                "scheduler": step["scheduler"].clone(),
            });
            if let Some(reject) = step.get("reject") {
                expected["reject"] = reject.clone();
            }
            expected
        }

        fn refusal(error: &ResumeError) -> &'static str {
            match error {
                ResumeError::FirstQueuedAfterCount { .. } => "first_queued_after_count",
                ResumeError::OutOfSequence { .. } => "out_of_sequence",
                ResumeError::BeyondInputCount { .. } => "beyond_input_count",
                ResumeError::MissingInputs { .. } => "missing_inputs",
                ResumeError::BlockDecreases { .. } => "block_decreases",
                ResumeError::FirstQueuedNotADirect { .. } => "first_queued_not_a_direct",
                ResumeError::DirectNotAboveClock { .. } => "direct_not_above_clock",
                ResumeError::FridgeLenMismatch { .. } => "fridge_len_mismatch",
            }
        }

        #[test]
        fn the_reference_reproduces_every_scheduler_only_scenario() {
            let vectors = crate::consensus_vectors::raw_vectors();
            let guest = &vectors["guest"];
            let inputs = Inputs {
                guest,
                chain_id: number(&guest["chain_id"]),
                app_contract: address(&guest["app_contract"]),
            };
            let section = &vectors["scheduler_only"];
            let pinned_config = &section["config"];
            let config = SchedulerConfig {
                sequencer_address: address(&pinned_config["sequencer_address"]),
                max_wait_blocks: number(&pinned_config["max_wait_blocks"]),
                fridge_bound: number(&pinned_config["fridge_bound"]),
                max_direct_payload_bytes: number(&pinned_config["max_direct_payload_bytes"]),
            };

            let scenarios = section["scenarios"].as_array().expect("pinned scenarios");
            assert!(!scenarios.is_empty());
            for scenario in scenarios {
                let name = text(&scenario["name"]);
                let mut h = Harness::new(config);
                for (index, step) in (0_u64..).zip(scenario["steps"].as_array().expect("steps")) {
                    let rendered = rendered_step(&mut h, inputs.input(index, step));
                    assert_eq!(rendered, expected_step(step), "{name}, step {index}");
                }
            }

            let cases = section["resume"].as_array().expect("pinned resume cases");
            assert!(!cases.is_empty());
            for case in cases {
                let name = text(&case["name"]);
                let state = pinned_state(&case["state"]);
                let queued = case["queued"]
                    .as_array()
                    .expect("pinned range")
                    .iter()
                    .map(|record| inputs.input(number(&record["index"]), record))
                    .collect();
                let resumed = Harness::resumed(config, state, queued);
                let mut h = match (resumed, text(&case["result"])) {
                    (Ok(h), "resumed") => h,
                    (Err(error), "refused") => {
                        assert_eq!(refusal(&error), text(&case["refusal"]), "{name}");
                        continue;
                    }
                    (Ok(_), pinned) => panic!("{name}: pinned {pinned}, but it resumed"),
                    (Err(error), pinned) => panic!("{name}: pinned {pinned}, but {error}"),
                };
                assert_eq!(h.state(), state, "{name}");
                let then = case["then"]
                    .as_array()
                    .expect("pinned steps after a resume");
                for (index, step) in (state.l1_input_count..).zip(then) {
                    let rendered = rendered_step(&mut h, inputs.input(index, step));
                    assert_eq!(rendered, expected_step(step), "{name}, step {index}");
                }
            }
        }
    }
}
