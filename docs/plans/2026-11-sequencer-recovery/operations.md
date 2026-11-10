# Recovering the sequencer

**Status:** proposed on 2026-11-05; nothing here is implemented. It needs the
next template ([scheduler](scheduler.md)), so today's deployments keep
[cockroach recovery](../../recovery/cockroach.md) and today's
`setup --recovery`.

A recovery starts a new sequencer from a canonical machine, when the old one
is gone or cannot be trusted: it had a bug, it served transactions that never
landed, its SQLite is corrupt, its disk failed, or a fridge overflow made it
diverge. The watchdog reports most of these as a divergence. Standard
recovery, which the sequencer runs by itself after a liveness failure, is a
different procedure ([automatic recovery](../../recovery/README.md)).

## One setup, from any base

```text
sequencer setup [--base <MACHINE_DIR>] [--dry-run]
```

Every setup starts from a **base** and rebuilds the history from L1 through a
**stop block**. A base is a stored machine of the deployment's template,
stopped between inputs. Without `--base`, the base is genesis: count zero, an
empty scheduler state, and the application's genesis state. A new deployment
and a recovery run the same procedure.

**Recovery is needed exactly when L1 shows the old submitter's activity past
the base:** a submitter input after the base's last input, or unsettled
transactions in the submitter's wallet. Without that activity, the rebuild only
moves directs. With it, the rebuild re-judges the old instance's batches.
Setup does the same either way, and the dry run says which case it found.
Which base to use is the operator's call: setup accepts any base, genesis
included, though a rebuild from genesis takes longer as the deployment ages.

### Inputs

| Input | Notes |
|---|---|
| A base machine | Optional, genesis without it; setup trusts it, and the operator checks it ([before anything](#before-anything)) |
| The submitter key | Setup flushes the old instance's transactions with it |
| An empty data directory | Setup refuses one that holds anything |
| Today's setup configuration | The RPC endpoint, chain, application address and fee source |

Setup takes no client count; each client checks its own count
([the client's playbook](#the-clients-playbook)).

### Outputs

- **The dry run** prints the base's L1 input count and L2 tx count, the
  submitter's inputs past the base, the wallet's unsettled transactions, and
  how many inputs the rebuild would replay. It changes nothing.
- **Setup** prints the same, then the stop block and the L2 tx count at its end.
- **The new era**, which setup publishes:

| Field | Value |
|---|---|
| Base count | The base's L2 tx count, where the era's history starts |
| Stop block | Where the rebuilt history ends |
| Rebuilt history end | The L2 tx count at the end of the stop block |
| Next batch nonce | The scheduler's, at the end of the stop block |
| First queued input | The InputBox index of the oldest direct still queued at the end of the stop block |
| Baseline | The application's state at the end of the stop block |

`/history` reports the era's identity, its base count and its rebuilt history
end.

### What setup does

Setup needs an empty data directory. It refuses one that holds anything, a
completed setup or a failed one, loudly and without touching it: nothing there
can be trusted to fit this run.

1. **Reads the base.** For a machine, setup loads it, reads its
   [export](application.md#the-state-export), and builds the sequencer's
   application from the export. For genesis, it starts empty.
2. **Flushes and pins the stop block.** It replaces what the provider's pool
   holds for the submitter account and waits until that is safe
   ([flush](../../recovery/cockroach.md#flush-and-stopping-block)). The flush is
   best effort, and on a settled wallet there is nothing to replace. The stop
   block is a safe block observed after it.
3. **Fills the input table.** It syncs every L1 input from the deployment
   block to a safe head at or past the stop block. Then it checks for an
   escape: it refuses if a submitter input lies past the stop block or the
   wallet is still unsettled. Either means a transaction escaped the flush or
   the old instance still runs; stop it and run setup again. This is setup's
   one check on the old instance.
4. **Checks the base's export against L1.** Among the inputs the base
   consumed, the admitted directs above its clock must be exactly the ones its
   export names: as many as it reports queued, starting at its first queued
   input ([the invariant](scheduler.md#the-invariant)). A mismatch stops
   setup: the machine is not canonical, or its scheduler drained wrongly.
5. **Rebuilds the history, with the scheduler alone.** It resumes the
   scheduler from the base's scheduler state, with the directs the base still
   had queued, and feeds it every input the base has not consumed, through the
   stop block. It stores each transaction the scheduler hands out, in order,
   in a new table. No application runs here.
6. **Executes the history.** The application advances through the stored
   transactions, through the shared execution boundary. A direct always
   executes; a user op executes or is skipped
   ([what counts](scheduler.md#what-counts)). An application error stops setup
   there. If the canonical machine halts on the same transaction, the
   application is dead on chain; otherwise the fault is in the native engine or
   the projection ([what can go wrong](#what-can-go-wrong)).
7. **Publishes the new era**, in one transaction. It opens no root frame.

Nothing is published until step 7. A failed setup leaves a directory to
discard; run setup again in an empty one.

### After setup

- **`run` opens the root frame** at its own safe head. That frame first
  executes the directs still queued at the stop block, from the first queued
  input on, as the canonical scheduler's first frame does.
- **The watchdog compares the baseline.** The application's state at the end
  of the stop block is the canonical state there, so the sequencer serves it
  as the era's first comparison checkpoint, and the watchdog compares it on
  its next tick, once no divergence latch holds it.
- **The feed serves the new era from the base count**: the rebuilt history
  first, then the live rows.

## Before anything

- **The old instance stays down.** The new data directory is pinned to the
  same submitter address. The flush settles transactions, not processes, so no
  command that signs with the key may run against the old directory again
  ([restart policy](../../watchdog/operator-deployment.md#sequencer-restart-policy)).
- **The base machine must be canonical.** Setup trusts it, so the operator
  checks where it came from. A machine from our validator node at a settled
  epoch can be checked against the root on chain. Sourcing is a
  [separate task](todo.md#machine-sourcing).
- **Recovery repairs only the sequencer.** What the canonical machine executed
  stays executed, a faulty sequencer's accepted batches included. A canonical
  machine that halted cannot be recovered.

## The operator's playbook

1. **Stop the old instance**, and keep it down.
2. **Ask each client for a state it trusts**, as an L2 tx count. For Bart,
   that is the count of a backup he knows is right.
3. **Pick a base** at or below every client's count: a machine, or genesis.
   `setup --base <machine> --dry-run` prints its counts.
4. **Run `sequencer setup --base <machine>`** in an empty data directory.
5. **Run `sequencer run`.**
6. **Tell each client to realign** at its own count.
7. **Clear the watchdog's latch, then watch its next comparison.** If a
   divergence latched it, clear it once `run` has started
   (`sequencer-watchdog clear --block <B> --reason ...`, as in the
   [incident runbook](../../watchdog/incident-runbook.md)). A mismatch on the
   new era's first comparison means the base machine was not canonical.

## The client's playbook

1. **Restore its trusted state**, at its count ([client backups](client-backups.md)).
2. **Read `/history`** for the new era's identity, base count and rebuilt
   history end.
3. **Check its count against that range.** Below the base count, the base is
   newer than the client's state, so the operator must recover from an older
   machine. Past the rebuilt history end, the client's state holds rows the old
   instance served that never landed by the stop block, so it is not
   canonical: restore an older state.
4. **Optionally check its rows.** Subscribe from the base count and compare
   each row up to its own count with the row it stored. Any difference means
   its state is not canonical.
5. **Subscribe to the new era at its count** and follow the feed as always.
   Today the feed refuses a claim from another era, so this step needs a
   protocol change ([todo](todo.md#changes-to-carry-through)).

## What can go wrong

| Problem | Seen where | Remedy |
|---|---|---|
| The base is newer than a client's state | The client's range check, or its subscription refused as history unavailable | Recover again from an older machine |
| A client's state holds rows that never landed | The client's range check | The client restores an older state |
| A client's state is wrong below its count | Only the client's own checks, such as the row comparison | The client restores an older state |
| A transaction escaped the flush, or the old instance runs | Setup's escape check | Stop the old instance and run setup again |
| The base's export disagrees with L1 | Setup's export check | Recover from a trusted machine; if this one is canonical, its scheduler drained wrongly |
| The base machine is not canonical | The new era's first watchdog comparison | Recover from a trusted machine |
| An I/O error while executing the history | Setup stops | Fix the host and run setup again |
| The application fails on a transaction of the history | Setup stops | Replay a trusted machine to that input: if it halts there too, the application is dead; if not, fix the native engine or the projection |
