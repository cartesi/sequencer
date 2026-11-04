# Sequencer recovery

**Status:** third plan, proposed on 2026-11-03 from the owner's procedure;
nothing here is implemented. If adopted, it supersedes the
[shared recovery tool](../2026-10-shared-recovery-tool.md) and its
[candidate](../2026-11-recovery-from-a-machine.md), which can then be archived.
Like them, it needs the next template ([template rules](template.md)), so
today's deployments keep [cockroach recovery](../../recovery/cockroach.md) and
today's `setup --recovery`.

## The scenario

The sequencer is gone. It had a bug, it served things that never landed, its
SQLite is corrupt, or its disk failed. We start from zero, with the old
instance kept down.

1. **Ask each client for a state it trusts.** Bart names an L2 tx count: the
   count of a backup he knows is right. A count is the only name a client has
   for a state.
2. **Pick a base machine**, a Cartesi machine image whose L2 tx count is at or
   below every client's. By default it comes from our validator node at a
   settled epoch, which the operator can check on chain.
3. **Run `sequencer setup --recovery <machine>`.** It takes the machine and
   nothing else.
4. **Run `sequencer run`.**
5. **Each client subscribes at its own count**, on the feed it already uses.

## The ideas

- **The machine exports its whole state.** Setup reads the machine's
  application state and its scheduler state (the next batch nonce and the
  fridge of queued directs) through inspect queries. Nothing below the
  machine is rebuilt.
- **The scheduler alone rebuilds the history.** From the machine's scheduler
  state and the L1 inputs after it, the scheduler decides which transactions
  run, in order, without the application. The application then advances
  through them, and each one it executes gets its L2 tx count.
- **Clients get the history through the feed.** The new era serves rows from
  the base machine's count: the rebuilt history first, then the live rows.
  There is no file, no separate tool and no special client mode.
- **A client's count is checked where it is used.** Setup never sees it. A
  client below the machine's count gets an error when it subscribes.

## Files

| File | What it holds |
|---|---|
| [definitions.md](definitions.md) | The L2 transaction sequence, the two counts, batch validity, and the names this plan uses; drafts for the real docs |
| [procedure.md](procedure.md) | The operator's steps, what setup does, what a client does, and what can go wrong |
| [interfaces.md](interfaces.md) | Rust outlines: the command, the machine export, the scheduler, the executor, the history table and the feed |
| [application.md](application.md) | Everything an application and its canonical machine must provide |
| [template.md](template.md) | The next template's rules: the one this plan needs, those that ride along, the open ones and the dropped ones |
| [client-backups.md](client-backups.md) | How a client keeps states it can restore, with notes on large SQLite databases |
| [todo.md](todo.md) | The changes to carry through implementation, remaining work, docs to promote, open questions and later reviews |

## Decisions

The owner settled these on 2026-11-03:

| Decision | Choice |
|---|---|
| Interface | `setup --recovery` takes a machine image; setup extracts everything from it |
| Scheduler state | The machine exports it, the fridge included, through paged inspect queries |
| History | Rebuilt by the scheduler alone, stored in a new table, then executed by the application during setup |
| Clients | Realign through the existing feed, from their own count |
| The shared tool | Gone; only `setup --recovery` remains |
| A client's count | Not an input to setup |
| Machine sourcing | The operator's job, by default from a validator node; a separate task |

These stay open:

| Decision | Recommendation |
|---|---|
| A user op whose sender cannot pay the fee | Skipped, as today; the authorization rule would include it instead ([definitions](definitions.md#what-counts)) |
| The floor gate | Keep only if libdex reads blocks as time; recovery does not need it |
| A direct row's batch nonce, when the backstop or an overflow drained it | Optional on the feed |
| Checking a client's count against the end of the rebuilt history | Client-side for now; revisit with the egress review |
| How setup loads a machine | Link the emulator, as `sdk/guest/testsi` does |
