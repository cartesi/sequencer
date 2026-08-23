# Feed & Replay Protocol Design (Track 3)

**Status: accepted architecture; storage/execution foundation landed; public
protocol open.** The team accepted the architecture recorded here; Bart's
consumer review remains open only for the decision gates named in §8. The
history identity `(EraId, RecoveryGeneration)` was accepted 2026-08-01; the
authoritative `Application::executed_input_count` offset and cockroach-base
semantics were accepted 2026-08-02. Durable DB metadata, the typed
application-execution boundary, per-input canonical attribution, and
snapshot/catch-up agreement are
landed. `GET /history-version`, replay routes, and the WS projection remain
open. The consumer bootstrap and `/inputs` questions remain decision gates.
The completed protocol will supersede the ad-hoc WS protocol and close the
open WS invalidation-contract finding (see the review register) only
when the ordered handoff in §7 is complete. At that point, the
normative parts graduate into `docs/protocol/` and the README is rewritten.

## 1. Motivation

The current protocol is an unframed infinite stream of a two-variant enum over
a WS socket, with every session-level signal smuggled into transport close
frames. Three defects drive the redesign:

- **The open invalidation-contract finding (register):** no
  invalidation/rollback signal. Today, recovery
  cascades hide already-streamed rowid-addressed rows and re-sequence
  replacements at higher rowids under a reused batch nonce. A cursor-resumed
  mirror can silently diverge.
- **No historical bootstrap:** the catch-up window is shallow; a consumer
  cannot build state from genesis over the feed.
- **No control plane:** catch-up-to-live is unmarked and continuity errors are
  prose close reasons rather than typed messages.

Consumer model (Bart's libdex is the concrete instance): replay finalized
history via HTTP, then subscribe for the soft tip while maintaining a bit-exact
mirror of application state. Recovery and rebuild boundaries must be detectable
and have explicit remediation.

## 2. Concepts and coordinates

- **Input-box coordinate `input_index: u64`** — position in `safe_inputs`
  (per-application InputBox order). Append-only and sourced from L1 safe blocks.
- **Feed coordinate `offset: ExecutedInputCount`** — the authoritative
  `Application::executed_input_count()` boundary, starting at zero. An
  application at `X` is ready to consume history entry `X`; applying that
  entry advances it to `X + 1`. SQLite now stores this as a sparse canonical
  attribution beside its append-only physical rowid replay log. The current
  public feed still exposes rowid and changes only at the later API cutover.
- **Era base `K`** — the smallest feed offset locally available in this era.
  Genesis setup starts at zero. Cockroach recovery sets it to
  `S'.executed_input_count()` after the fold; absolute offsets continue, but
  the unavailable prefix is not reconstructed. `K` is application history,
  not the snapshot's physical `l2_tx_index`: recovery cursor-padding rows may
  advance the latter without executing an application input.
- **Gold boundary `G`** — within one era, the exclusive executed-input count
  after the scheduler-accepted prefix. Entries with `offset < G` cannot be
  invalidated; `G` only advances.
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
- The protocol will expose the pair through `GET /history-version`; every
  subscribe request will claim it. WS responses repeat only the recovery
  generation, including best-effort in-band `discontinuity` events. None of
  those API changes is part of the landed storage foundation.

Within the same era, a stale generation means “discard and replay the soft
suffix.” Standard recovery restores the retained application state, so its
count rolls back, then advances it over replacement force-drained directs; the
same suffix offsets may therefore name different inputs under the new
generation. A changed era means the client reacquires a current-era
snapshot/bootstrap even if its old state's numeric count happens to be in the
new era's range.

Cockroach recovery now leaves the rebuild base NULL at baseline creation, then
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
public API cutover a projection over already-correct durable values rather than
the moment those values first become authoritative.

## 4. Historical replay endpoints (HTTP, paginated, finalized-only)

Both endpoints serve immutable entries and therefore carry no recovery
generation. Every response carries `era_id`; an entry already returned within
that era never changes. Tail pages and their current end/gold metadata can
advance, so clients and caches must revalidate the current tail rather than
treating every page response as immutable. A client must never splice pages
from different eras.

### 4.1 `GET /inputs?from_index=N&limit=K`

Raw InputBox order over `safe_inputs`: direct inputs and our own batches,
exactly as L1 ordered them, with PR #26 provenance:

```json
{
  "era_id": "550e8400-e29b-41d4-a716-446655440000",
  "items": [
    {
      "input_index": 7,
      "sender": "0x...",
      "payload": "0x...",
      "block_number": 123,
      "block_timestamp": 1700000000,
      "transaction_hash": "0x..."
    }
  ],
  "next_index": 8,
  "end_index": 41
}
```

`end_index` is the current exclusive upper bound. Rows appear once the L1 safe
head passes them.

### 4.2 `GET /l2-txs?from_offset=N&limit=K`

Feed order capped at the gold boundary. Items use the same shapes as WS data
events so replay pages and live events are interchangeable mirror inputs:

```json
{
  "era_id": "550e8400-e29b-41d4-a716-446655440000",
  "items": [],
  "next_offset": 101,
  "gold_boundary": 250
}
```

`from_offset = N` is inclusive: the first item, if present, is history entry
`N`; `next_offset` is the application count after all returned entries. The
server never serves `offset >= G` here. A dedicated indexed query computes `G`
per page. Because it only advances, a stale read is conservative. Requests
below the era base fail with `history_unavailable` rather than pretending the
missing prefix is empty.

## 5. WS subscription v2

### 5.1 Handshake and continuity

`GET /ws/subscribe?from_offset=N&era_id=e&recovery_generation=g` upgrades. All
three coordinates are required: the client claims both the history reality it
holds and the exact application boundary it is ready to execute.

```json
{
  "kind": "hello",
  "recovery_generation": 4,
  "available_from": 100,
  "live_head": 312,
  "gold_boundary": 250,
  "max_subscribers": 64
}
```

- Every server WS message carries the current `recovery_generation`; no WS
  response repeats `EraId`. The era is an admission credential, and a client
  obtains the current value through the bootstrap/history-version path.
- If `era_id` differs, send `era_changed`, then close 1008. Old application
  state cannot be resumed merely because its count is numerically in range;
  the client reacquires the current-era snapshot/bootstrap.
- If the era matches but `recovery_generation` differs, send
  `stale_generation` with the current generation, then close 1008. The client
  rebuilds only its soft suffix above its last known `G`.
- If `from_offset < K`, send `history_unavailable { available_from: K, ... }`,
  then close 1008. The client acquires the snapshot/bootstrap at `K`.

### 5.2 Depth guarantee

Within one era, every `from_offset` in `[G, H]` is serveable: WS supplies the
soft-history entries in `[G, H)`, while a request exactly at `H` joins live and
waits for the next input. Section 8 owns the policy for offsets above `H`. HTTP
replay covers entries below `G`. A request in `[K, G)` receives:

```json
{
  "kind": "error",
  "recovery_generation": 4,
  "error": "below_gold_boundary",
  "gold_boundary": 250
}
```

then a 1008 close. The client loop is:

1. Page `/l2-txs` until history is exhausted at `G0`, verifying one `era_id`
   across every page.
2. Subscribe from `G0` with the pair from `/history-version` (or the
   current-era bootstrap response).
3. On `stale_generation`, discard the soft suffix and return to step 1. On
   `below_gold_boundary`, return to step 1. On `era_changed`, reacquire the
   current-era snapshot/bootstrap and restart.

There is no same-era continuity hole if `G` advances between replay and
subscribe: entries in `[G0, G)` remain finalized and serveable over HTTP, and
the typed `below_gold_boundary` response sends the client back through that
replay. Both HTTP and WS use the same boundary convention: an application at
count `X` requests `X`, and the first returned input is entry `X`.

### 5.3 Events

Data events retain PR #26's denormalized row context (`user_op` /
`direct_input` with nonce, safe block, batch nonce, input index, block
timestamp, and transaction hash). Bart confirmed this field set suffices for a
bit-exact mirror. Normalized frame/batch boundary events remain rejected until
a consumer demonstrates a need.

Every event carries `recovery_generation`. Control events join the same tagged
enum:

- `hello` — §5.1;
- `live` — sent once catch-up reaches the live head;
- `discontinuity { recovery_generation }` — best-effort only; reconnect
  validation is load-bearing;
- `error { error, ... }` — typed policy error followed by close 1008.

### 5.4 Wire format

JSON text frames, serde-tagged by `kind`, with mandatory new fields. We break
old clients freely. `WsTxMessage = BroadcastTxMessage` remains the shared
exhaustive SDK/server type.

## 6. What this replaces

- `WS_CATCHUP_WINDOW_EXCEEDED_REASON` and the `live_start_offset` prose close
  reason — replaced by `below_gold_boundary` plus `/l2-txs`.
- The interim “rebuild on any socket drop” rule — replaced by mandatory
  history-version validation on every subscription.
- README's WS section — rewritten as part of the WS v2 cutover.

## 7. Ordered implementation handoff

> **At-risk dependency:** Bart's 2026-07-28
> confirmation covers the scalar generation contract only. The `EraId` leg —
> changed-era rejection and current-era bootstrap behavior — is explicitly
> unconfirmed by the consumer. The durable schema slice is cheap to carry,
> but treat the era semantics as provisional in every wire-projection step
> below: the wire form is the part that cannot be cheaply migrated once a
> consumer depends on it, so it must not ship ahead of Bart's review.

Track 3 owns the remaining consumer-facing history/feed protocol. The
storage/execution foundation in §3.1 is complete. The current public rowid
contract remains unchanged until the API cutover lands as one deployable
protocol boundary.

1. **Resolve the consumer decision gates.** With Bart, settle the `/inputs`
   representation and the current-era post-cockroach bootstrap artifact/API.
   Decide future-offset behavior before WS v2 and replay authentication/rate
   limits before public exposure. Boundary events remain excluded unless a
   consumer demonstrates a need. Track 3 owns the consumer-visible bootstrap
   contract; Track 6 owns the dump/image representation it may serve.
2. **Add the typed history read/API foundation.** Define shared history-claim,
   page, and policy-error types. Logical boundaries use `EraId`,
   `RecoveryGeneration`, and `ExecutedInputCount`, not interchangeable raw
   `u64` cursors. Back them with one internally consistent SQLite read of
   `(e, g, K, H, G)`, canonical inclusive pagination through
   `executed_inputs`, and raw safe-input pagination with provenance.
3. **Implement HTTP bootstrap and finalized replay.** Add
   `GET /history-version`, `/inputs`, and `/l2-txs`, including era-tagged pages,
   the gold-boundary query, typed below-`K` bootstrap errors, and
   pagination/provenance/forced-cascade immutability tests.
4. **Cut `/ws/subscribe` over to v2.** Require
   `(EraId, RecoveryGeneration, ExecutedInputCount)`, validate the claim before
   admission, serve canonical-coordinate soft history for every admitted offset
   at or above `G`, emit the typed hello/control/error vocabulary, carry
   `RecoveryGeneration` on every response, and remove the
   physical-rowid/string-close contract. Update the SDK and harness in the same
   cutover.
5. **Prove and close the feature.** Replace the behavior-pinning E2Es with
   stale-generation, era-change, below-base, logical-suffix-reuse,
   pagination-hole, and replay-to-live race cases; rewrite the README; graduate
   the normative protocol text; remeasure submit-to-matching-WS-event latency;
   then mark the WS invalidation-contract finding fixed in the register.

Steps 2 and the internal part of 3 can begin before the consumer questions
close. Do not freeze the public below-`K` recovery recipe or `/inputs` response
shape until the relevant questions do. Feed output must come from committed
valid SQLite history and match the requested `HistoryVersion`; it does not
depend on a global runtime actor.

## 8. Decision gates and revisit triggers

1. **Boundary events:** revisit if a consumer needs frame/batch boundaries the
   denormalized rows cannot express.
2. **`/inputs` shape:** confirm how Bart reconciles raw L1 order against feed
   order and settlement.
3. **Post-cockroach bootstrap:** select the current-era snapshot/artifact and
   API a consumer uses when the old era's ordered history is unavailable.
4. **Future offsets:** decide whether `from_offset > H` waits for the head or
   fails with a typed ahead-of-head response. It does not affect the settled
   `X`-means-next-input boundary.
5. **Access policy/limits:** decide whether replay remains
   internal/network-restricted or needs application authentication, and set
   rate limits before public exposure.

Questions 2 and 3 shape step 3 and need Bart's review. Question 4 gates the WS
v2 contract. Question 5 gates public exposure. Question 1 is explicitly
non-blocking unless a consumer brings a concrete requirement.

## 9. Review remarks (non-normative)

- Cockroach recovery cannot currently supply historical user ops as ordered L2
  transactions from genesis. Therefore finalized replay stability is scoped to
  an era. The new era retains the recovered application's absolute count as
  `K`; a changed history version detects the boundary, and offsets below `K`
  fail honestly rather than fabricating the missing feed.
- `EraId` replaces the earlier `instance_id` name because it describes an
  externally visible history era, not a process or machine instance. It is
  UUIDv4; the first-setup timestamp remains separate metadata.
- The reference scheduler count audit is landed: successful directs and user
  ops share one checked typed boundary, overdue-direct ordering is preserved,
  and `AppError` is fatal rather than skipped. The durable per-input mapping is
  also landed; the public feed cutover is not.
