# Sequencer recovery

**Status:** proposed on 2026-11-03 from the owner's procedure and revised on
2026-11-05; nothing here is implemented. It supersedes two
[archived plans](../../recovery/history/README.md#superseded-cockroach-recovery-plans).
It needs the next template, so today's deployments keep
[cockroach recovery](../../recovery/cockroach.md) and today's
`setup --recovery`.

## The scenario

The sequencer is gone. It had a bug, it served things that never landed, its
SQLite is corrupt, or its disk failed. We start from zero, with the old
instance kept down.

1. **Ask each client for a state it trusts.** Bart names an L2 tx count: the
   count of a backup he knows is right. A count is the only name a client has
   for a state.
2. **Pick a base machine**, a Cartesi machine whose L2 tx count is at or below
   every client's. By default it comes from our validator node at a settled
   epoch, which the operator can check on chain.
3. **Run `sequencer setup --base <machine>`**, then `sequencer run`.
4. **Each client realigns at its own count**, on the feed it already uses.

## The ideas

- **The machine exports its whole state.** Setup reads the application's state
  and the scheduler's state, the fridge of queued directs included, through
  inspect queries. Nothing below the machine is rebuilt.
- **The scheduler alone rebuilds the history.** From the machine's scheduler
  state and the L1 inputs after it, the scheduler decides which transactions
  run, in order, without the application. The application then executes them.
- **Setup is one procedure from any base.** A new deployment starts from
  genesis, a recovery from a machine. Recovery is needed exactly when L1 shows
  the old submitter's activity past the base.
- **Clients realign through the feed.** The new era serves rows from the base
  count: the rebuilt history first, then the live rows. There is no file, no
  separate tool and no special client mode.
- **A client's count is checked where it is used.** Setup never sees it.

## What to read

Three layers, each readable on its own:

| Layer | For | Files |
|---|---|---|
| 1. The shared software | Whoever writes the guest or ports the scheduler | [scheduler.md](scheduler.md), every check and gate; [application.md](application.md), the state export and the `Application` trait |
| 2. Operations | Whoever runs the sequencer or a client | [operations.md](operations.md), the interface and the playbooks; [client-backups.md](client-backups.md) |
| 3. The design | Whoever builds or reviews the sequencer's side | [sequencer.md](sequencer.md), the rebuild, the history table and the feed; [todo.md](todo.md) |

Each file becomes a real doc when the plan lands:
[scheduler semantics](../../protocol/scheduler-semantics.md), the
[application contract](../../protocol/application-contract.md) and its
[C binding](../../protocol/c-application-binding.md), and
[cockroach recovery](../../recovery/cockroach.md) with the
[incident runbook](../../watchdog/incident-runbook.md).

## Names

| Name | Meaning |
|---|---|
| L2 tx count | How many L2 transactions a state has executed; it names the state ([scheduler](scheduler.md#what-counts)) |
| base | The state a setup starts from: a machine, or genesis |
| base count | The base's L2 tx count, where the new era's history starts |
| stop block | The safe L1 block setup pins after the flush; the rebuilt history ends with it |
| rebuilt history | The L2 transactions from the base count through the stop block, rebuilt from L1 |
| scheduler state | The next batch nonce, the scheduler clock and the fridge |
| scheduled transaction | A transaction the scheduler hands the application; a user op may still be skipped |
| executed transaction | A scheduled transaction the application executed; it has an L2 tx count |
| first queued input | The InputBox index of the oldest direct still queued at the end of the stop block |

## Decisions

The owner settled these on 2026-11-03:

| Decision | Choice |
|---|---|
| Interface | Setup takes a machine and extracts everything from it |
| Scheduler state | The machine exports it, the fridge included, through paged inspect queries |
| History | Rebuilt by the scheduler alone, stored in a new table, then executed by the application during setup |
| Clients | Realign through the existing feed, from their own count |
| The shared tool | Gone; only setup remains |
| A client's count | Not an input to setup |
| Machine sourcing | The operator's job, by default from a validator node; a separate task |

And these on 2026-11-05:

| Decision | Choice |
|---|---|
| Setup | One procedure from any base, genesis included; it never refuses on intent |
| An underfunded user op | Executes and counts; the application decides what it charges |
| Blocks | Monotone along the L2 sequence: a scheduler clock, a gate on it, and strict frames |
| Overflow | Drains whole older blocks; a direct joins over the bound when only its own block is left |
| State export | Generic inspect queries; the application's part through the trait |
| The scheduler's interface | Designed later with Bart, as the application's trait and C binding were |
| Sharing | The repository, which Bart's team clones |

These stay open:

| Decision | Recommendation |
|---|---|
| Which of a direct's index, timestamp and prevRandao reach the application | Index and timestamp; prevRandao only on a stated need |
| The fridge bound, an entry's largest size and the largest drain | Settle with Bart and measure |
| A direct row's batch nonce, when the backstop or an overflow drained it | Optional on the feed |
| Checking a client's count against the rebuilt history end | Client-side for now; revisit in the egress redesign |
| How setup loads a machine | Link the emulator, as `sdk/guest/testsi` does |
| The rules already open with the C++ port | Settle with Bart ([register](../../review/register.md#canonical-rules-open-for-the-c-port)) |
