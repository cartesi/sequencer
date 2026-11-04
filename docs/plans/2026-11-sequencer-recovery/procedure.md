# The procedure

## Before anything

- **The old instance stays down.** Recovery creates a second data directory
  pinned to the same submitter address. The flush settles transactions, not
  processes, so no command that signs with the key may run against the old
  directory again
  ([restart policy](../../watchdog/operator-deployment.md#sequencer-restart-policy)).
- **The base machine must be canonical.** Setup trusts it; the operator checks
  where it came from. A machine from our validator node at a settled epoch can
  be checked against the root on chain. Sourcing is a
  [separate task](todo.md#machine-sourcing).
- **Recovery repairs only the sequencer.** What the canonical machine executed
  stays executed, a faulty sequencer's accepted batches included. A canonical
  machine that halted cannot be recovered.

## The operator's steps

1. **Ask each client for a state it trusts**, as an L2 tx count. For Bart, that
   is the count of a backup he knows is right.
2. **Pick a base machine** whose L2 tx count is at or below every client's
   count. `setup --recovery --dry-run <machine>` prints the machine's counts
   and stops.
3. **Run `sequencer setup --recovery <machine>`** in a fresh data directory. It
   needs the submitter key, for the flush.
4. **Run `sequencer run`.**
5. **Tell each client to subscribe** to the new era at its own count.

## What `setup --recovery` does

1. **Reads the machine.** It loads the image and asks its scheduler for the L1
   input count and the scheduler state, paging through the fridge. It reads
   the application state and builds the sequencer's application from it. It
   prints the machine's L1 input count and L2 tx count.
2. **Flushes and pins the stop block.** It settles the old instance's
   transactions, best effort: it replaces what the provider's pool holds for
   the submitter account and waits until that is safe
   ([flush](../../recovery/cockroach.md#flush-and-stopping-block)). The stop
   block is a safe block observed after that.
3. **Fills the input table.** It syncs every L1 input from the deployment
   block to a safe head at or past the stop block, as setup does today. Then it
   applies genesis setup's detection gate at the stop block: it refuses if a
   submitter input lies past it or the submitter's wallet nonce is unsettled.
   Either means a transaction escaped the flush or the old instance still runs;
   stop it and run setup again.
4. **Rebuilds the history, with the scheduler alone.** It resumes the scheduler
   from the machine's scheduler state and feeds it every input the machine has
   not consumed, through the stop block. It stores each
   transaction the scheduler hands out, in order, in a new table. No
   application runs in this step.
5. **Advances the application** through the stored transactions, through the
   shared execution boundary. Each transaction either executes and takes the
   next L2 tx count, or is a user op the application rejects. An application
   error stops setup there: the canonical machine halts on the same
   transaction.
6. **Publishes the new era**, in one transaction: the base count, where its
   history starts; the stop block, and the count there, where its rebuilt
   part ends; the next batch nonce; the first queued input; the application's
   state at the end of the stop block; and setup completion. It opens no root
   frame.

Nothing is published until step 6, and step 3 refuses before then, so a failed
setup leaves a directory to discard and run again.

## After setup

- **`run` opens the root frame** at its own safe head. That frame first
  executes the directs still queued at the stop block, from the first queued
  input on, as the canonical scheduler's first frame does.
- **The watchdog compares the new baseline.** The application's state at the
  end of the stop block is the canonical state there, so the sequencer serves
  it as the era's first comparison checkpoint, and the watchdog compares it on
  its next tick.
- **The feed serves the new era from the base count**: the rebuilt history
  first, then the live rows. The switch between the two tables is internal.

## What a client does

1. **Restores its trusted state**, at its count ([client backups](client-backups.md)).
2. **Reads `/history`** for the new era's identity and range: where its history
   starts (the base count) and where the rebuilt part ends (the count at the
   end of the stop block).
3. **Checks its count against that range.** Below the start, the machine is
   newer than the client's state: the operator must recover from an older
   machine. Past the end of the rebuilt part, the client's state holds rows the
   old instance served that never landed by the stop block, so it is not
   canonical: restore an older state.
4. **Optionally checks its rows.** It subscribes from the base count and
   compares each row up to its own count with the row it stored. Any
   difference means its state is not canonical.
5. **Subscribes to the new era at its count** and follows the feed as always.

## What can go wrong

| Problem | Seen where | Remedy |
|---|---|---|
| The machine is newer than a client's state | the client's subscription is refused: history unavailable | recover again from an older machine |
| A client's state holds rows that never landed | the client's range check | the client restores an older state |
| A client's state is wrong below its count | only the client's own checks, such as the row comparison | the client restores an older state |
| A transaction escaped the flush, or the old instance runs | setup's detection gate | stop the old instance and run setup again |
| The machine is not canonical | the new era's first watchdog comparison | recover from a trusted machine |
| The application errors while advancing | setup stops | none: the canonical machine halts there too |
