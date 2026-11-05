# The application and its machine

**Status:** proposed on 2026-11-05 for the next template, to settle with Bart;
nothing here is implemented. **New** marks what changes.

An application lives in two places: natively, inside the sequencer and its
clients, and in the canonical machine, the template every validator runs. A
recovery reads a machine's state and builds the native side from it, so both
sides carry obligations. The [scheduler](scheduler.md) owns the rules the
machine runs; this page owns the interfaces around them: the machine's state
export, the `Application` trait, and the guest's duties.

## The state export

**New.** Setup reads a stopped machine's whole state through inspect queries.
The interface is generic. The canonical harness answers the scheduler's part
for every application, since it owns that state, and the application answers
only for its own state, in an encoding it chooses.

| Query | Answered by | Returns |
|---|---|---|
| `scheduler` | The harness | The counts, the next batch nonce, the clock and the fridge's length |
| `fridge`, with a start and a count | The harness | A page of fridge entries, oldest first |
| `state`, with a page | The application | A page of the application's export |

An outline in Rust; the names and the encoding are proposals. SSZ, as batches
use, would do:

```rust
/// An inspect request's payload.
pub enum ExportQuery {
    Scheduler,
    Fridge { start: u64, count: u32 },
    State { page: u32 },
}

pub struct SchedulerExport {
    /// InputBox inputs the machine has consumed: its position.
    pub l1_input_count: u64,
    /// L2 transactions it has executed: the base count of a recovery from it.
    pub l2_tx_count: u64,
    pub next_batch_nonce: u64,
    /// The latest block the scheduler has executed at.
    pub clock: u64,
    pub fridge_len: u64,
}

/// A queued direct. The first entry's index is the first queued input.
pub struct FridgeEntry {
    pub input_index: u64,
    pub sender: Address,
    pub block: u64,
    pub timestamp: u64,
    pub payload: Vec<u8>,
}

pub struct StatePage {
    pub page_count: u32,
    pub bytes: Vec<u8>,
}
```

- **Each query returns one report**, and a report holds at most 2 MiB, so the
  fridge and the state come in pages. A `fridge` reply holds as many of the
  asked entries as fit, and the reader continues after the last one. An entry
  can carry nearly 64 KiB, the InputBox's cap on an encoded input.
- **The export is the comparison bytes.** Today the `state` query returns the
  bytes the watchdog hashes, whole, in one report
  ([`inspect_state`](../../../sequencer-core/src/scheduler/mod.rs)). The same
  bytes, paged, are what the native side builds from, so one method serves
  both.
- **The application chooses its encoding.** The wallet exports its SSZ
  snapshot; libdex could export the flat buffer its drive holds, as it is.
- **An unknown query gets a report saying so**, never a halt, as today.

## The `Application` trait

What the sequencer calls, in
[`sequencer-core/src/application/mod.rs`](../../../sequencer-core/src/application/mod.rs).
The [C binding](../../protocol/c-application-binding.md) maps it to native
engines and gets the same additions.

| Method | Obligation |
|---|---|
| `max_method_payload_bytes` | A fixed cap on a user op's payload |
| `validate_user_op` | Decide, without changing state, whether an op executes; **New:** only a wrong nonce rejects it, and an underfunded op is reported as such |
| `apply_valid_user_op` | Advance the sender's nonce and the L2 tx count by exactly one, and charge the fee, business failures included; **New:** never fail on a fee the sender cannot pay |
| `apply_direct_input` | Apply a direct, advancing the L2 tx count and never a nonce; **New:** it receives the input index and timestamp |
| `progress` | Report the L2 tx count; **New:** the count alone, without today's clock |
| `create_dump`, `from_dump`, `state_file_in_dump` | Write a durable dump, load it back to the same state, and name the file that holds the comparison bytes |
| `CanonicalState::canonical_snapshot_bytes` | The comparison bytes, which the `state` query returns; **New:** paged |
| **New:** the direct filter | Say whether a direct is well formed, from the direct and the application's fixed configuration alone |
| **New:** the projection | Build a native instance from a machine's export |

- **Underfunded ops.** The scheduler executes an op whose sender cannot pay
  the fee ([scheduler](scheduler.md#frame-execution)), so validation reports it
  apart from a wrong nonce. The sequencer's ingress refuses it, since it would
  pay less than the L1 data it costs; the canonical boundary executes it. How
  much `apply_valid_user_op` charges and whether the effect runs are the
  application's choice. Charging what the sender has is the recommendation.
- **The direct filter.** A stateless predicate: it reads only the direct and
  fixed configuration, such as the wallet's portal and token addresses, never
  state that transactions change. The guest drops what it refuses, and so must
  the sequencer's lane, or the two diverge.
- **The projection.** The instance it builds must equal the machine's: the
  same comparison bytes and the same L2 tx count. For the wallet that means
  decoding its SSZ snapshot; for libdex, loading the flat buffer.

The rules behind the trait still hold. Execution is deterministic and reads
nothing but the state and the transaction
([§1](../../protocol/application-contract.md#1-determinism--purity)); a
rejected op changes nothing, and every executed op follows the nonce rule
([§2](../../protocol/application-contract.md#2-replay-safety--rejection-inclusion-and-failure));
an `AppError` is fatal and defines no successor state.

## The canonical machine

What the guest must do, for every validator and for recovery:

- **Run the scheduler, then the application, input by input**, matching the
  [scheduler](scheduler.md) bit for bit, as the conformance vectors test.
- **Never answer an input with `RX_REJECTED`**, and halt only on the
  [fatal conditions](scheduler.md#no-reverts-and-when-to-halt).
- **Keep each input's index.** The guest records how many inputs it has
  consumed, and each queued direct keeps its own index.
- **Answer the [export queries](#the-state-export).**
- **Compile in the pinned submitter** as `sequencer_address`.

## Where this gets written down

The trait's rows live in the
[application contract](../../protocol/application-contract.md). The export,
the direct filter, the projection and the changed rows go into it and into the
C binding when the template lands ([todo](todo.md#promote-to-real-docs)).
