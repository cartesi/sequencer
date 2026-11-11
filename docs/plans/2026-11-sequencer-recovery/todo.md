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

## One recovery

[outages.md](outages.md) plans deleting the standard recovery: what the
sequencer does when batches stop landing, the playbooks, what goes, the order
of work and what is [still to decide](outages.md#still-to-decide). It also
takes the recovery generation off every client interface, which the
[egress redesign](#redesign-the-egress-api) below then need not do.

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
  scheduler's C header, which stays until its direction is settled
  ([build](#build)).

## Build

- **The scheduler header's direction.** [`scheduler.h`](../../../bindings/c-scheduler/include/scheduler.h)
  and its reference export are code, tested against the Rust `Scheduler`.
  Settle with Bart whether his guest links that export or his port implements
  the header ([questions](#questions-for-bart)); only the second needs a Rust
  adapter over a foreign scheduler, and a `Scheduler` trait for it. A port is
  held to the [conformance vectors](../../protocol/scheduler-semantics.md#conformance)
  through `c-scheduler`'s `tests/conformance.rs`, which today links only the
  reference export; package it so a port links its own implementation in one
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
- the feed's identity checks (era and generation), `available_from` and the
  rebuilt range, and whether the sequencer refuses a client from an older era
  whose count lies past the rebuilt history end, which today only the client's
  own adopt check catches;
- `/nonce` after a recovery, which reads only the live rows;
- the L2 tx count's older name, `executed_input_count`, which the HTTP fields
  and the `X-Executed-Input-Count` header still carry (the storage columns say
  `l2_tx_count`);
- the WS headers.

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

- Can your guest answer the `scheduler` query and keep each queued direct's
  input index? Which encoding will libdex's `state` export use, and how large
  can it grow?
- Does libdex's drive hold the L2 tx count? The projection is given the
  machine's either way. Does the drive hold only the flat buffer, or a padding
  tail too?
- Which of a direct's index, timestamp and prevRandao does libdex read? Does it
  read a transaction's block as time?
- What is the longest direct payload libdex accepts? It sets the payload cap.
  What bound and largest drain can the guest afford?
- How should libdex's apply handle an underfunded op? We recommend charging
  what the sender has.
- Can libdex report a nonce for an address it has never seen, and store one
  after that address's first op? A bounded account table cannot, and the nonce
  getter would then need a "no such sender" answer that skips the op.
- Would your guest link the reference scheduler through `scheduler.h`, or
  would your port implement `scheduler.h`, so that the sequencer links it for
  the rebuild?
- Is your guest's compiled sequencer address the pinned submitter? Can its
  harness answer an input with `RX_REJECTED`?
- Does your scheduler reproduce the conformance vectors, and reject an
  out-of-range `fee_price` at gate d, as ours does since PR #56?
- Can your client restore a state at a count it trusts, by the two backup
  rules or its own? Which SQLite version does it link? Will it compare its rows
  from the base count before it adopts the new era?
- What are libdex's per-op cycles and state size? Who fixes libdex during an
  incident?

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
