# What an application must provide

An application lives in two places: natively, inside the sequencer and its
clients, and in the canonical machine, the template every validator runs.
Recovery moves state from the second to the first, so both sides carry
obligations. Some are written down in the
[application contract](../../protocol/application-contract.md) and the
[C binding](../../protocol/c-application-binding.md); this page collects them
all, and marks what is new.

## Native: the `Application` trait

What the sequencer calls today, in
[`sequencer-core/src/application/mod.rs`](../../../sequencer-core/src/application/mod.rs):

| Method | Obligation |
|---|---|
| `max_method_payload_bytes` | a fixed cap on a user op's payload |
| `validate_user_op` | decide, without changing state, whether an op runs; today, that its nonce is the sender's next and its sender can pay the fee |
| `apply_valid_user_op` | apply an accepted op: charge its fee, and advance the sender's nonce and the L2 tx count by exactly one, business failures included |
| `apply_direct_input` | apply a direct; it always advances the L2 tx count, never a nonce |
| `progress` | report the L2 tx count (today also a clock, which the next template drops) |
| `create_dump`, `from_dump`, `state_file_in_dump` | write a durable dump, load it back to the same state, and name the file that holds the comparison bytes |
| `CanonicalState::canonical_snapshot_bytes`, when our scheduler hosts the guest | the comparison bytes, which the guest's `state` inspect report returns and the watchdog hashes |

The rules behind them: execution is deterministic and reads nothing but the
state and the transaction ([§1](../../protocol/application-contract.md#1-determinism--purity));
a rejected op changes nothing, and every included op follows the nonce rule
([§2](../../protocol/application-contract.md#2-replay-safety--rejection-inclusion-and-failure));
an `AppError` is fatal and defines no successor state.

New for this plan:

- **A direct filter.** A stateless predicate that says whether a direct is
  well formed. It reads only the direct and the application's fixed
  configuration, such as the wallet's portal and token addresses, never state
  that transactions change. The scheduler drops what it refuses, and so must
  the sequencer's lane, or the two diverge.
- **A projection from a machine.** Build a native instance from the
  application state a machine exports. For the wallet that is its SSZ snapshot;
  for libdex, the flat buffer its drive holds. The instance must start at the
  machine's L2 tx count.
- **Progress is the L2 tx count alone**, with the next template.

## The canonical machine

What the guest must do, for recovery and for every validator:

- **Run the scheduler, then the executor, input by input**, matching our
  reference bit for bit, as the conformance vectors test.
- **Never answer an input with `RX_REJECTED`**, and halt only on the
  [fatal conditions](../../protocol/scheduler-semantics.md#fatal-conditions).
  A revert would roll the scheduler state back with the application, so the
  state would depend on the application.
- **Keep each input's index.** The guest records how many inputs it has
  consumed, and each queued direct keeps its own index.
- **Answer the inspect queries setup reads:**

| Query | Returns |
|---|---|
| `state` | the application state, the same bytes the watchdog compares (exists today) |
| `scheduler` | the L1 input count, the L2 tx count, the next batch nonce, the floor block if the template has the floor gate, and the fridge's length |
| `fridge` with a start and a length | that page of the fridge, oldest first: each direct's index, sender, block and payload |

An inspect report holds at most 2 MiB, so the fridge comes in pages. Its
entries can be large: a direct can carry nearly 64 KiB, the InputBox's cap on
an encoded input.

## Where this gets written down

The trait's rows already live in the application contract. The direct filter,
the projection, the inspect queries and the recorded indices are new, so they
go into the application contract and the C binding when they land
([todo](todo.md#promote-to-real-docs)).
