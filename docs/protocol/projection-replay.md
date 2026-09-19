# Application projections from historical L1 inputs

A reader may maintain transfers, orders, or other application-specific history
absent from the latest application state. It owns that projection and its
checkpoints. The sequencer supplies raw scheduler inputs for the immutable era
baseline, then application inputs for the optimistic suffix. The canonical
scheduler remains the ordering authority. The [README API](../../README.md#history-metadata-and-historical-l1-inputs-internal-only)
owns routes, fields, limits, and refusals; the
[history contract](application-history.md) owns application coordinates.

## Bootstrap and handoff

1. Read `/history`. Pin the deployment identity and era, baseline application
   count `K`, L1 stop block `C`, exclusive raw end `R`, and next scheduler nonce.
   Use the deployment's application/genesis configuration and scheduler release,
   including its batch codec, wait bound, and signing-domain name/version.
2. From trusted genesis, request historical records from raw index 0 and process
   every record through the scheduler. Preserve InputBox order, including within
   blocks. Malformed/rejected batch payloads still reach the scheduler: its
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
release plus deployment chain id and app address. Timestamps and transaction
hashes are provenance, not additional scheduler transition inputs.

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

For efficient manual recovery, prepare checkpoints at supported accepted L1
block boundaries. Read the coherent `accepted_checkpoint` and `history.version`
from `/history`; restore an earlier compatible client backup and replay the
application feed exactly to the accepted count. Save the complete result with
that receipt. If the live reader is already ahead, use an independent restore;
the client owns the copy/replay cost. Bind the receipt to the matching history
and count. Count alone does not identify its block/nonce: empty batches can
share a count, and generations can reuse replaced offsets.

Such a backup contains core state and projection at count `X`, inclusion block
`B`, next scheduler nonce `N`, and the application's own clock `A`. The
[manual recovery contract](../recovery/cockroach.md#replay-boundaries) requires
`A < B`, except known empty genesis, and `B <= C` for the target rebuild:

1. Independently establish trust in the backup, projection implementation, and
   checkpoint boundary under the [incident playbook](../recovery/cockroach.md#application-specific-reader-state).
2. Fetch raw records **after `A`**, not after `B`. Enqueue external directs from
   `(A,B]` without executing them; skip batch envelopes in this seed range.
3. Process all records in `(B,C]` through the scheduler at nonce `N`, then perform
   the same terminal drain and handoff as genesis bootstrap.

Seeds and replay may share one paginated traversal. Page boundaries can split
block `B`; they must not cause premature replay or draining. An arbitrary
mid-batch optimistic checkpoint lacks this scheduler continuation. If an eligible
backup cannot be trusted, use an earlier eligible backup or trusted genesis.
Watchdog agreement on current application bytes does not certify the reader's
additional transfer/order history.

If checkpointing during raw replay, persist the scheduler queue/nonce and raw
cursor alongside app/projection state, or resume from the prepared backup.
Persisting only an application count cannot resume an interrupted scheduler.

## Worked recovery boundary

With the 1200-block wait bound, consider valid direct inputs `D*` and accepted
user operations `U*` in this raw order:

| Raw index | Block | Input | Application execution |
|---:|---:|---|---|
| 0 | 5 | `D0` | Queued |
| 1 | 12 | `D1` | Queued |
| 2 | 20 | Batch 0, safe block 10, `U0` | `D0`, `U0` |
| 3 | 20 | `D2`, after the batch | Queued |
| 4 | 24 | `D3` | Queued |
| 5 | 1230 | Malformed batch | Backstop executes `D1,D2,D3`; decoding rejects |
| 6 | 1232 | Batch 1, safe block 1228, `U1` | `U1` |
| 7 | 1235 | `D4` | Queued |

The backup at `B=20` has count 2, clock `A=10`, and next nonce 1. A count-1
backup sits partway through batch 0; it must execute `U0` before attaching this
receipt. Seed reconstruction starts at raw index 1, preserving both `D1` and the
same-block `D2` without executing batch 0 twice.

For a rebuilt stop `C=1240`, the malformed input executes three overdue directs
without consuming a batch nonce. Batch 1 executes `U1`; terminal drain executes
`D4`. The resulting baseline is `K=7`, next nonce 2, raw end `R=8`, and app clock
1235. Subscribe at application offset 7; the next raw index is independently 8.

The [reference integration test](../../sequencer/src/integration_tests/historical_bootstrap.rs)
restores a wallet checkpoint with separately saved notice history, deletes the
source backup, fetches one-record HTTP pages through the SDK, executes this
trace, and joins the real WS feed at nonzero `K`. It checks full wallet state and
explicit projection order against uninterrupted execution. It uses controlled
storage fixtures and a trusted checkpoint receipt; it does not establish Bart's
private database backup procedure or canonical-machine export conformance.
