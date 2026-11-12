# libdex, as its author describes it — 2026-11-12

Scope: what libdex, the first application built on this sequencer, does today,
from its author's answers to fourteen questions on 2026-11-12. Each section
gives the answers, then what they mean for this tree, checked against
`2aaa4d7`. The second part is our reading, not the author's words.

Retained for the [recovery plan's todo](../plans/2026-11-sequencer-recovery/todo.md#libdex)
and the register's [fridge](register.md#bound-the-canonical-direct-input-fridge)
and [canonical rules](register.md#canonical-rules-open-for-the-c-port)
entries. It describes libdex at one date and no number here is measured.
Replace it when libdex ports to this tree's headers.

## The revision libdex builds against

libdex builds against `8a7c914` (2026-10-06), on a branch that bumps the
contracts to `3.0.0-alpha.10`. Its author will move to the next release whose
sequencer, node, contracts, emulator and CLI work together.

What it means: that revision has `application_engine_validate_user_op` and no
scheduler header. The port crosses to admission, total execution and
[`scheduler.h`](../../bindings/c-scheduler/include/scheduler.h) in one step,
so a change to the signed op or to either header costs libdex least before
that port.

## Ops and their sizes

- A price push's method is `86 + 27 × N` bytes for `N` markets, `N ≤ 255`: 167
  bytes at 3 markets, 518 at 16, 6,971 at 255. Fewer than 16 markets are
  expected at launch; the number is a business choice.
- Every other op is 9 to 50 bytes.
- Pushing is open to anyone, but a push must carry the oracle's signature
  beside its sender's.
- A late push is included and discarded today, which costs a fee and machine
  time for nothing. libdex can tell a redundant push without advancing its
  tick. Whether pushers are refunded is undecided on libdex's side.

What it means:

- Admission's `APPLICATION_ENGINE_REFUSED_BY_POLICY` answers the redundant
  push, and it is cheap for libdex to compute. Nothing changes here.
- The lane closes a batch on `count × (71 + max_method_payload_bytes)`
  against a target of about 12.6 KB. With the bound at 167 bytes a batch
  closes at 53 ops, at 518 bytes at 22, and at 6,971 bytes at 2, where about
  110 small ops fit. Closing on real size
  ([todo](../plans/2026-11-sequencer-recovery/todo.md#build)) has to land
  before libdex ports.
- The fee assumes 126 bytes an op. A push takes 250 bytes on the wire at 3
  markets, 601 at 16 and 7,054 at 255: 2, 4.8 and 56 times the assumption,
  against the fee's tenfold slack. A per-byte price can wait while the markets
  are few, and cannot at 255.

## The scheduler

- The header looks right to its author, who will know for sure on integrating
  it. Resuming from the five numbers is expected to work after the port.
- libdex's scheduler asserts on engine state, which can go, and calls the
  engine's `validate_user_op` to decide whether a user op is skipped.
- The scheduler's state lives in process memory, outside the engine's state.
- `fee_to_linear` is ported and matches bit for bit, in 64 bits. An exponent of
  5,612 or more is unpayable, and its author proposed a reason code for it.

What it means:

- Here the scheduler never asks the engine. The executor compares the nonce
  and execution is total
  ([user nonces](../protocol/application-contract.md#user-nonces)), so
  `validate_user_op` has no place in the port as the contract stands
  ([canonical rules](register.md#canonical-rules-open-for-the-c-port)).
- The guest must answer the `scheduler` query itself
  ([canonical machine](../plans/2026-11-sequencer-recovery/application.md#the-canonical-machine)).
- The engine header already covers an unpayable fee: admission reports
  `APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE` with `required` all ones.

## L1 inputs

- Every user op can also be sent as an L1 input.
- The only other input is a portal's ERC-20 deposit. libdex ignores a
  deposit's extra bytes, and its author asks that a long deposit be trimmed
  and never dropped.

What it means:

- No filter on sender or shape exists: any sender may send any op.
- The scheduler drops a direct over `max_direct_payload_bytes`
  ([the fridge](../protocol/scheduler-semantics.md#the-fridge)). A deposit with
  long extra data is such a direct, and the portal has already taken its
  tokens, so dropping it loses them.

## Accounts and nonces

- Only an ERC-20 deposit of at least `MIN_COLLATERAL` creates an account. A
  user op never does. System accounts, the fee collector among them, exist
  from genesis, and the fee collector may not send user ops.
- When the account table is full, a deposit is refunded at once by a
  withdrawal voucher.
- A session wallet can be deleted and used again. After that the engine's
  nonce and `GET /nonce` can disagree; libdex's clients sign with the engine's.

What it means: libdex cannot follow the nonce rule. The rule gives every
address a nonce from 0 that each executed op advances and nothing else
changes; libdex has no place for the nonce of a signer without an account,
and a deleted wallet's nonce does not survive. If a reused wallet restarts
below where it stopped, its earlier ops, which are public on L1, can run
again; that is to confirm.

## Clocks

libdex has two. The oracle's clock, fed by price pushes, is the one traders
see. The block clock, fed by the safe blocks of frames and directs, detects a
stale oracle and halts trading. libdex reads no timestamp and stores none.

What it means: after an outage the block clock is ahead of the oracle's, so
libdex halts trading until a fresh push. Recovery needs nothing more.

## State

The state is a 512 MiB address range, memory-mapped, in a layout Boost's
containers define, and mostly zeros. Its first 4 MiB hold the table an
emergency withdrawal reads. The L2 tx count is inside, named
`executed_input_count`.

What it means: setup reads a base machine's state only through an inspect
report of at most 2 MiB, so it cannot read a libdex base until it reads the
range ([todo](../plans/2026-11-sequencer-recovery/todo.md#build)). The native
engine must lay out the same bytes as the guest, on every host a recovery
runs on; that is to confirm.

## Machine cycles

- Placing an order costs by how far it crosses the book: a large market
  order matches hundreds of orders, a resting limit order may match none.
- A deposit is a few checks.
- A price push is the most expensive op, since it advances a tick:
  liquidations, pegged orders and socialized losses. An idle tick is linear
  in the accounts; the worst case is many billions of cycles.

What it means: a push can arrive as a direct, so a drain of the fridge can
hold as many ticks as the fridge has entries. At ten billion cycles a tick,
an assumed figure, about 28,000 ticks fit one input's budget, under twice the
wallet's provisional bound of 16,384 entries. libdex's bound has to be priced
by its worst direct.

## The client

- The backend calls `/history` and `/ws/subscribe`, the frontend `/tx`, the
  price pusher `/readyz` and `/tx`.
- The indexer cannot go back to an earlier count and has no backups yet.
  After a recovery it reprocesses from genesis, which takes long. With backups
  its author guesses minutes to reprocess a few hours.
- It links the SQLite 3.53.4 amalgamation statically.

What it means: the list names no caller of `/nonce`, `/fee` or `/domain`. A
libdex recovery's time includes a full reindex until the indexer has
[backups](../plans/2026-11-sequencer-recovery/client-backups.md).
