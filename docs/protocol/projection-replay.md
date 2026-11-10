# Application projections from historical L1 inputs

A reader may maintain transfers, orders, or other application-specific history
absent from the latest application state. It owns that projection and its
checkpoints. The feed serves the era's history from its base count: the
rows setup rebuilt from L1, then the live rows. Below the base count, the
sequencer supplies the raw L1 inputs, which the reader runs through the
canonical scheduler itself. The canonical scheduler remains the ordering
authority. The [README API](../../README.md#history-metadata-and-historical-l1-inputs-internal-only)
owns routes, fields, limits, and refusals; the
[history contract](application-history.md) owns application coordinates and
the era record.

The raw-input route and this workflow are to be deleted when the
[sequencer recovery](../plans/2026-11-sequencer-recovery/README.md) plan
lands, so they get bug fixes only; new client work should not build on them
([Track 3](../plans/2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)).

## Bootstrap and handoff

A reader needs history from count zero. An era whose base is genesis has base
count `K = 0`, so the feed already serves all of it: subscribe at 0 and skip
the rest of this section. An era whose base is a stored machine has `K > 0`,
and the rows below `K` exist only on L1.

1. Read `/history`. Pin the deployment identity and era, the base count `K`
   (`available_from`), the stop block `C`, the exclusive raw end `R`
   (`l1_end_input_index`), the rebuilt history end `E`, and the next batch
   nonce. Use the deployment's application/genesis configuration and scheduler
   release, including its batch codec, wait bound, payload cap, and
   signing-domain name/version.
2. From trusted genesis, request historical records from raw index 0 and process
   every record, with its index and timestamp, through the canonical scheduler
   and then the application, input by input. Preserve InputBox order, including
   within blocks. Malformed or rejected batch payloads still reach the
   scheduler: its overdue-direct backstop runs before batch decoding.
3. Continue with the returned raw cursor until it equals `R`; page length and
   block changes are not EOF. Process nothing more: the inputs through `C` are
   the ones setup's rebuild fed, and the directs still queued at `C` belong to
   the era's first live frame. The application's count must now equal `E`, and
   the scheduler's next batch nonce the baseline's.
4. Refresh `/history?era_id=<selected-era>` for the current generation and
   subscribe at count `E`. The feed contains entries `[K,head)`, then future
   entries; from `E` on it serves only what the reader has not executed.

Raw records carry original inner payloads and authenticated sender/block context,
not the complete machine transport envelope. The signing domain uses the pinned
release plus deployment chain id and app address. A record's index and
timestamp are scheduler inputs, which reach the application with each direct
([direct context](scheduler-semantics.md#direct-context)); transaction hashes
are provenance.

An ordinary generation change leaves `K`, `C`, `R`, `E`, the nonce and the
historical pages intact. If it occurs before subscription, refresh the same
era's metadata and retry the claim. An era change before subscription is what
any client of an older era meets: the reader adopts the new era at its own
count when the [adopt rule](application-history.md#adopting-a-new-era) allows,
and otherwise starts again against the new era. Count and nonce equality is a
consistency check, not independent proof that the reader computed correct
state.

The era's baseline is the application's state at the end of block `C`, which
the canonical machine reaches too, so the state the reader holds after step 3
is canonical at `C` and is the era's first comparison checkpoint.

## Client checkpoints

Save core application state, projection, and their actual `HistoryClaim`
consistently. For a checkpoint within the era, query
`/history?era_id=<saved-era>&from_generation=<saved-generation>`
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
stays queued at stop block `C = 1240`: the first queued input is 8. The era
has `E = 7`, next nonce 2 and raw end `R = 8`. Subscribe at offset 7; the next
raw index is independently 8.

The [reference integration test](../../sequencer/src/integration_tests/historical_bootstrap.rs)
publishes the era a setup from a machine stopped at the stop block would, so
`K = E = 7`. It replays this trace from genesis through one-record HTTP pages
fetched with the SDK, checks the scheduler's export and the application's
state against the reference run and the era, and joins the real WS feed at
`E`, where the next direct arrives with its index and timestamp. It checks the
transfer history against uninterrupted execution. It uses controlled storage
fixtures; it does not run setup or establish canonical-machine export
conformance.
