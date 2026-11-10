# The application and its machine

**Status:** proposed on 2026-11-05 for the next template, to settle with Bart;
nothing here is implemented. **New** marks what changes.

An application lives in two places: natively, inside the sequencer and its
clients, and in the canonical machine, the template every validator runs. A
recovery reads a machine's state and builds the native side from it, so both
sides carry obligations. The [scheduler](scheduler.md) owns the rules the
machine runs; this page owns what surrounds them: the machine's state export,
the `Application` trait's obligations, and the guest's duties.
[Interfaces](interfaces.md) outlines the signatures, in Rust and in C.

## The state export

**New.** Setup reads two things from a stopped machine. The canonical harness
answers the scheduler's part for every application, since it owns that state,
and the application answers only for its own state, in an encoding it chooses.

| What | How setup reads it | Answered by |
|---|---|---|
| The application's state | Through the deployment's state source, as the watchdog does: the `state` inspect report, or the labeled range | The application |
| The scheduler's state and the counts | The `scheduler` inspect query | The harness |

The `scheduler` report is six little-endian `u64`s, in this order:

```rust
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
```

- **The application's state is the comparison bytes**, read the way the
  watchdog reads them ([state sources](../../watchdog/README.md#state-sources)).
  It needs no new query: a state too large for one report already uses the
  range source.
- **The fridge is named, not exported.** It holds the admitted directs from the
  first queued input up to the L1 input count
  ([scheduler](scheduler.md#the-schedulers-state)). Setup reads them from its
  own input table and checks the export against it
  ([operations](operations.md#what-setup-does)).
- **The application chooses its encoding.** The wallet exports its SSZ
  snapshot; libdex could export the flat buffer its drive holds, as it is.
- **The `state` query keeps its meaning.** The literal bytes or an empty
  payload return the comparison bytes whole, as today
  ([`inspect_state`](../../../sequencer-core/src/scheduler/mod.rs)). The
  literal `scheduler` is the one query added.
- **An unknown query gets a report saying so**, never a halt, as today.

## The `Application` trait

What the sequencer calls, in
[`sequencer-core/src/application/mod.rs`](../../../sequencer-core/src/application/mod.rs).
[Interfaces](interfaces.md#the-application) outlines the trait as proposed.
The [C binding](../../protocol/c-application-binding.md) maps it to native
engines and gets the same changes.

| Method | Obligation |
|---|---|
| `max_method_payload_bytes` | A fixed cap on a user op's payload, which only the lane enforces |
| **New:** `user_nonce` | Report a sender's next nonce, which the shared executor compares with the op's |
| `admit_user_op`, today's `validate_user_op` | Say, without changing state, whether the operator wants an op; **New:** it no longer checks the nonce, and only the lane calls it |
| `apply_user_op`, today's `apply_valid_user_op` | Advance the sender's nonce and the L2 tx count by exactly one, business failures included; **New:** never fail, even on a fee the sender cannot pay |
| `apply_direct`, today's `apply_direct_input` | Apply a direct, advancing the L2 tx count and never a nonce; **New:** it receives the input index and timestamp |
| `l2_tx_count`, today's `progress` | Report the L2 tx count; **New:** the count alone, without today's clock |
| `create_dump`, `from_dump`, `state_file_in_dump` | Write a durable dump, load it back to the same state, and name the file that holds the comparison bytes |
| `CanonicalState::canonical_snapshot_bytes` | The comparison bytes, which the `state` query returns whole |
| **New:** `from_canonical_state` | Build a native instance from a machine's comparison bytes, at the machine's L2 tx count |

- **Nonces.** The application stores each sender's next nonce and reports it.
  It no longer decides whether an op's nonce is right: the shared executor
  compares before an op and checks the one-step advance after it, so the
  [nonce rule](../../protocol/application-contract.md#user-nonces) is checked
  the same way for every application. The same getter is what
  [Track 6](../2026-07-coordination-tracks.md#track-6--dump--application-api-redesign)
  proposes for an exact `GET /nonce`.
- **What an op pays.** The scheduler executes every op its sender authorized,
  funded or not ([scheduler](scheduler.md#a-user-ops-fate)). How much
  `apply_user_op` charges and whether the effect runs are the application's
  choice. Charging what the sender has, a saturating subtraction, is the
  recommendation.
- **Admission.** The lane asks before it includes an op, and refuses an
  underfunded one, since it would pay less than the L1 data it costs. It is
  policy: the canonical machine never calls it, a guest build need not link
  it, and its answer never changes what an included op does. An operator's
  exemptions, such as a sponsored role, are answered here.
- **The projection.** The instance it builds must equal the machine's: the
  same comparison bytes, at the L2 tx count it is given. For the wallet that
  means decoding its SSZ snapshot; for libdex, loading the flat buffer.

The rules behind the trait still hold. Execution is deterministic and reads
nothing but the state and the transaction
([§1](../../protocol/application-contract.md#1-determinism--purity)); a
skipped op changes nothing, and every executed op follows the nonce rule
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
- **Answer the [export](#the-state-export)**: its state through the state
  source, and the `scheduler` query.
- **Compile in the deployment's constants**: the pinned submitter as
  `sequencer_address`, the fridge's bound and the directs' payload cap.

## Where this gets written down

The trait's rows live in the
[application contract](../../protocol/application-contract.md). The export,
the projection and the changed rows go into it and into the C binding when the
template lands ([todo](todo.md#promote-to-real-docs)).
