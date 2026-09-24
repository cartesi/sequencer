# Watchdog Incident Runbook

What to do when the watchdog latches a divergence: `tick` exits 2 and
`cartesi_watchdog_status{state="failed"}` is 1. Every step names the command
that performs it, and each scenario has a drill that practices it before an
incident does. The [watchdog README](README.md) owns how the latch and its
evidence are produced.

## What you have

```bash
sequencer-watchdog status
```

`status` prints the state directory's summary. Its `divergence` field is the
latch record, with the evidence index under `divergence.evidence`:

| Field | Meaning |
|---|---|
| `kind` | `state_mismatch`, `canonical_machine_dead`, or `inclusion_block_regressed` |
| `target_block` (B) | The L1 block the comparison ran at |
| `agreed` (P) | The watchdog's head, which stays there: the last block the sequencer agreed with, or the trusted bootstrap block before the first agreement |
| `canonical_sha256`, `sequencer_sha256` | Both digests, for `state_mismatch` |
| `stop` | For `canonical_machine_dead`: the input index and block that stopped the machine, and how |
| `evidence` | What was kept under `incident/` in the state directory, and `collection` |

Evidence under `incident/`, each listed in `evidence` by its path, or as
`<item>_missing` with the reason:

- `sequencer.bin`: the sequencer's comparison file, when it still served
  block B. `evidence.comparison` gives the 0-based offset of the first byte
  where it differs from the canonical bytes (`cmp -l` prints it plus one) and
  the number of differing 4 KiB pages.
  `evidence.comparison.identical` means the bytes agree and only the digests
  did not: the fault is in how the sequencer served its digest, not in either
  execution.
- `canonical/`: the canonical machine at B (a stored machine directory).
- `canonical.bin`: the canonical comparison bytes; `cmp -l canonical.bin
  sequencer.bin` works on the files directly.

`evidence.collection` tells whether the list is complete:

| `collection` | Meaning |
|---|---|
| `running` | The latching tick is still collecting. It holds the wrapper's lock, so `clear` reports `already locked`; killing it is safe. If nothing holds the lock, the collection died and no tick has marked it yet: read it as `interrupted` |
| `finished` | Every item that applies is listed, kept or missing; `comparison` only when `sequencer_bytes` was kept |
| `interrupted` | The latching tick died while collecting. Files under their final names are complete; `*.tmp` files are partial |
| `unreadable` | A crash tore the index. Files under their final names are still complete |
| (no `evidence`) | The latching tick died before collecting anything, or the kind has none (`inclusion_block_regressed`) |

Evidence files are renamed into place but not fsynced, except `canonical/`.
After a host crash, check `sha256sum canonical.bin` against `canonical_sha256`,
and `sequencer.bin` against `sequencer_sha256`, or against `canonical_sha256`
when `comparison.identical`.

A latch of kind `unreadable_marker` is an `incident/` whose record was lost or
torn by a crash or a full disk. Its evidence, if any, is still under
`incident/`; `clear` accepts any block for it.

A log line `divergence <kind> detected at block B but NOT latched` means the
tick could not even create `incident/` (a full state directory). Treat block B
as latched and follow this runbook from the log line; fix the state directory,
and the next tick finds the divergence again if it persists. A log line `the
latch record could not be written` means `incident/` exists and the latch
holds, as `unreadable_marker`; the log lines carry its kind and block.

The log of the latching tick also carries the guest's console output, which
for `canonical_machine_dead` usually includes the application's last message.

## 1. Stop the sequencer

Stop the sequencer's `run` process. The sequencer cannot take funds, but users
act on its soft confirmations, and after a real divergence new confirmations
may not hold. A false alarm costs downtime; users can still reach the
application through L1 direct inputs meanwhile. Keep it running only with
evidence that the fault is on the watchdog's side (step 2).

The latching tick fetches the sequencer's comparison file first, while
`evidence.collection` is `running` without `sequencer_bytes`. A graceful stop
of the sequencer stops it taking transactions at once but lets that download
finish, so the stop can take as long as the download; a supervisor that kills
the sequencer first leaves `sequencer_bytes_missing: transient: …`. Do not
wait for the file: step 2's verdict comes from `replay`. To end the download
now, kill the collecting watchdog tick. Preserve the sequencer's data
directory before any restart (cockroach recovery, step 1).

## 2. Find the faulty side

### `state_mismatch`

Re-derive the canonical state at B from a trusted machine that predates the
incident, independently of the watchdog's checkpoint chain:

```bash
sequencer-watchdog replay --from <trusted machine> --from-block <A> \
    --to-block <B> --out /tmp/replay-<B>
```

Use the application's template machine with `--from-block 0` (`replay` checks
it against the on-chain template hash), or an archived checkpoint whose block
you trust. `replay` needs an RPC with state at `A` (an archive node for old
blocks), absolute paths, and an `--out` that does not exist; it never writes
the state directory.

| `replay`'s `sha256` equals | Conclusion |
|---|---|
| `canonical_sha256` | The canonical side is reproducible: **the sequencer is wrong** |
| `sequencer_sha256` | The watchdog's checkpoint chain is wrong (bad bootstrap or state): **the watchdog is wrong** |
| neither | The trusted source is not what you think, or something is nondeterministic: escalate with all three digests |

To locate a sequencer fault, start from `evidence.comparison` and the inputs of
blocks `(P, B]`.

### `canonical_machine_dead`

The canonical machine reached a permanent fixed point (`stop.description`) at
input `stop.input_index`. First rule out a wrong image: `replay` from the
trusted template to B must stop at the same input. If it does, the
application is dead on chain: no later input will ever be processed, and
nothing the sequencer does changes that. The sequencer stays stopped; this is
an incident for the application's owners.

### `inclusion_block_regressed`

The sequencer's accepted block went below a block it had agreed with. Expected
causes are an operator error on the sequencer side: the watchdog pointed at
another deployment (`CARTESI_WATCHDOG_SEQUENCER_URL`), or a sequencer data
directory restored from an older copy. Check the sequencer's
`/finalized_state/inclusion_block` and its deployment before anything else. A
sequencer that has not yet reached the watchdog's bootstrap block is not a
regression; the watchdog idles.

## 3. Resolve

**Sequencer wrong.** Fix the bug, then rebuild the sequencer with
[cockroach recovery](../recovery/cockroach.md) from a trusted canonical
checkpoint: `incident/canonical` at B (or step 2's `replay --out` directory
when `incident/canonical` is absent), or the head at P. Resume the sequencer,
then clear the latch:

```bash
sequencer-watchdog clear --block <B> --reason "sequencer fixed: <ticket>"
```

The watchdog keeps its head at P: recovering the sequencer does not change
canonical history, so the next tick replays to the new accepted block and
compares.

**Watchdog wrong.** Copy `incident/` somewhere for the record, fix the cause
(configuration, bootstrap machine, image), wipe the state directory, run
`init` from a trusted bootstrap, and restart the sequencer.

**Canonical machine dead.** The sequencer stays stopped and the latch stays.

`clear` archives the incident under `incidents/` with the reason; it never
deletes evidence, and it refuses a block other than the latched one (an
unreadable marker has no block, so any is accepted). While an evidence
collection is `running`, `clear` reports `already locked`; retry once it
ends. Clearing is always safe: if the cause persists, the next tick latches
again.

## Drills

| Scenario | Drill | Practices |
|---|---|---|
| Watchdog wrong (`state_mismatch`) | `watchdog_divergence_drill_test` in `just test-rollups-e2e` | init refusing a non-template image at genesis; a wrong non-genesis bootstrap latching; `status`; `clear` refusing another block; `replay` from the trusted image matching the sequencer; re-latching after a premature `clear`; re-`init` agreeing |
| Canonical machine dead | `tick latches a dead canonical machine and replay reproduces the stop` in `just test-watchdog-e2e` | the latch at the stopping input, and `replay` reproducing the stop |
| Sequencer wrong (`state_mismatch`) | `tick latches a mismatch with the canonical machine and a byte diff` in `just test-watchdog-e2e` | the evidence: canonical machine, both byte strings, and the first difference |
| Evidence lost to a full disk | `main tick keeps a divergence latched when its evidence cannot be kept` in `just test-watchdog` | the latch and exit 2 surviving a failed evidence item, which `evidence` names |
| `inclusion_block_regressed` | `tick latches a regression below an agreed block` and `tick idles while the sequencer has not reached the bootstrap block` in `just test-watchdog` | the latch, and idling instead behind a never-agreed bootstrap |

Before production, run the first drill once against the staging deployment
with the operators who will be paged: bootstrap a scratch watchdog state
directory from a wrong image at a block after the first input, and walk this
runbook to the re-`init`.
