# To do

What this plan still needs. The scheduler, the shared executor, the
`Application` trait, the state export, `setup [--base] [--dry-run]`, the
rebuilt history and the feed's realignment are code in this tree.
[Cockroach recovery](../../recovery/cockroach.md) states the procedure and the
playbooks, and the
[history contract](../../protocol/application-history.md#era-baseline) the
era, the rebuilt history and the adopt rule. The
[register](../../review/register.md#sequencer-recovery-follow-ups) holds the
limits this work accepts and the paths it left unverified.

## Promote to real docs

- **[scheduler.md](scheduler.md)**: [scheduler semantics](../../protocol/scheduler-semantics.md)
  states the rules the reference runs, so this page shrinks to its open items,
  as [operations](operations.md) and [sequencer](sequencer.md) have.
- **[application.md](application.md)**: the
  [application contract](../../protocol/application-contract.md) and the
  [C binding](../../protocol/c-application-binding.md) state the trait, the
  export and the projection; this page shrinks to the guest's open duties and
  the questions below.
- **[interfaces.md](interfaces.md)** is code, except the outline of the
  scheduler's C header, which stays until a foreign scheduler links through
  it ([build](#build)).

## Build

- **A foreign scheduler in the rebuild.** An application's author may bring
  their own scheduler: one library, compiled into their guest, and natively
  behind [`scheduler.h`](../../../bindings/c-scheduler/include/scheduler.h)
  for setup's rebuild, as libdex will. The reference scheduler is then the
  specification's implementation and the wallet's scheduler, and its export
  is the header's reference. Setup calls the Rust `Scheduler` directly today
  ([`rebuild.rs`](../../../sequencer/src/commands/setup/rebuild.rs)); it needs
  a `Scheduler` trait and an adapter over the header, linked as an engine's
  archive is ([C engine](../../../bindings/c-app-engine/README.md)). The lane
  still predicts the canonical scheduler in Rust, so a foreign scheduler is
  held to the [conformance vectors](../../protocol/scheduler-semantics.md#conformance)
  through `c-scheduler`'s `tests/conformance.rs`, which today links only the
  reference export; package it so an author links their own archive in one
  command.
- **A base machine's state through the range source.** Setup reads only the
  `state` inspect report, which must fit 2 MiB; add the labeled-range read the
  watchdog uses before a deployment's state can outgrow it, and measure its
  queries against the loader's cycle cap
  ([register](../../review/register.md#sequencer-recovery-follow-ups)).
- **Drills that are not tests yet.** The round trips from genesis and from a
  stored machine run on the devnet for both hosts
  ([drills](../../watchdog/incident-runbook.md#drills)). No test clears a
  watchdog latch around a rebuild, or realigns a reader other than the toy
  wallet. A path run once in a deployment's life rots.

## Redesign the egress API

Later, in light of this plan:

- keep the WS subscription;
- consider a paginated HTTP endpoint for history;
- remove the methods nothing uses, starting with what the
  [tracks board](../2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
  lists to reshape or retire, `/history`'s baseline and accepted checkpoint;
- the feed's identity check (the era), `available_from` and the
  rebuilt range, and whether the sequencer refuses a client from an older era
  whose count lies past the rebuilt history end, which today only the client's
  own adopt check catches;
- `/nonce` after a recovery, which reads only the live rows;
- the L2 tx count's older name, `executed_input_count`, which the HTTP fields
  and the `X-Executed-Input-Count` header still carry (the storage columns say
  `l2_tx_count`);
- the WS headers.

## Later

- **A metrics endpoint.** The sequencer exports no metrics: an operator reads
  logs and `/healthz`. Design one on the internal listener, beside the other
  internal routes, starting from the two numbers `/healthz` reports
  ([what the sequencer does](../../recovery/README.md#what-the-sequencer-does)).
- **A batch proxy, to rotate the submitter's key.** The canonical machine
  compiles in the address that posts batches, so the key behind that address
  can never change. A proxy contract would stand at it: the proxy forwards a
  batch to the InputBox from one configured hot wallet, the sequencer's, and a
  cold multisig can replace that wallet. The machine then pins the proxy,
  which never changes. A rotation may still need a cockroach recovery; that is
  to work out.

## Machine sourcing

A separate task. Setup only takes a machine; finding a trusted one is the
operator's job.

- **From a validator node at a settled epoch**, so its root can be checked on
  chain. Review our sling node: does it keep the machines at the end of past
  epochs, or only the newest?
- **A playbook** for finding a machine at or below a given L2 tx count, and for
  checking it.
- **A backup procedure for machines.** Storing directory trees for a long time
  is a solved problem; pick a mature tool rather than building one.

## Questions for Bart

The guest's duties are stated in [application](application.md#the-canonical-machine),
and what an op pays is the application's choice, with charging what the sender
has as the recommendation; neither is a question.

To confirm:

- Price pushes are the only large op: about 165 bytes with three markets,
  sent every few minutes by wallets the operator runs, against about 32 bytes
  for the rest. How many bytes does a market add, and how many markets are
  planned?
- Official pushers should pay nothing, and a redundant push should not be
  included at all. `application_engine_execute_user_op` decides what an op
  pays, and `application_engine_admit_user_op` can refuse one with
  `APPLICATION_ENGINE_REFUSED_BY_POLICY`, which costs its sender no nonce and
  no fee. Does that cover it? Is pushing open to anyone, or to a list?
- Which revision of the sequencer, and which version of
  `application-engine.h`, is libdex built against?

Your scheduler in a recovery, which the sequencer runs natively through
`scheduler.h`, apart from your engine:

- Can your scheduler library implement
  [`scheduler.h`](../../../bindings/c-scheduler/include/scheduler.h) as
  written? What would you change?
- Does your scheduler read anything from the engine's state?
- Can it resume from five numbers and the L1 inputs still queued: the inputs
  consumed, the next batch nonce, the clock, the index of the oldest queued
  direct, and how many are queued? Does it keep any other state?
- Have you ported `fee_to_linear` bit for bit? Would a checked-in table of
  exact values be easier to match?

libdex:

- Which L1 inputs does libdex act on, by sender and by payload shape, and how
  long is the longest valid one? The scheduler could drop every other direct
  before it queues it.
- How does an account come to exist: by a deposit only, or by a first user op
  too? What happens to each when the account table is full?
- What does libdex read as time: a direct's block or timestamp, a user op's
  frame block, or the price pusher's own timestamp?
- The state: its encoding, its size now and at full scale, whether the drive
  holds only the flat buffer or a padding tail too, and whether the L2 tx
  count is inside it.
- How many machine cycles do a small op, a price push and a deposit take?

Your client:

- Which endpoints and headers does your indexer call, and what is missing or
  awkward?
- After a recovery the history can differ from what the indexer saw, from
  some count on. Can it go back to an earlier count from its own backups
  ([client backups](client-backups.md)), or does it rebuild from zero, and how
  long does that take? Which SQLite version does it link?

## Measure

The [recovery measurements](../../review/2026-11-10-recovery-measurements.md)
give setup's rebuild rate, the cost of reading a base machine, the wallet's
largest drain and the drills' times. What they leave out:

- **The L1 sync** through a deployment's RPC provider, the one part of
  setup's time the provider bounds.
- **libdex's numbers**: its execution rate in setup, its cost per drained
  direct, and its `state` size, since every guest number there is the
  wallet's ([questions](#questions-for-bart)).
- **Production-like hardware.** One Apple machine on macOS ran every measurement;
  re-measure the rebuild on the recovery host's Linux storage.
