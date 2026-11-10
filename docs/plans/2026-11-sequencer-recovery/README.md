# Sequencer recovery

**Status:** proposed; nothing here is implemented. It supersedes two
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

- **The machine exports its state.** Setup reads the application's state and
  the scheduler's: its counts, its batch nonce, its clock and where its queue
  of directs starts. L1 holds the queued directs themselves. Nothing below the
  machine is rebuilt.
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
| 1. The shared software | Whoever writes the guest or ports the scheduler | [scheduler.md](scheduler.md), every check and gate; [application.md](application.md), the state export and the `Application` trait; [interfaces.md](interfaces.md), both components' signatures in Rust and C |
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
| first queued input | The InputBox index of the oldest direct still queued, at the base and again at the end of the stop block |
