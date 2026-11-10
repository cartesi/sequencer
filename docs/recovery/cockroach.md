# Cockroach recovery

Cockroach recovery starts a new sequencer when the old one is gone or cannot be
trusted: it had a bug, it served transactions that never landed, its SQLite is
corrupt, or its disk failed. The watchdog reports most of these as a divergence
([incident runbook](../watchdog/incident-runbook.md)).
[Standard recovery](README.md) is a different procedure: the sequencer runs it
by itself after a liveness failure, over a database it still trusts.

The operator starts a recovery and one command does the work: `setup`, the
same command that starts a new deployment. Every setup starts from a **base**
and rebuilds the history from L1; a recovery is a setup on a chain that
already has history, usually from a stored canonical machine.

**Find and fix the bug before rebuilding.** The canonical image keeps its own
bugs: a fix to code the image shares makes the sequencer diverge from it, and
nothing but the watchdog's next comparison would notice.

## One setup, from any base

```text
wallet-sequencer setup [--base <MACHINE_DIR>] [--dry-run]
```

Every application binary takes these arguments; `wallet-sequencer` is the
reference. A base is a stored machine of the deployment's template, stopped
between inputs. Without `--base`, the base is genesis: L2 tx count zero, the
scheduler's genesis state, and the binary's genesis application. Setup rebuilds
the history from the base through a **stop block**, a safe L1 block it pins
after flushing the submitter's wallet, and publishes a new **era**: a history
that starts at the base's L2 tx count, holds the rebuilt history through the
stop block, then the rows `run` sequences.

**Recovery is needed exactly when L1 shows the old submitter's activity past
the base**: a submitter input after the base's last input, or unsettled
transactions in the submitter's wallet. Without that activity the rebuild only
moves directs. With it, the rebuild re-judges the old instance's batches by the
canonical scheduler's rules. Setup does the same either way, and its summary
says which case it found. Which base to use is the operator's call: any base
works, genesis included, though a rebuild from genesis takes longer as the
deployment ages.

What the old instance served and never landed by the stop block is gone. Its
user ops are revoked, not cancelled: they carry no expiry, so anyone holding
one can resubmit it while its sender's nonce is unused.

### Inputs

| Input | Notes |
|---|---|
| A base machine | `--base <MACHINE_DIR>`, optional; setup trusts it, and the operator checks it ([before anything](#before-anything)). |
| The submitter key | `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY` or `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE`, as for `run`; it must sign for the submitter address, and `--dry-run` takes none. |
| An empty data directory | Absent, empty, or holding only `sequencer.lock`; setup refuses anything else. |
| The setup configuration | The RPC endpoint, chain id, application address, submitter address and fee source ([Running](../../README.md#running)). |

Setup takes no client count; each client checks its own
([the client's playbook](#the-clients-playbook)).

### Outputs

Setup and its dry run print the same summary:

```text
base: <inputs> L1 inputs, L2 tx count <count>
submitter inputs past the base: <n>
unsettled submitter transactions: <n>
inputs the rebuild replays: <n> (through block <block>)
<whether the rebuild re-judges the old instance's batches>
```

Setup's block is the stop block, and it adds the stop block and the L2 tx count
at its end. The dry run's block is the safe head it synced to.

**The dry run** reads the base, syncs L1 into a scratch directory under the
system's temporary directory, and runs the export check (step 4) there. It
takes no key, flushes nothing and never touches the data directory, and it
reports the escape facts rather than refusing on them. It syncs every input
from the deployment block, so it costs as much time and temporary disk as
setup's own sync.

**Setup** publishes the new era:

| Field | Value |
|---|---|
| Base count | The base's L2 tx count, where the era's history starts. |
| Stop block | The safe block pinned after the flush, stored as `stop_block`. |
| Rebuilt history end | The L2 tx count at the end of the stop block, where the live rows start. |
| First queued input | The InputBox index of the oldest direct still queued at the end of the stop block, or the input count through it when none is. |
| Anchor batch nonce | The scheduler's next batch nonce at the end of the stop block. |
| Baseline | A snapshot of the application at the end of the stop block, registered at the rebuilt history end. |

`/history` reports the base count as `history.available_from`, and the stop
block, its input count, the anchor and the rebuilt history end under
`baseline` ([history metadata](../../README.md#history-metadata-internal-only)).
The first queued input stays off the wire.

### What setup does

**Before writing anything**, setup runs every check that can fail and be
retried: the RPC's chain id, a safe head sampled before discovery, and the
application's deployment block at or below that safe head. It also prepares
the fee source (a fixed price is held to the fee floor here), checks that the
key signs for the submitter address, and reads the base (step 1), whose state
bytes go to a scratch directory under the system's temporary directory. A
failure here, or a kill while a large machine loads, leaves the directory
empty but for the lock file, so retrying in the same directory is safe.

1. **Reads the base.** For a machine, setup loads it privately and answers two
   inspect queries ([reading a stored machine's export](../cartesi-machine.md#reading-a-stored-machines-export)):
   `scheduler`, the 48-byte scheduler export, and `state`, the application's
   canonical state bytes ([state export](../protocol/scheduler-semantics.md#state-export)).
   It builds the application with `Application::from_canonical_state` at the
   export's L2 tx count and refuses one that reports another count. For
   genesis, it builds the binary's genesis application, which must start at
   count zero.
2. **Flushes and pins the stop block.** From here setup writes: it pins the
   deployment identity and the first fee quote, then replaces whatever the
   provider's pool holds for the submitter account and waits until that is
   safe ([the flush and the stop block](#the-flush-and-the-stop-block)). The
   stop block is a safe block observed after it.
3. **Fills the input table and checks for an escape.** It syncs every L1 input
   from the deployment block to a safe head at or past the stop block. Then it
   refuses, with exit 40, if the wallet is still unsettled or a submitter
   input lies past the stop block. Either means a transaction escaped the
   flush or the old instance still runs. This is setup's one check on the old
   instance.
4. **Checks the base's export against L1.** Among the inputs the base
   consumed, the admitted directs above its clock (not from the submitter,
   payload within the application's cap) must number exactly the export's
   queue length and start at its first queued input
   ([the invariant](../protocol/scheduler-semantics.md#the-invariant)). The
   base must also lie at or below the stop block, and the scheduler must
   resume from the export over its queued inputs, with its clock at or below
   the stop block. A mismatch means the machine is not canonical, or its
   scheduler drained wrongly.
5. **Rebuilds the history with the scheduler.** It resumes the scheduler from
   the export, re-supplying the inputs from the first queued input up to the
   base's position, then feeds it every later input through the stop block,
   never the sync's overshoot. The scheduler never reads the application:
   what it hands out depends only on its state and the inputs. After the last
   input its clock must lie at or below the stop block.
6. **Executes each transaction as it is handed out.** The application runs it
   at once through `sequencer_core::application::execute`, the boundary the
   lane and the guest share. A direct always executes; a user op executes or
   is skipped ([a user op's fate](../protocol/scheduler-semantics.md#a-user-ops-fate)).
   An executed transaction is stored in the `rebuilt_history` table with its
   L2 tx count; a skipped user op is not stored. An application error stops
   setup naming the input, the transaction's position among those the input
   released, and the transaction. At the end, the directs executed must number
   the admitted directs the scheduler drained, counted in the input table.
7. **Publishes the new era.** It writes the baseline dump, then one transaction
   inserts the era row, which carries the batches' anchor nonce and whose
   existence marks setup complete, and registers the baseline. Setup opens no
   root frame and no Tip.

The rebuild reads inputs and writes rows in pages of 1024, so memory stays
bounded however long the rebuilt range is. Nothing is published until step 7.
A setup that fails after writes begin leaves a directory to discard, whatever
its exit code: another setup there exits 30.

### The flush and the stop block

Setup's database is new, so it has no wallet-nonce watermark from the old
instance, even when the old directory survives: a lost database takes its
watermark with it. The flush is therefore best effort. It sends no-ops for the
wallet slots the provider's pool knows about, which normally holds the
transactions in flight when the old instance stopped; on a settled wallet it
returns at once. A transaction dropped from that pool but alive elsewhere can
escape it. If it lands before setup's sync, the escape check refuses; if it
lands later, the new era's content-identity check sees a foreign landing,
freezes the accepted frontier, and the deployment needs another recovery. The
provider is trusted fail-stop, as the
[threat model](../threat-model/README.md#rpc-endpoint) specifies.

The stop block is fixed by the flush. The sync may run past it, but the rebuild
stops there, and a sync that ends below it refuses. In the new era, acceptance
scans only blocks after the stop block, starting at the anchor nonce, so no
landing in the rebuilt prefix is reinterpreted. Inputs past the stop block wait
in the input table for `run`.

## After setup

- **`run` opens the root frame** at its own safe head, through startup
  recovery's `EnsureOpenTip`. That frame first drains the directs still queued
  at the stop block, from the first queued input on, as the canonical
  scheduler's first frame does; submitter and over-cap inputs never execute.
  A standard-recovery cascade that tears every batch of the era drains from
  there again.
- **The watchdog compares the baseline.** The application's state at the end
  of the stop block is the canonical machine's state there, so until a batch of
  the era is accepted the sequencer serves the baseline as its comparison
  checkpoint at the stop block, for a genesis base too
  ([snapshot lifecycle](../snapshots/lifecycle.md#acceptance-and-comparison)).
  The watchdog compares it on its next tick, once no latch holds it; a mismatch
  there means the base machine was not canonical.
- **The feed serves the new era from the base count**: the rebuilt rows up
  to the rebuilt history end, then the live rows, in one read
  transaction. A direct's row takes its sender, payload, block, timestamp and
  transaction hash from the input table. A claim from an older era is refused
  with `ERA_CHANGED`; its client adopts the new era
  ([the client's playbook](#the-clients-playbook)).
- **`GET /nonce` counts only the live rows.** For a sender whose last op lies
  in the rebuilt history it reads low until that sender's next op is sequenced.
  A wallet that signs at the nonce its own state holds is unaffected, and a
  `422` bad-nonce rejection names the expected nonce.
- **The rebuilt history is permanent.** Its rows come from safe blocks, so they
  never change: no standard recovery cuts below the stop block, and storage
  triggers refuse an update or a deletion.

## How long a recovery takes

Setup's time has three parts, and the base decides the largest one. The
[recovery measurements](../review/2026-11-10-recovery-measurements.md) hold
the numbers for the reference wallet.

- **The sync** reads every input from the deployment block, so the RPC
  provider bounds it; its database writes are a small part. The dry run syncs
  the same inputs, so it takes as long. A sync through a deployment's provider
  is not measured yet.
- **The flush** returns at once on a settled wallet, and otherwise waits until
  its no-ops are safe.
- **The rebuild** recovers each user op's signature, one op after another on
  one core, and that is almost all of its time; executing the transactions,
  writing the rows and publishing take the rest. Its time therefore follows the user ops
  past the base, not the length of the history: a recent base keeps a
  recovery short, and genesis costs the whole history. The dry run prints how
  many inputs the rebuild replays; the measured rate per user op turns the ops
  among them into a time. Execution is the application's own cost, so measure
  a native engine's with it.

Reading the base machine is never the slow step. Memory stayed bounded in the
measurements, as the paging in [what setup does](#what-setup-does) intends,
but one reading is unexplained
([register](../review/register.md#sequencer-recovery-follow-ups)). On disk,
the rebuilt history about doubles the database. When the new era's first
batch lands, the canonical machine drains every direct still queued in that
one input; for the wallet, a fridge full to its bound drains well inside the
guest's budget for one input.

## Before anything

- **The old instance stays down.** The new data directory is pinned to the
  same submitter address, and the process lock guards one directory, not two.
  The flush settles transactions, not processes, so no command that signs with
  the key may run against the old directory again
  ([restart policy](../watchdog/operator-deployment.md#sequencer-restart-policy)).
  Setup's escape check catches an old instance that is mid-flight when it
  looks, not one started afterwards.
- **The base machine must be canonical.** Setup trusts it. The export check
  catches a machine whose queued directs disagree with L1, but nothing in setup
  checks the application state; the watchdog's first comparison of the new era
  does. The operator checks where the machine came from: a watchdog checkpoint,
  `incident/canonical`, or `sequencer-watchdog replay --out` were derived from
  L1 independently of the sequencer, and a machine from a validator node at a
  settled epoch can be checked against the root on chain.
- **The base must load.** It must be a machine of the deployment's template,
  stored by the Cartesi Machine version the sequencer links
  ([versions](../cartesi-machine.md#versions-are-all-or-nothing)), and stopped
  between inputs, waiting at an `RX_ACCEPTED` yield. Not every machine the
  watchdog stores does
  ([which ones](../watchdog/README.md#machines-a-recovery-can-start-from)).
  Setup refuses a halted machine, so a canonical machine that halted cannot be
  recovered: the application is dead on chain.
- **Recovery repairs only the sequencer.** What the canonical machine executed
  stays executed, a faulty sequencer's accepted batches included.
- **Preserve the evidence first.** Keep the old data directory, its logs, and
  the watchdog's `incident/` before anything is rebuilt; affected client
  databases are preserved separately.

## Recovery readiness before deployment

A recovery runs once in a deployment's life, so it must be ready before it is
needed:

- **The application rebuilds from a machine.** It implements
  `from_canonical_state`, and its template answers the `scheduler` and `state`
  queries ([canonical recovery integration](../protocol/application-contract.md#7-canonical-recovery-integration)).
  The `state` answer must fit one 2 MiB report; a larger state needs another
  state source ([the loader's limits](../cartesi-machine.md#reading-a-stored-machines-export)).
  The [measurements](../review/2026-11-10-recovery-measurements.md#b-reading-a-base-machine)
  give how many wallet accounts fit; measure the application's own `state`
  size against it, since until setup has a range read a machine whose state
  has outgrown it cannot be a base.
- **The operator can find a trusted machine** at or below a given L2 tx count,
  and keeps machines backed up. The watchdog's checkpoints are one source
  ([backups](../watchdog/operator-deployment.md#checkpoint-disk-usage-and-backups));
  sourcing from a validator node is
  [open work](../plans/2026-11-sequencer-recovery/todo.md#machine-sourcing).
- **Each client keeps backups it can restore at a count it trusts**
  ([client backups](../plans/2026-11-sequencer-recovery/client-backups.md)).
- **The operators have run the drill.** The
  [incident runbook's drills](../watchdog/incident-runbook.md#drills) say which
  parts exist as tests.

## The operator's playbook

1. **Stop the old instance**, keep it down, and preserve its evidence
   ([before anything](#before-anything)).
2. **Fix the cause** when a bug caused the incident.
3. **Ask each client for a state it trusts**, as an L2 tx count.
4. **Pick a base** at or below every client's count: a machine, or genesis.
   `setup --base <machine> --dry-run` prints its L2 tx count and the old
   instance's activity past it.
5. **Run `setup --base <machine>`** with the submitter key, in an empty data
   directory.
6. **Run `run`** on that directory.
7. **Tell each client to realign** at its own count.
8. **Clear the watchdog's latch, then watch its next comparison.** If a
   divergence latched it, clear it once `run` has started
   ([incident runbook](../watchdog/incident-runbook.md#3-resolve)). The next
   tick compares the new era's baseline at the stop block.

## The client's playbook

A client of the old era holds state at some L2 tx count `X`. It realigns on the
feed it already uses, with no file and no separate tool:

1. **Restore a trusted state** at its count `X`.
2. **Read `/history`** for the new era's identity, its base count
   (`history.available_from`) and its rebuilt history end
   (`baseline.rebuilt_history_end`).
3. **Check `X` against that range** and take the claim to resume with:
   `HistoryInfo::adopt_claim(X)` in `sequencer-core`, or
   `SequencerClient::adopt_current_era(X)` in the Rust SDK. Below the base
   count (`BelowBase`), the base is newer than the client's state, so the
   operator must recover from an older machine. Past the rebuilt history end
   (`PastRebuiltEnd`), the state holds rows the old instance served that never
   landed by the stop block: restore an older state.
4. **Optionally check its rows.** Subscribe from the base count and compare
   each row up to `X` with the row it stored. Any difference means its state is
   not canonical.
5. **Subscribe with the adopted claim** and follow the feed as always. The
   claim carries the new era's version, and the server checks it like any
   other; it does not check the rebuilt history end itself, so step 3 is the
   client's to do.

A reader that owns more state than the application, such as an indexer of
transfers or orders, restores a checkpoint of that state too, at the same
count; matching current balances does not validate historical records. A
reader with no checkpoint it trusts can rebuild from the feed only when the
era's base is genesis: the feed starts at the base count, and the rows below
it exist only on L1.

## What can go wrong

| Problem | Seen where | Remedy |
|---|---|---|
| The data directory holds anything | Setup refuses it untouched, exit 30 | Run setup in an empty directory. |
| L1 is unreachable, or the deployment block is not yet safe | Setup's checks before writing, exit 20 | Retry in the same directory. |
| The key does not sign for the submitter address | Setup's identity check before writing, exit 30 | Configure the pinned submitter's key. |
| The base machine cannot be loaded, is not between inputs, or does not answer | Setup's step 1, exit 30 | Pick another machine ([which watchdog machines qualify](../watchdog/README.md#machines-a-recovery-can-start-from)). |
| The base's `state` report is over 2 MiB | Setup's step 1, exit 30: the machine did not answer the `state` query, because it halted | Recover from an older machine whose state fits, or from genesis. |
| The application cannot be built from the machine's state | Setup's step 1, exit 30 | A guest that fails to encode its state still answers `state`, with its error text (`inspect failed: ...`) as the report, so a decode error here may mean the guest's encoder failed: query the machine's `state` and read the report. |
| A transaction escaped the flush, or the old instance runs | Setup's escape check, exit 40 | Stop the old instance, discard the directory, run setup again. |
| The RPC node lags the base, so the base lies past the stop block | Setup's export check, exit 30 | Let the node catch up, discard the directory, run setup again. |
| The base's export disagrees with L1 | Setup's export check, exit 30 | Recover from a trusted machine; if this one is canonical, its scheduler drained wrongly. |
| A failure after writes began, retryable or not | Setup's exit code, then exit 30 on any retry in place | Discard the directory and run setup again. |
| The application fails on a rebuilt transaction | Setup stops naming the input index, the position and the transaction, exit 30 | Replay a trusted machine to that input: if it halts there too, the application is dead; if not, fix the native engine or its projection. |
| The base machine is not canonical | The new era's first watchdog comparison | Recover from a trusted machine. |
| The base is newer than a client's state | The client's `BelowBase`, or `HISTORY_UNAVAILABLE` on subscribe | Recover again from an older machine. |
| A client's state holds rows that never landed | The client's `PastRebuiltEnd` | The client restores an older state. |
| A client's state is wrong below its count | Only the client's own checks, such as the row comparison | The client restores an older state. |

## Code map

| Concern | Code and tests |
|---|---|
| Arguments and the key | [`commands/config.rs`](../../sequencer/src/commands/config.rs) `SetupConfig`; its `setup_*` tests. |
| The procedure, the empty-directory rule, the dry run and the escape check | [`commands/setup/mod.rs`](../../sequencer/src/commands/setup/mod.rs); [`chain_tests.rs`](../../sequencer/src/commands/setup/chain_tests.rs) runs setup against Anvil on a chain with history and compares it with `CanonicalRunner`. |
| The base, and the seam tests use instead of an emulator | [`commands/setup/base.rs`](../../sequencer/src/commands/setup/base.rs) `ReadMachineExport`; [`machine.rs`](../../sequencer/src/machine.rs) `read_machine_export`, whose unit tests drive every failure arm through a scripted emulator, and `canonical-test`'s `export` cases run on the built image. |
| Export check, and the rebuild executing as it goes | [`commands/setup/rebuild.rs`](../../sequencer/src/commands/setup/rebuild.rs); `Scheduler::resume` in [`sequencer-core`](../../sequencer-core/src/scheduler/mod.rs); `admitted_directs_in` in [`storage/l1_inputs.rs`](../../sequencer/src/storage/l1_inputs.rs) for the closing direct count. |
| Publication | [`commands/setup/fill.rs`](../../sequencer/src/commands/setup/fill.rs); `publish_era` in [`storage/lifecycle.rs`](../../sequencer/src/storage/lifecycle.rs). |
| The rebuilt history table and the `era` row | [`storage/rebuilt_history.rs`](../../sequencer/src/storage/rebuilt_history.rs), [`storage/history.rs`](../../sequencer/src/storage/history.rs) (`Era`, `NewEra`, `era_in`, `history_version_in`), and their triggers in [`0001_schema.sql`](../../sequencer/src/storage/migrations/0001_schema.sql). |
| The root frame's drain from the first queued input | `next_undrained_safe_input_index_in` in [`storage/ingress.rs`](../../sequencer/src/storage/ingress.rs). |
| The baseline as the first comparison checkpoint | `finalized_dump_in` in [`storage/snapshots.rs`](../../sequencer/src/storage/snapshots.rs). |
| The feed across the rebuilt and live rows | [`storage/history/canonical.rs`](../../sequencer/src/storage/history/canonical.rs). |
| Adoption | `HistoryInfo::adopt_claim` in [`history_api.rs`](../../sequencer-core/src/history_api.rs); `adopt_current_era` in [`sdk/rust-client`](../../sdk/rust-client/src/history.rs). |
| The flush | [`recovery/flusher.rs`](../../sequencer/src/recovery/flusher.rs) `flush_to_safe`. |
| End to end on the devnet | [`tests/e2e/src/recovery_round_trip.rs`](../../tests/e2e/src/recovery_round_trip.rs): the round trips from genesis and from a stored machine, and setup's refusal of a set-up directory, each for the Rust and C hosts ([drills](../watchdog/incident-runbook.md#drills)). |
