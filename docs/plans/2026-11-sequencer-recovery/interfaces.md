# Interfaces

Outlines in Rust, to show roles and boundaries; field and method names are
proposals. The [procedure](procedure.md) says how they are used.

## The command

```text
sequencer setup --recovery <MACHINE_DIR> [--dry-run]
```

`MACHINE_DIR` is a stored Cartesi machine of the deployment's template,
stopped between inputs. `--dry-run` prints what setup would read from it and
stops before the flush. Setup takes no client count.

Loading a machine needs the emulator. Today only `sdk/guest/testsi` links it,
through the `cartesi-machine` crate; setup would link it the same way.

## What setup reads from the machine

```rust
/// Everything setup takes from the base machine.
pub struct MachineExport {
    /// InputBox inputs the machine has consumed: its position.
    pub l1_input_count: u64,
    /// L2 transactions it has executed: the new era's base count.
    pub l2_tx_count: u64,
    pub scheduler: SchedulerState,
    /// The application's state as the machine holds it, for the projection.
    pub application_state: Vec<u8>,
}

/// Everything the canonical scheduler carries from one input to the next.
pub struct SchedulerState {
    pub next_batch_nonce: u64,
    /// Admitted directs not yet executed, oldest first.
    pub fridge: Vec<QueuedDirect>,
    /// The floor gate's block, if the template has the gate.
    pub floor_block: Option<u64>,
}

pub struct QueuedDirect {
    pub input_index: u64,
    pub sender: Address,
    pub block: u64,
    pub payload: Vec<u8>,
}
```

The guest answers inspect queries for these
([application requirements](application.md#the-canonical-machine)): one for
the counts and the scalar scheduler state, and one that returns a page of the
fridge, since an inspect report holds at most 2 MiB.

## The scheduler, without an application

```rust
/// One L1 input, as the InputBox delivers it.
pub struct L1Input {
    pub index: u64,
    pub sender: Address,
    pub block: u64,
    pub payload: Vec<u8>,
}

/// A transaction the scheduler hands the application, in execution order.
pub enum ScheduledTx {
    /// Always executes. `batch_nonce` names the batch whose frame drained it;
    /// it is `None` when the backstop or an overflow drained it.
    Direct { direct: QueuedDirect, batch_nonce: Option<u64> },
    /// Executes if the application accepts it.
    UserOp {
        sender: Address,
        op: UserOp,
        fee: u16,
        safe_block: u64,
        batch_nonce: u64,
    },
}

pub struct SchedulerConfig {
    pub sequencer_address: Address,
    pub max_wait_blocks: u64,
    pub fridge_bound: usize,
    /// The application's direct filter, built from its fixed configuration.
    pub is_well_formed: Box<dyn Fn(&L1Input) -> bool + Send>,
}

/// The canonical scheduler. It judges batches, keeps the fridge and decides
/// which transactions the application must try. It never reads the
/// application's state.
pub trait Scheduler {
    fn resume(state: SchedulerState, config: SchedulerConfig) -> Self;
    /// Processes one input and returns what it schedules, in order.
    fn process(&mut self, input: &L1Input) -> Vec<ScheduledTx>;
    fn state(&self) -> &SchedulerState;
}
```

Our implementation is the reference. Bart's guest runs his own port, held to
ours by the conformance vectors. A second implementation that calls his
scheduler through the C seam stays deferred, as before.

## The executor

```rust
pub struct ExecutedTx {
    pub l2_tx_count: u64,
    pub tx: ScheduledTx,
    pub outputs: AppOutputs,
}

/// Runs scheduled transactions through the shared execution boundary: each
/// one executes and takes the next L2 tx count, or is a user op the
/// application rejects. An `AppError` is where the canonical machine halts.
pub fn execute<A: Application>(
    app: &mut A,
    txs: Vec<ScheduledTx>,
) -> Result<Vec<ExecutedTx>, AppError>;
```

The canonical guest is the scheduler followed by the executor, input by input.
Setup runs the same two pieces, one after the other, over the whole rebuilt
range.

## The rebuilt history table

One row per scheduled transaction, in order:

| Column | Holds |
|---|---|
| `l1_input_index` | the input whose processing scheduled it |
| `position` | its place among that input's transactions |
| the transaction | a direct's input index and drain batch nonce, or a user op's sender, nonce, fee, data, safe block and batch nonce |
| `l2_tx_count` | its count once executed; empty if the application rejected it |

Rows come from safe L1 blocks, so they never change: no standard recovery cuts
below the stop block.

## The era and the feed

The new era records:

| Field | Value |
|---|---|
| base count | the base machine's L2 tx count; `/history` reports it as `available_from` |
| rebuilt history end | the L2 tx count at the end of the stop block |
| `base_safe_block` | the stop block, as today; acceptance scans only blocks after it |
| first queued input | where the lane starts draining directs; an era without one drains from the first input after `base_safe_block`, as today |
| anchor batch nonce | the next batch nonce at the stop block, as today |

The feed serves the executed rows from the base count: from the rebuilt
history table up to its end, then from the live table. A direct row takes its
timestamp and transaction hash from the input table, by input index. Its
`batch_nonce` becomes optional, since a direct the backstop or an overflow
drained belongs to no batch.
