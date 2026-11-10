# Application projections from historical L1 inputs

A reader may maintain transfers, orders, or other application-specific history
absent from the latest application state. It owns that projection and its
checkpoints. The sequencer supplies raw scheduler inputs for the immutable era
baseline, then application inputs for the optimistic suffix. The canonical
scheduler remains the ordering authority. The [README API](../../README.md#history-metadata-and-historical-l1-inputs-internal-only)
owns routes, fields, limits, and refusals; the
[history contract](application-history.md) owns application coordinates.

The planned shared recovery tool supersedes this workflow, so it gets bug fixes
only; new client work should not build on it
([Track 3](../plans/2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)).

## Bootstrap and handoff

1. Read `/history`. Pin the deployment identity and era, baseline application
   count `K`, L1 stop block `C`, exclusive raw end `R`, and next scheduler nonce.
   Use the deployment's application/genesis configuration and scheduler release,
   including its batch codec, wait bound, and signing-domain name/version.
2. From trusted genesis, request historical records from raw index 0 and process
   every record, with its index and timestamp, through the scheduler. Preserve
   InputBox order, including within blocks. Malformed/rejected batch payloads still reach the scheduler: its
   overdue-direct backstop runs before batch decoding.
3. Continue using the returned raw cursor until it equals `R`; page length and
   block changes are not EOF. Drain the remaining directs through `C`, reproducing
   the recovery terminal drain. Verify resulting count `K` and next nonce.
4. Refresh `/history?era_id=<selected-era>` for the current generation, confirm
   the baseline association, and subscribe at application count `K`. The feed
   contains entries `[K,head)` and then future entries. It does not redeliver the
   inputs already included in the baseline.

Raw records carry original inner payloads and authenticated sender/block context,
not the complete machine transport envelope. The signing domain uses the pinned
release plus deployment chain id and app address. A record's index and
timestamp are scheduler inputs, which reach the application with each direct
([direct context](scheduler-semantics.md#direct-context)); transaction hashes
are provenance.

An ordinary generation change leaves `C/R/K/nonce` and historical pages intact.
If it occurs before subscription, refresh the same era's metadata and retry the
claim. An era change requires establishing correspondence with the new baseline;
never silently splice its pages into an earlier replay. Count/nonce equality is
a consistency check, not independent proof that a client computed correct state.

The terminal-drained baseline need not equal canonical state at block `C`:
young directs may execute preemptively. A later accepted batch provides the
canonical comparison boundary. Keep that distinction when validating recovery.

## Client checkpoints

Save core application state, projection, and their actual `HistoryClaim`
consistently. An exact era-baseline checkpoint can be rebound to the current
generation after confirming the same immutable era/baseline. For suffix
checkpoints, query `/history?era_id=<saved-era>&from_generation=<saved-generation>`
and require `K <= saved_count <= compatibility.preserved_input_count`. Query each
candidate using its own generation and choose the newest eligible backup.
Restore the complete application/projection checkpoint, persist the returned
version with it, and resume at its saved count. Repeat compatibility lookup if
another recovery causes WS to refuse that version. The
[history contract](application-history.md#checkpoint-compatibility-after-standard-recovery)
owns the calculation and its standard-recovery trust assumptions.

If checkpointing during raw replay, persist the scheduler's state and the raw
cursor alongside app/projection state; persisting only an application count
cannot resume an interrupted scheduler. The reference's state is its
[export](scheduler-semantics.md#state-export), from which `Scheduler::resume`
rebuilds the queue by re-reading the records from the first queued input.

## Worked bootstrap

With the 1200-block wait bound, consider valid direct inputs `D*` and executed
user operations `U*` in this raw order:

| Raw index | Block | Input | Application execution |
|---:|---:|---|---|
| 0 | 5 | `D0` | Queued |
| 1 | 12 | `D1` | Queued |
| 2 | 20 | Batch 0, safe block 10, `U0` | `D0`, `U0` |
| 3 | 20 | `D2`, after the batch | Queued |
| 4 | 24 | `D3` | Queued |
| 5 | 1230 | Malformed batch | The backstop executes `D1`, `D2`, `D3`; decoding rejects it. |
| 6 | 1231 | `D4` | Queued |
| 7 | 1232 | Batch 1, safe block 1231, `U1` | `D4`, `U1` |

The malformed input executes three overdue directs without consuming a batch
nonce. Batch 1's frame drains `D4`, whose block is its safe block, so nothing
stays queued and, for a rebuilt stop `C=1240`, the terminal drain executes
nothing. The resulting baseline is `K=7`, next nonce 2 and raw end `R=8`.
Subscribe at application offset 7; the next raw index is independently 8.

The [reference integration test](../../sequencer/src/integration_tests/historical_bootstrap.rs)
replays this trace from genesis through one-record HTTP pages fetched with the
SDK, checks the scheduler's state against the baseline, and joins the real WS
feed at nonzero `K`, where the next direct arrives with its index and
timestamp. It checks full wallet state and explicit projection order against
uninterrupted execution. It uses controlled storage fixtures; it does not
establish canonical-machine export conformance.
