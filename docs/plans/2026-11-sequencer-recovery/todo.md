# To do

## Changes to carry through

Found in the plan's review on 2026-11-03. Each one changes what today's code,
docs, clients or Bart's guest do, and each is easy to miss. An item leaves this
list when it lands.

- **A client adopts the new era at its own count.** Today a claim must carry
  the server's exact era and generation, and equal counts never authorize
  crossing eras ([identity](../../protocol/application-history.md#identity-and-available-history)).
  After this recovery, a client whose count passes the range check below claims
  the new era at that count, and the history protocol must say so.
- **`/history` reports where the rebuilt history ends, and a client checks its
  count against it.** A state past that end holds rows the old instance served
  that never landed. The feed checks a claim only against the era's start and
  its head, and the head moves past the rebuilt end as soon as `run` executes
  new transactions. The [egress review](#review-the-egress-interface) decides
  whether the sequencer refuses such a client itself
  ([procedure](procedure.md#what-a-client-does)).
- **The client guide says what the count checks miss.** A machine that is not
  canonical shows only at the watchdog's first comparison. A client state that
  is wrong below its count shows only in the client's own row comparison
  ([procedure](procedure.md#what-can-go-wrong)).
- **A direct row's `batch_nonce` becomes optional on the feed.** A direct the
  backstop or an overflow drained belongs to no batch. Every feed consumer
  changes with it: the OpenAPI spec, the SDK and Bart's client
  ([interfaces](interfaces.md#the-era-and-the-feed)).
- **Each queued direct keeps its input index**, in the guest and in the
  `fridge` export. The feed takes a direct's transaction hash and timestamp
  from the input table by that index, and the lane starts draining at the first
  queued input ([application](application.md#the-canonical-machine)).
- **The rebuilt history table keeps every scheduled transaction, and the feed
  serves only the executed ones.** A user op the application rejects stays in
  the table without a count
  ([interfaces](interfaces.md#the-rebuilt-history-table)).
- **Setup links the emulator**, to load the machine and run its inspect
  queries. Today only `sdk/guest/testsi` links it, so the sequencer's build and
  image grow ([interfaces](interfaces.md#the-command)).
- **An op whose sender cannot pay the fee is skipped, not counted.** Only a
  business failure, after the fee is paid, counts. The real docs must say so
  when the definitions move there, and change with them if the authorization
  rule is adopted ([definitions](definitions.md#what-counts)).

## Promote to real docs

- **The L2 transaction sequence and the L2 tx count**
  ([draft](definitions.md#the-l2-transaction-sequence)): a new opening section
  of [scheduler semantics](../../protocol/scheduler-semantics.md), and the
  [application contract's §4](../../protocol/application-contract.md#4-canonical-history-cursor-executed_input_count).
- **Batch validity** ([draft](definitions.md#batch-validity)): restate gates a
  to f in scheduler semantics as the checks a batch passes, in order.
- **Two counts and their names**: rename the executed-input count to the L2 tx
  count across the docs, then in code (`executed_input_count`).
- **Application requirements** ([draft](application.md)): the direct filter,
  the projection, the inspect queries and the recorded indices go into the
  application contract and the C binding.
- **As each template rule lands**, update scheduler semantics, whose
  **pending** marks name what is not true yet, and the application contract.
- **When this plan lands**, the procedure moves into
  [cockroach recovery](../../recovery/cockroach.md) and the incident runbook,
  and this directory keeps only remaining work.

## Build

The changes above are part of this work; each item here names only the rest.

- **Scheduler and executor**: split today's `Scheduler<A>` into a scheduler
  that never touches the application and the executor
  ([interfaces](interfaces.md#the-scheduler-without-an-application)). The
  guest, the tests and setup use the pair. Today's `fold_replay` and its
  terminal drain go.
- **Guest**: the `scheduler` and `fridge` inspect queries, and the template
  rules.
- **Setup**: `--recovery <machine>` with `--dry-run`. It loads the machine and
  runs its inspect queries, builds the application from the export, rebuilds
  and stores the history, executes it, and publishes the era. It applies
  genesis setup's detection gate at the stop block.
- **Sequencer storage**: the rebuilt history table; the era's base count,
  rebuilt history end and first queued input; the lane draining from the first
  queued input and dropping malformed directs; the new baseline served as the
  era's first comparison checkpoint, only in an era this recovery built.
- **Feed**: serve the rebuilt history, then the live rows, switching tables
  internally.
- **Drills** that recover from a machine with directs queued at the stop block
  and realign a toy-wallet client, on the devnet in CI. A path run once in a
  deployment's life rots.

## Review the egress interface

Later, in light of this plan: the feed's identity checks (era and generation),
`available_from` and the rebuilt range, whether the sequencer should refuse a
client from an older era whose count lies past the rebuilt history end, the
`/history` baseline fields, the WS headers, and the deletions below.

## Machine sourcing

A separate task. The interface only takes a machine; finding a trusted one is
the operator's job.

- **From a validator node at a settled epoch**, so its root can be checked on
  chain. Review our sling node: does it keep the machines at the end of past
  epochs, or only the newest?
- **A playbook** for finding a machine at or below a given L2 tx count, and for
  checking it.
- **A backup procedure for machines.** Storing directory trees for a long time
  is a solved problem; pick a mature tool rather than building one.

## Questions for Bart

- Can your guest answer the `scheduler` and `fridge` inspect queries, and keep
  each queued direct's input index?
- Does libdex read a transaction's block as time, for example to expire
  orders? That decides the floor gate.
- Does libdex's drive hold the L2 tx count, so that a projected state starts at
  the machine's count? Does it hold only the flat buffer, or a padding tail
  too?
- Which of a direct's index, timestamp and prevRandao does libdex read?
- Which directs are well formed for libdex, and how large can one be?
- Is your guest's compiled sequencer address the pinned submitter? Can its
  harness answer an input with `RX_REJECTED`?
- Does your scheduler reproduce the conformance vectors, and reject an
  out-of-range `fee_price` at gate d, as ours does since PR #56?
- Can your client restore a state at a count it trusts, by the two backup
  rules or its own? Which SQLite version does it link? Will it compare its rows
  from the base count before it continues?
- What are libdex's per-op cycles and state size? Who fixes libdex during an
  incident?

## Measure

- Setup's rebuild from the base machine to the stop block, at the age a base
  machine will typically have.
- Exporting a full fridge through paged inspect queries.
- The guest's cost to drain a full fridge in one input, which the first frame
  after an outage and the root frame after a recovery both do.
- The time to recover, end to end, in a drill.

## Archive the exploration

Once this plan is adopted, archive the
[shared recovery tool](../2026-10-shared-recovery-tool.md) and its
[candidate](../2026-11-recovery-from-a-machine.md). Their settled facts that
still hold already live here or in the register; when the tool lands, delete
what the [tracks board](../2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
lists for deletion, `/history`'s baseline and accepted checkpoint, and today's
`setup --recovery` fold.
