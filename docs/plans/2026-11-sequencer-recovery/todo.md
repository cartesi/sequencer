# To do

## Changes to carry through

Each one changes what today's code, docs, clients or Bart's guest do, and each
is easy to miss. An item leaves this list when it lands.

- **A client adopts the new era at its own count.** Today a claim must carry
  the server's exact era and generation, and equal counts never authorize
  crossing eras ([identity](../../protocol/application-history.md#identity-and-available-history)).
  After this recovery, a client whose count passes the range check claims the
  new era at that count, and the history protocol must say so
  ([client's playbook](operations.md#the-clients-playbook)).
- **`/history` reports where the rebuilt history ends, and a client checks its
  count against it.** A state past that end holds rows the old instance served
  that never landed. The feed checks a claim only against the era's start and
  its head, and the head moves past the rebuilt end as soon as `run` executes
  new transactions. The [egress redesign](#redesign-the-egress-api) decides
  whether the sequencer refuses such a client itself.
- **The client guide says what the count checks miss.** A machine that is not
  canonical shows only at the watchdog's first comparison. A client state that
  is wrong below its count shows only in the client's own row comparison
  ([what can go wrong](operations.md#what-can-go-wrong)).
- **A direct row's `batch_nonce` becomes optional on the feed.** A direct the
  backstop or an overflow drained belongs to no batch. Every feed consumer
  changes with it: the OpenAPI spec, the SDK and Bart's client
  ([the feed](sequencer.md#the-era-and-the-feed)).
- **Each queued direct keeps its input index**, in the guest and in the
  `fridge` export. The feed takes a direct's transaction hash and timestamp
  from the input table by that index, and the lane starts draining at the first
  queued input ([export](application.md#the-state-export)).
- **The rebuilt history table keeps every scheduled transaction, and the feed
  serves only the executed ones.** A skipped user op stays in the table without
  a count ([table](sequencer.md#the-rebuilt-history-table)).
- **Setup links the emulator**, to load the machine and run its export
  queries. Today only `sdk/guest/testsi` links it, so the sequencer's build and
  image grow ([loading a machine](sequencer.md#loading-a-machine)).
- **An underfunded op executes and counts.** Today it is skipped. Validation
  reports it apart from a wrong nonce, `apply_valid_user_op` must never fail on
  it (the wallet treats an unpayable fee as fatal today), and the lane refuses
  it at `POST /tx`. The application contract's
  [§2](../../protocol/application-contract.md#2-replay-safety--rejection-inclusion-and-failure)
  changes with it ([frame execution](scheduler.md#frame-execution)).
- **Setup becomes one procedure.** `--recovery` and its dump directory go;
  plain setup's refusal on submitter activity goes, and its post-flush escape
  check stays; setup takes the submitter key; genesis is a base like any other
  ([operations](operations.md#one-setup-from-any-base)).

## Promote to real docs

- **[scheduler.md](scheduler.md)** replaces the rules in
  [scheduler semantics](../../protocol/scheduler-semantics.md), whose
  sequencer-side sections stay. The L2 transaction sequence and the L2 tx
  count also go into the
  [application contract's §4](../../protocol/application-contract.md#4-canonical-history-cursor-executed_input_count).
- **[application.md](application.md)** goes into the application contract and
  the [C binding](../../protocol/c-application-binding.md).
- **[operations.md](operations.md)** replaces the procedure in
  [cockroach recovery](../../recovery/cockroach.md) and the
  [incident runbook](../../watchdog/incident-runbook.md); then this directory
  keeps only remaining work.
- **Two counts and their names**: rename the executed-input count to the L2 tx
  count across the docs, then in code (`executed_input_count`).

## Build

The changes above are part of this work; each item here names only the rest.

- **The scheduler's interface**: design with Bart a free-standing scheduler's
  C interface and its Rust wrapper, with examples, as the `Application` trait
  and the C binding were ([scheduler](scheduler.md#the-schedulers-interface)).
  Then split today's `Scheduler<A>` into the scheduler and the executor
  ([rebuild](sequencer.md#the-rebuild)).
- **Guest**: the template rules and the export queries.
- **Setup**: `--base` and `--dry-run`. It loads the machine, runs its export
  queries, builds the application from the export, rebuilds and stores the
  history, executes it, and publishes the era.
- **Sequencer storage**: the rebuilt history table; the era's base count,
  rebuilt history end and first queued input; the new baseline served as the
  era's first comparison checkpoint.
- **The lane and the predicate**: the direct filter, the underfunded refusal,
  and gate f in `scheduler_accepts` ([sequencer](sequencer.md#the-lane-and-the-predicate)).
- **Feed**: serve the rebuilt history, then the live rows, switching tables
  internally.
- **Tests of setup against a chain with history**, from genesis and from a
  machine, and of its escape check. Today plain setup's refusal is only
  unit-tested (`run_detection_gate`); no test runs setup end to end on a chain
  that already has batches.
- **Drills** that recover from a machine with directs queued at the stop block
  and realign a toy-wallet client, on the devnet in CI. A path run once in a
  deployment's life rots.

## Redesign the egress API

Later, in light of this plan:

- keep the WS subscription;
- consider a paginated HTTP endpoint for history;
- remove the methods nothing uses, starting with what the
  [tracks board](../2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
  lists for deletion, `/history`'s baseline and accepted checkpoint;
- the feed's identity checks (era and generation), `available_from` and the
  rebuilt range, and whether the sequencer refuses a client from an older era
  whose count lies past the rebuilt history end;
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

- Can your guest answer the export queries and keep each queued direct's input
  index? Which encoding will libdex's `state` export use?
- Does libdex's drive hold the L2 tx count, so that a projected state starts at
  the machine's count? Does it hold only the flat buffer, or a padding tail
  too?
- Which of a direct's index, timestamp and prevRandao does libdex read? Does it
  read a transaction's block as time?
- Which directs are well formed for libdex, and how large can one be? What
  bound and largest drain can the guest afford?
- How should libdex's apply handle an underfunded op? We recommend charging
  what the sender has.
- Is your guest's compiled sequencer address the pinned submitter? Can its
  harness answer an input with `RX_REJECTED`?
- Does your scheduler reproduce the conformance vectors, and reject an
  out-of-range `fee_price` at gate d, as ours does since PR #56?
- How should the free-standing scheduler's interface look from your side?
- Can your client restore a state at a count it trusts, by the two backup
  rules or its own? Which SQLite version does it link? Will it compare its rows
  from the base count before it continues?
- What are libdex's per-op cycles and state size? Who fixes libdex during an
  incident?

## Measure

- Setup's rebuild from the base machine to the stop block, at the age a base
  machine will typically have, and from genesis.
- Exporting a full fridge, and a large application state, through paged
  inspect queries.
- The guest's cost to drain a full fridge in one input, which the first frame
  after an outage and the root frame after a recovery both do.
- The time to recover, end to end, in a drill.

## When this plan lands

Delete today's `setup --recovery` fold and what the
[tracks board](../2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
lists for deletion. The earlier plans are already
[archived](../../recovery/history/README.md#superseded-cockroach-recovery-plans).
