# Feed & Replay Protocol Design (Track 3)

**Status: internal read foundation implemented; consumer API cutover pending.**
Typed history claims and refusals, coherent canonical pages, and history
identity captured with snapshot leases are implemented alongside the durable
history/execution foundation. HTTP snapshot metadata, the canonical-coordinate
WS protocol, and the matching SDK/consumer workflow remain to be implemented. The current API contract is
in the [README](../../README.md); the [ordered handoff](#7-ordered-implementation-handoff)
defines the remaining work. Close the WS invalidation-contract finding only
after the consumer cutover and its acceptance tests.

## 1. Consumer workflow and scope

A subscriber starts cold by downloading an application-defined snapshot over
HTTP, restores its replicated application, and subscribes from that state's
executed-input count. One WS stream supplies every subsequent committed input
in order, then continues following the tip. Recovery and rebuild boundaries
must be detected before the consumer applies inputs from a different history.
The application owns the restore artifact and its format; egress transports
that artifact with enough metadata to identify the state it contains.

The watchdog has a different trust boundary: it starts from trusted state,
advances independently using L1 inputs, and fetches the matching finalized
comparison artifact. Its finalized-state HTTP endpoints remain available.
The egress API serves both consumers exclusively within the operator's own
infrastructure, with network access controls.

HTTP fits a finite snapshot download and already has file streaming and leases.
WS fits the existing ordered-input feed: stored and newly committed inputs use
the same replay loop. Splitting finalized replay onto HTTP would add another
input-fetching path and a moving-boundary handoff without helping this replica
workflow. Raw `/inputs` and paginated HTTP `/l2-txs` are outside this scope;
revisit them for a concrete archival or historical-query consumer. WS carries
backlog on both sides of the gold boundary.

The current snapshot-then-subscribe path already supports ordinary bootstrap,
but its physical rowid cursor cannot detect recovery discontinuities. Its total
catch-up limit can also reject a valid snapshot whose download/restore or
preceding execution leaves too much backlog. The new protocol addresses those
boundaries without requiring different checkpoint timing.

## 2. Concepts and coordinates

- **Input-box coordinate `input_index: u64`** — position in `safe_inputs`
  (per-application InputBox order). Append-only and sourced from L1 safe blocks.
- **Feed coordinate `offset: ExecutedInputCount`** — the authoritative
  `Application::executed_input_count()` boundary, starting at zero. An
  application at `X` is ready to consume history entry `X`; applying that
  entry advances it to `X + 1`. SQLite stores this as a sparse canonical
  attribution beside its append-only physical rowid replay log. The current
  feed still exposes rowid and changes only at the API cutover.
- **Era base `K`** — the smallest feed offset locally available in this era.
  Genesis setup starts at zero. Cockroach recovery sets it to
  `S'.executed_input_count()` after the fold; absolute offsets continue, but
  the unavailable prefix is not reconstructed. `K` is application history,
  not the snapshot's physical `l2_tx_index`: recovery cursor-padding rows may
  advance the latter without executing an application input.
- **Snapshot count `C`** — the exact executed-input count in a selected
  application dump. The replica restores at `C` and requests input `C` next.
  A current snapshot may be newer than `K`; retaining a snapshot exactly at
  `K` is not required.
- **Gold boundary `G`** — within one era, the exclusive executed-input count
  after the scheduler-accepted prefix. Entries with `offset < G` cannot be
  invalidated; `G` only advances. It does not restrict WS admission or
  determine which transport carries an input.
- **Era ID `e`** — random durable UUIDv4 minted write-once in one era's
  baseline transaction. Cockroach recovery/fresh setup creates a new era
  because the rebuilt DB cannot serve the prior era's ordered L2 history from
  genesis.
- **Recovery generation `g: u64`** — soft-suffix reality version within one
  era. Bumped exactly once by a standard-recovery transaction iff it
  invalidates at least one valid batch. Entries with `offset < G` are
  generation-free within that era.
- **History version `(e, g)`** — equality/discontinuity token carried by the
  protocol. It is not a globally ordered number.
- **Live head `H`** — the current application's exclusive
  `executed_input_count`; locally available entries occupy `[K, H)`.

## 3. History-version semantics

- `EraId` is a 16-byte UUIDv4 newtype, persisted write-once per era and exposed
  as canonical lowercase hyphenated JSON. Store `created_at` separately; a
  bare timestamp is not collision-resistant under clock rollback or
  simultaneous setup.
- `RecoveryGeneration` starts at zero and increments exactly once in the same
  transaction iff standard recovery invalidates at least one valid batch.
  Ensuring/reopening a missing Tip without invalidation does not bump it.
- Clean restart and inspection that admits without changing history change
  neither field.
- Cockroach recovery/fresh setup mints a new era and resets generation to
  zero. An interrupted attempt that retains its incomplete DB reuses the
  already-minted, externally unexposed era. A fail-loud partial-fill refusal
  requires an operator wipe/retry and therefore mints another unexposed era.
  A new era is an explicit operator-driven setup/rebuild action; there is no
  in-place rotation tool, automated DB replacement, implicit clone detection,
  distributed fencing, or partial-fill resume protocol.
- Copying an initialized DB copies its era too. Arbitrary clone-and-run is
  unsupported. Operating copied state as a new era requires explicit
  fresh/wiped-directory setup/rebuild; detecting uncoordinated clones requires
  external authority.
- The snapshot response carries the pair together with the dump's count;
  every subscribe request claims that pair. WS responses carry the admitted
  recovery generation. The bootstrap workflow requires no separate
  `/history-version` request. Reading current metadata cannot authorize an old
  state: the client keeps the history identity associated with its own state.

Standard recovery restores the retained application state, so its count rolls
back, then advances over replacement force-drained directs. The same suffix
count may name a different input under a new generation. The initial client
workflow handles both a stale generation and a changed era by discarding its
replica and downloading a current snapshot. It does this even when the old
state's numeric count is in the new history's range. Retaining a known-stable
checkpoint for cheaper rollback is a possible later client optimization.

Recovery and rebuild run before runtime admission across a process boundary.
Existing subscriptions end before history changes. Each admitted session is
bound to its validated history version; reconnect validation is the correctness
boundary. A guaranteed farewell, invalidation broadcaster, or generation-polling
worker is unnecessary under this lifecycle.

Cockroach recovery leaves the rebuild base NULL at baseline creation, then
binds `K = S'.executed_input_count()` in the same transaction that registers
the initial finalized snapshot. Setup completion refuses until both exist.
Requests below `K` receive a typed `history_unavailable` response carrying
`available_from = K` and the bootstrap recipe. This preserves the absolute
application coordinate without claiming that the rebuilt DB can serve the
lost prefix.

### 3.1 Landed storage representation

The physical and logical coordinates deliberately remain separate:

- `sequenced_l2_txs.offset` is the append-only SQLite replay/audit cursor.
  Invalidated rows remain, and batch-envelope/cockroach-padding rows exist even
  though the application does not execute them.
- `executed_inputs` is a sparse **current-canonical projection** from an
  executable physical row to its pre-execution `ExecutedInputCount`. User-op
  and direct mappings commit atomically with their existing durability
  transaction; envelopes and padding have no row.
- Standard recovery retains physical audit history but deletes the invalidated
  mapping suffix in the same transaction that bumps generation and opens the
  replacement Tip. `H` therefore rolls back without scanning invalid physical
  history, and replacements reuse the same suffix offsets under the new
  generation.
- Snapshot rows store both physical `l2_tx_index` and canonical
  `executed_input_count`. Registration checks the app count against
  storage-derived `H`; startup checks the loaded dump against the snapshot row;
  catch-up checks every mapping before executing its physical row.

There is no backfill, repair, or neighbor-derived fallback. A missing, extra,
or wrong attribution is a terminal self-invariant failure. This keeps the
API cutover a projection over already-correct durable values rather than
the moment those values first become authoritative.

## 4. HTTP snapshot bootstrap

`GET /latest_snapshot` supplies the latest available application snapshot:
latest valid pending snapshot if present, otherwise finalized. The response
carries the selected dump's canonical count `C` and history version `(e, g)`.
These are selected together with the artifact's lease in one coherent storage
transaction. They describe that artifact at acquisition, not a separately read
live head after the download.

Logical response metadata, with header spellings fixed during the API cutover:

```json
{
  "era_id": "550e8400-e29b-41d4-a716-446655440000",
  "recovery_generation": 7,
  "executed_input_count": 100
}
```

The response body is the application-defined restore artifact. The current
handler opens the single file named by `Application::state_file_in_dump`.
Integration must demonstrate that the delivered artifact restores the intended
replica, including progress; the application contract permits recovery dumps
whose full representation differs from that comparison file. This is an
application/adapter integration obligation, not an egress-owned state format.

The lease protects the artifact through response completion or disconnect.
Once downloaded, the consumer owns its copy and does not depend on the server
retaining that dump. A snapshot exactly at `K` need not remain available: a
retained snapshot at `C >= K` initializes the replica, which replays from `C`.
Recovery during download or restoration can invalidate the claim; subscription
validation handles that race without holding intake or history advancement.

The client verifies the restored application's count against `C`. Cache
validators must distinguish the era and selected artifact, including a rebuilt
era at the same inclusion block. Cached bytes must retain their matching
history metadata; a fresh version lookup must not relabel cached old state.
Any conditional response must preserve that association. Range resumption is a
possible later transport feature, not a prerequisite for cold bootstrap.

`GET /finalized_state` and its metadata route keep serving the watchdog's
comparison workflow. Their artifact metadata and cache identity must likewise
refer to the selected finalized checkpoint. The watchdog's trusted starting
state and independent L1 replay are not replaced by the tip-replica bootstrap.

## 5. WS subscription v2

### 5.1 Admission and continuity

`GET /ws/subscribe?from_offset=N&era_id=e&recovery_generation=g` requires all
three coordinates. The client claims the history associated with its own state
and the exact application boundary it is ready to execute. Validate the claim
against one coherent read of `(e, g, K, H)` before delivering any input.

Apply history checks before offset checks:

| Condition | Response | Client action |
|---|---|---|
| Era differs | `era_changed` | Download and restore a current snapshot. |
| Generation differs | `stale_generation` | Download and restore a current snapshot. |
| `N < K` | `history_unavailable`, with `available_from = K` | Download and restore a current snapshot. |
| `N > H` | `ahead_of_head`, with `live_head = H` | Report the invalid claim; do not wait or silently clamp it. |
| `K <= N <= H` | Admit | Replay inclusively from `N`, then follow new inputs. |

Policy refusals use a typed error response before any data, followed by close
1008. Successful admission sends `hello` with the admitted recovery generation
and coherent available/head boundaries. Exact response fields and header names
are fixed together with the SDK during the cutover. The era is bound by the
mandatory request claim; every message on an admitted session carries that
session's generation. Refusal metadata describes the history observed when
validating the rejected claim.

### 5.2 Replay and resource bounds

Every matching-history offset in `[K, H]` is serveable. For `N < H`, the first
returned input is entry `N`. A request exactly at `H` waits for the next input.
The gold boundary does not gate admission: finalized and soft entries use the
same ordered stream, and advancing finalization requires no client action.

Read canonical pages from committed valid SQLite history, with a bounded page
size and bounded send queue. Preserve the concurrent-subscriber limit. There
is no total catch-up event cap: neither snapshot cadence nor download/restore
time guarantees a backlog below 50,000 inputs, and refetching can return the
same snapshot repeatedly. Memory and concurrency remain bounded independently
of total history depth. A replica must process faster than ongoing production
to reach the tip; another transport cannot remove that capacity requirement.

The client verifies that each event's offset equals its application's current
count, applies the input, and advances that count. It resumes from the next
unapplied input, not the last received network message. A client that persists
its replica must preserve the corresponding history identity with that state.
An offset mismatch is a continuity failure; skipping an input or jumping to a
suggested live head would corrupt the replica.

### 5.3 Snapshot-to-tip walkthrough

1. Download an artifact with `(era=A, generation=7, count=100)` and restore it.
2. Subscribe with `(A, 7, from_offset=100)`. If the head is 105, receive entries
   `100..104`, including any entries already finalized.
3. Continue on the same socket when input 105 commits; no transport handoff or
   special catch-up transition is required.
4. After an ordinary disconnect, reconnect using the state's saved history
   version and actual next-input count. Clean restart preserves that version.
5. If recovery changes the generation, or a rebuild changes the era, the old
   claim is rejected before data. Discard the replica, download a current
   snapshot, and repeat. This also handles recovery between steps 1 and 2.

### 5.4 Events and wire format

Keep the existing denormalized user-op/direct-input context: nonce, fee, safe
block, batch nonce, input index, block timestamp, and transaction hash where
applicable. Replace physical rowids with canonical next-input coordinates.
JSON text messages share a tagged SDK/server enum containing input events,
`hello`, and typed `error` responses. A live-transition message, frame/batch
boundary events, or best-effort invalidation message requires a concrete
consumer need; the basic replay loop and reconnect rules do not rely on them.

## 6. Compatibility boundary

The cutover changes snapshot metadata and the subscription contract together
with the SDK and replica harness. Mandatory history claims and canonical
inclusive offsets replace the optional physical-cursor subscription. Remove
the total catch-up rejection and its suggestion to skip directly to the live
head. An ordinary disconnect permits a same-version resume; recovery requires
a new snapshot in the initial client workflow.

Keep the deployed README truthful until implementation lands. No intermediate
generation-aware physical-rowid API is needed. The storage foundation alone
does not close the consumer invalidation finding.

## 7. Ordered implementation handoff

The [coordination roadmap](2026-07-coordination-tracks.md)
owns PR sequencing. Implement two review boundaries:

1. **Internal read and snapshot foundation (implemented).** Define typed history claims,
   canonical pages, snapshot metadata, and policy errors. Read `(e, g, K, H)`
   coherently, paginate inclusively through `executed_inputs`, and acquire the
   snapshot's count/version with its artifact lease. Preserve physical cursors
   for internal catch-up and recovery. Keep intermediate helpers internal.
   The unused internal canonical reader has a scoped non-test dead-code
   expectation until the next step gives it a runtime caller; remove that
   expectation when connecting WS.
2. **Coordinated consumer cutover (next).** Project snapshot metadata over HTTP and
   require `(EraId, RecoveryGeneration, ExecutedInputCount)` on WS. Admit the
   entire available history range, remove the total catch-up cap, and implement
   typed refusals and fresh-snapshot remediation. Update SDK, replica harness,
   cache validators, and consumer documentation in the same deployable change.
   Fix the exact metadata/error serialization as part of that shared contract.

Required evidence belongs with the change that owns the behavior:

- Coherent metadata and canonical pagination across physical-row holes,
  envelope/padding rows, nonzero era bases, and replacement suffixes.
- Snapshot bytes, count, and version remain associated through transfer and
  cache reuse; restored application count agrees with response metadata.
- Clean restart and ordinary reconnect resume without rebootstrap.
- Stale generation is refused before any data; replacement inputs can reuse
  canonical counts without preserving the invalidated replica.
- A changed era is detected even with the same numeric generation/count, and
  cache validators differ for rebuilt artifacts at the same inclusion block.
- Below-base and ahead-of-head claims receive their typed errors; a claim at
  the head waits normally, and a valid claim below gold is admitted.
- More than 50,000 inputs after the latest snapshot, including direct-heavy
  history, remain replayable with bounded pages and queues.
- Writes during download/restoration and replay-to-live delivery cause no gaps;
  recovery between snapshot acquisition and subscription forces rebootstrap.
- Subscriber limits, disconnect cleanup, snapshot lease lifetime, and watchdog
  finalized comparison behavior remain correct.

Remeasure submit-to-matching-WS-event latency, rewrite the README, graduate the
normative protocol text into `docs/protocol/`, and close the register's WS
invalidation-contract finding only after the cutover and acceptance evidence.
Feed output comes from committed valid SQLite history matching the admitted
history version; it does not depend on a global runtime actor.

## 8. Revisit triggers

- **Archive or historical queries:** evaluate dedicated HTTP replay endpoints
  when a consumer needs capabilities beyond snapshot-to-tip replication.
- **Recovery cost:** consider retaining a known-stable client checkpoint when
  repeated full snapshot restoration is a measured problem.
- **Snapshot transport:** add resumable transfer or another artifact packaging
  only when actual application size/layout and clients require it.
- **Extra stream controls:** add live/frame/batch boundary events only for an
  identified consumer operation that cannot use the existing event context.
- **Access boundary:** authentication and public rate-limit policy require a
  separate decision if egress is exposed beyond operator infrastructure.
- **Runtime lifecycle:** re-evaluate session fencing if recovery can change
  history inside an admitted process or multiple writers are introduced.
