# The interfaces

**Status:** the Rust side, `application-engine.h` and `scheduler.h` with its
reference export are code in this tree. An application's author may implement
`scheduler.h` with their own scheduler; the Rust adapter that would link it
is not built ([todo](todo.md#build)). The signatures of the two
components, in Rust and in C. The [scheduler](scheduler.md) owns the rules
they carry out, and [application](application.md) owns the trait's
obligations and the machine's export. Where this page and the code differ,
the code is right.

## The model

```text
L1 inputs ──▶ scheduler ──▶ L2 transactions ──▶ application ──▶ state, outputs
```

- **The scheduler turns inputs into transactions.** It reads nothing from the
  application: not its state, not its code.
- **The application executes every transaction it is handed.** It never
  refuses one.
- **One check needs both sides.** A user op executes only at its sender's next
  nonce. The application stores the nonce, and shared code compares it.

There are four interfaces, each component in Rust and in C:

| Interface | Implemented by | Called by |
|---|---|---|
| `Scheduler`, a Rust struct in `sequencer-core` | The reference scheduler | The guest harness, through `CanonicalRunner`, and setup |
| `scheduler.h` | The reference scheduler's C export, or an author's own scheduler | A C or C++ guest; no Rust adapter exists yet |
| `Application`, a Rust trait | An application, and the adapter over `application-engine.h` | The executor, the lane and setup |
| `application-engine.h` | A native engine | The Rust adapter |

One record crosses from the scheduler to the application, the
[L2 transaction](#l2-transactions), and it is the same record in the guest, in
setup's rebuild, in the lane's replay and on the feed.

## Who decides a user op's fate

The [scheduler](scheduler.md#a-user-ops-fate) owns the rule: an op its sender
did not authorize is skipped, an op the operator does not want is refused at
`POST /tx`, and every other op executes. Each verdict has one place here:

| Verdict | Decided in |
|---|---|
| Skipped for its signature or its fee cap | The [scheduler](#the-scheduler), which never hands the op out |
| Skipped for its nonce | The [executor](#the-executor) |
| Refused | `admit_user_op`, which only the lane calls |
| What an executed op pays and does | `apply_user_op` |

## L2 transactions

```rust
/// What the scheduler hands out and the application executes.
pub enum L2Tx {
    Direct(Direct),
    UserOp(UserOpTx),
}

pub struct Direct {
    pub input_index: u64,
    pub sender: Address,
    /// Its L1 inclusion block.
    pub block: u64,
    pub timestamp: u64,
    pub payload: Vec<u8>,
}

pub struct UserOpTx {
    /// The signer the scheduler recovered.
    pub sender: Address,
    /// The nonce the sender signed.
    pub nonce: u32,
    /// The frame's fee price, which the op's cap covers.
    pub fee: u16,
    /// The frame's safe block.
    pub block: u64,
    pub data: Vec<u8>,
}
```

The op keeps its nonce, so every caller runs the same nonce check, a replay
included. `UserOpTx::new(sender, &op, fee, block)` builds the record from a
signed op and its frame, the same way in every caller.

## The scheduler

```rust
/// Deployment constants, compiled into the template. No default: a build
/// that forgot one would run another deployment's rules.
pub struct SchedulerConfig {
    pub sequencer_address: Address,
    /// At least 1; genesis and resume assert it.
    pub max_wait_blocks: u64,
    pub fridge_bound: u64,
    /// A direct with a longer payload never joins the fridge.
    pub max_direct_payload_bytes: u64,
}

impl SchedulerConfig {
    /// The submitter and the application's limits.
    pub fn for_application<A: Application>(sequencer_address: Address) -> Self;
}

/// One InputBox input, as `EvmAdvance` delivers it.
pub struct L1Input {
    pub index: u64,
    pub sender: Address,
    pub block: u64,
    pub timestamp: u64,
    pub chain_id: u64,
    pub app_contract: Address,
    pub payload: Vec<u8>,
}

/// All that carries over between inputs, except the queued directs' bytes,
/// which L1 holds.
pub struct SchedulerState {
    /// Inputs consumed: the scheduler's position in the InputBox.
    pub l1_input_count: u64,
    pub next_batch_nonce: u64,
    pub clock: u64,
    /// Index of the oldest queued direct; `l1_input_count` when none is queued.
    pub first_queued_input: u64,
    /// How many directs are queued, to check the re-supplied range against.
    pub fridge_len: u64,
}

pub enum InputOutcome {
    DirectQueued,
    DirectDropped,
    BatchAccepted,
    BatchStale,
    BatchRejected(BatchRejectReason),
}

pub struct Scheduled {
    /// The scheduler's next batch nonce when it handed this out: the batch
    /// whose frame holds or drained it, or the next batch to come.
    pub batch_nonce: u64,
    pub tx: L2Tx,
}

pub struct Scheduler { /* private */ }

impl Scheduler {
    pub fn genesis(config: SchedulerConfig) -> Self;

    /// Resume from an exported state. `queued` re-supplies every input from
    /// `first_queued_input` up to `l1_input_count`, in order. The scheduler
    /// re-admits the directs among them by the same rule, and refuses a
    /// range that does not match the state.
    pub fn resume(
        config: SchedulerConfig,
        state: SchedulerState,
        queued: impl IntoIterator<Item = L1Input>,
    ) -> Result<Self, ResumeError>;

    pub fn config(&self) -> &SchedulerConfig;
    pub fn state(&self) -> SchedulerState;

    /// Take the next input, whose index must be `l1_input_count`, and say
    /// what became of it. Panics on misuse: another index, or a previous
    /// input with transactions left.
    pub fn feed(&mut self, input: L1Input) -> InputOutcome;

    /// The current input's next transaction, in execution order.
    pub fn next_tx(&mut self) -> Option<Scheduled>;
}

pub enum ResumeError {
    FirstQueuedAfterCount { first_queued_input: u64, l1_input_count: u64 },
    OutOfSequence { expected: u64, got: u64 },
    BeyondInputCount { index: u64, l1_input_count: u64 },
    MissingInputs { next: u64, l1_input_count: u64 },
    BlockDecreases { index: u64, block: u64, previous_block: u64 },
    FirstQueuedNotADirect { index: u64 },
    DirectNotAboveClock { index: u64, block: u64, clock: u64 },
    FridgeLenMismatch { expected: u64, admitted: u64 },
}

/// The machine's `scheduler` inspect report: six little-endian `u64`, in
/// field order (48 bytes).
pub struct SchedulerExport {
    pub l1_input_count: u64,
    pub l2_tx_count: u64,
    pub next_batch_nonce: u64,
    pub clock: u64,
    pub first_queued_input: u64,
    pub fridge_len: u64,
}
```

- **It pulls, one transaction at a time.** The guest executes each transaction
  as it comes, a batch's signatures are recovered only as its ops are reached,
  and no callback crosses the C boundary.
- **It needs no application.** The fridge admits by sender and by payload
  length, both from the configuration. A user op is handed out once its sender
  is recovered and its cap covers the price; the nonce is the executor's check.
- **Resuming needs four numbers and a range of L1.** The fridge is a queue
  ([scheduler](scheduler.md#the-schedulers-state)), so `queued` is the inputs
  from `first_queued_input` up to `l1_input_count`, which setup's input table
  already has. Resume refuses a range out of sequence, short or long, whose
  blocks decrease, whose first input is not an admitted direct, with a queued
  direct at or below the clock, or that admits another number of directs.
- **Its state is the machine's export.** `CanonicalRunner`
  (`scheduler/driver.rs`) holds the scheduler and the application, feeds each
  input and executes its transactions, and answers the `state` and
  `scheduler` queries, the latter from `state()` and the application's count
  ([application](application.md#the-state-export)). A runner whose input
  failed with an `AppError` refuses further use.
- **It halts where the reference halts.** A batch nonce without a successor
  panics in Rust and returns a fatal status in C.

In C, the transaction records are the application's own, so a guest passes
them straight on. The header and its reference export are
[`bindings/c-scheduler`](../../../bindings/c-scheduler/include/scheduler.h),
which states each call's statuses and lifetimes:

```c
typedef struct Scheduler Scheduler;

typedef enum SchedulerStatus {
    SCHEDULER_STATUS_OK = 0,
    SCHEDULER_STATUS_DONE = 1,            /* next_tx: the input has no more */
    SCHEDULER_STATUS_MISUSE = -1,         /* out of order, a bad argument, a dead handle */
    SCHEDULER_STATUS_NO_SUCCESSOR = -2,   /* a counter is exhausted: halt */
    SCHEDULER_STATUS_RESUME_REFUSED = -3, /* the state and the queued range disagree */
    SCHEDULER_STATUS_INTERNAL_ERROR = -4, /* an impossible state */
} SchedulerStatus;

typedef enum SchedulerTxKind {
    SCHEDULER_TX_DIRECT = 0,
    SCHEDULER_TX_USER_OP = 1,
} SchedulerTxKind;

typedef union SchedulerTxValues {
    ApplicationEngineDirectInput direct;
    ApplicationEngineUserOpTx user_op;
} SchedulerTxValues;

typedef struct SchedulerTx {
    SchedulerTxKind kind;
    uint64_t batch_nonce;
    SchedulerTxValues values; /* spans borrowed until the next next_tx, feed or destroy */
} SchedulerTx;

uint32_t scheduler_abi_version(void);
const char *scheduler_get_last_error_message(void);
SchedulerStatus scheduler_create(const SchedulerConfig *config, Scheduler **out);
/* The queued range, from first_queued_input up to l1_input_count, in one call. */
SchedulerStatus scheduler_resume(const SchedulerConfig *config,
    const SchedulerState *state, const SchedulerInput *queued, uint64_t queued_count,
    Scheduler **out);
void scheduler_destroy(Scheduler *scheduler);
SchedulerStatus scheduler_state(const Scheduler *scheduler, SchedulerState *out);
SchedulerStatus scheduler_feed(Scheduler *scheduler, const SchedulerInput *input,
    SchedulerInputOutcome *out);
SchedulerStatus scheduler_next_tx(Scheduler *scheduler, SchedulerTx *out);
```

`SchedulerConfig`, `SchedulerInput`, `SchedulerState` and
`SchedulerInputOutcome` mirror the Rust records, with fixed widths and no
`size_t`, as the engine's header does.

- **Resume is one call.** It takes the whole queued range, as the Rust
  `resume` does, so a refused range never yields a handle and no handle is
  ever half resumed. It refuses a state whose `first_queued_input` is past its
  `l1_input_count` before it reads any input, and a range that does not match
  the state.
- **A failure kills the handle.** Where Rust panics, C returns a negative
  status, and after a negative status from `feed` or `next_tx` every call but
  `destroy` is a misuse. No port then has to prove which failures left its
  state whole, which is also why `scheduler_state` returns a status.
- **A feed waits for `DONE`.** Rust lets a feed follow an input whose
  transactions ran out unseen; C asks for the `DONE` the canonical loop always
  reads, so the export names every misuse before calling in.

## The executor

The one way a transaction reaches the application. The guest, setup's rebuild,
the lane and its replay all call it, and it is the whole of the canonical
user-op check that needs the application:

```rust
pub enum Executed {
    /// It executed; `index` is the L2 tx count before it, its index in the history.
    Done { index: L2TxCount, outputs: AppOutputs },
    /// A user op whose nonce is not its sender's next. Nothing changed.
    Skipped(SkipReason),
}

pub enum SkipReason {
    WrongNonce { expected: u32, got: u32 },
    /// The sender's next nonce is `u32::MAX`, which has no successor.
    NonceExhausted,
}

pub fn execute<A: Application>(app: &mut A, tx: &L2Tx) -> Result<Executed, AppError>;
```

A direct always executes. For a user op, the executor reads
`app.user_nonce(sender)`, skips on a mismatch, then on `u32::MAX`, and
otherwise applies the op. The mismatch comes first, so a stale op that carries
`u32::MAX` still reads as a wrong nonce. After either kind it checks
that the L2 tx count advanced by exactly one, and after a user op that the
sender's nonce did.

A canonical guest is then one loop, here in C:

```c
scheduler_feed(scheduler, &input, &outcome);
while (scheduler_next_tx(scheduler, &tx) == SCHEDULER_STATUS_OK) {
    if (tx.kind == SCHEDULER_TX_DIRECT) {
        application_engine_execute_direct_input(engine, &tx.values.direct, &outputs);
    } else {
        application_engine_user_nonce(engine, &tx.values.user_op.sender, &nonce);
        if (nonce != tx.values.user_op.nonce || nonce == UINT32_MAX) continue;
        application_engine_execute_user_op(engine, &tx.values.user_op, &outputs);
    }
    emit_outputs(engine, outputs);
}
```

The lane checks a live op with `check_live_user_op`, then calls `execute`. The
check runs the fee cap, the executor's own nonce rule, then admission, so an
op it lets through is one `execute` runs; a skip after it is a terminal lane
fault:

```rust
pub fn check_live_user_op<A: Application>(
    app: &A,
    sender: Address,
    op: &UserOp,
    frame_fee: u16,
) -> Result<Option<Refusal>, AppError>;

pub enum Refusal {
    FeeCapBelowPrice { max_fee: u16, price: u16 },
    Skip(SkipReason),
    Policy(PolicyRefusal),
}
```

A refusal reaches the client as a 422 whose text is the refusal's `Display`. The README
teaches clients to read the expected nonce from `bad nonce: expected N, got M`,
and the tests match the fee-balance text.

## The application

```rust
pub trait Application: Send + Sized {
    // Execution: the canonical machine, the sequencer and every replica.

    /// The sender's next nonce; zero for an address that has sent nothing.
    fn user_nonce(&self, sender: Address) -> Result<u32, AppError>;

    /// Execute a user op. It never refuses: whatever the op does or pays,
    /// the sender's nonce and the L2 tx count each advance by one.
    fn apply_user_op(&mut self, op: &UserOpTx) -> Result<AppOutputs, AppError>;

    /// Execute a direct. The L2 tx count advances by one; no nonce moves.
    fn apply_direct(&mut self, direct: &Direct) -> Result<AppOutputs, AppError>;

    /// The L2 tx count.
    fn l2_tx_count(&self) -> L2TxCount;

    // Admission: the sequencer's lane only. Policy, never consensus.

    fn max_method_payload_bytes() -> usize;

    /// The template's fridge bound and direct payload cap.
    fn scheduler_limits() -> SchedulerLimits;

    /// Does the operator want this op in a frame at this fee? The canonical
    /// machine never asks, and the answer never changes what an included
    /// op does.
    fn admit_user_op(
        &self,
        sender: Address,
        op: &UserOp,
        fee: u16,
    ) -> Result<Admission, AppError>;

    // Durable state: the sequencer and replicas.

    fn from_dump(prefix: &Path) -> Result<Self, AppError>;
    fn create_dump(&mut self, prefix: &Path) -> Result<(), AppError>;
    fn state_file_in_dump(prefix: &Path) -> PathBuf;

    /// Build an instance from a canonical machine's comparison bytes, at the
    /// machine's L2 tx count. A dump of the result holds the same bytes.
    fn from_canonical_state(
        state_file: &Path,
        l2_tx_count: L2TxCount,
    ) -> Result<Self, AppError>;
}

pub enum Admission {
    Admit,
    Refuse(PolicyRefusal),
}

pub struct SchedulerLimits {
    /// The staleness window, at least 1.
    pub max_wait_blocks: u64,
    pub fridge_bound: u64,
    pub max_direct_payload_bytes: u64,
}

pub enum PolicyRefusal {
    /// The sender cannot pay the fee.
    Underfunded { required: U256, available: U256 },
    Other(String),
}
```

- **The application has no say in which ops execute.** It stores nonces and
  answers `user_nonce`; the comparison is shared code. An application bug can
  then change what a transaction does, but not which transactions execute or
  what their counts are.
- **Admission is the old validation without the nonce.** A guest build needs
  only the first group of methods.
- **The projection takes the count.** Whether the comparison bytes hold the
  L2 tx count or not, the instance starts at the machine's.
- **`user_nonce` may need a "no such sender" answer (Open).** An engine with a
  bounded account table cannot store a nonce for an address it has never seen.
  Its ops would then be skipped, as a wrong nonce is
  ([todo](todo.md#questions-for-bart)).

[Application](application.md#the-application-trait) states each method's
obligation and what changes from today.

`application-engine.h` matches it. Its ABI is version 3: version 2 made the
changes below, and version 3 added the staleness window to the limits.

| Version 1 | Version 2 |
|---|---|
| `application_engine_validate_user_op` | `application_engine_admit_user_op`: the same call without the nonce check, refusing with the fee-balance reason or a new policy reason |
| No nonce getter | `application_engine_user_nonce(engine, sender, out_nonce)` |
| `ApplicationEngineValidUserOp` and a `safe_block` argument | `ApplicationEngineUserOpTx`: sender, nonce, fee, safe block and data |
| `application_engine_execute_valid_user_op` fails on an unpayable fee | `application_engine_execute_user_op` never fails on one |
| `ApplicationEngineDirectInput`: sender, block and payload | The same with the input index and timestamp |
| `ApplicationEngineProgress`: count and clock | `application_engine_l2_tx_count` |
| No projection | `application_engine_from_canonical_state(state_file, l2_tx_count, out_engine)` |
| No version | `application_engine_abi_version()`, which the adapter checks before anything else |
| No limits | `application_engine_scheduler_limits(out_limits)` |

A new function fails at link time against an old archive, but a changed
meaning does not: an engine that still fails on an unpayable fee would halt
the canonical machine on the first underfunded op a batch carries. The version
is the only check on that. The reason values the engine stops producing keep
their numbers, since the header's vocabularies are append only; admission
adds `APPLICATION_ENGINE_REFUSED_BY_POLICY` (4), whose text travels in the
last error message.
