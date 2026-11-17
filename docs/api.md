# Sequencer API

This document owns the sequencer's wire contract: listeners, routes, message
shapes, limits, and status codes. [`openapi.yaml`](../openapi.yaml) renders it
as OpenAPI 3.1 for client generation and documentation tools; change both
together. The [application history guide](protocol/application-history.md)
explains the era, generation, and offset coordinates the feed routes use.

## Listeners

The sequencer serves two listeners. The public one
(`CARTESI_SEQUENCER_HTTP_ADDR`) serves only `POST /tx`, `GET /fee`,
`GET /nonce`, and `GET /domain`. Every other route, health probes included, is
internal and served only on the internal listener
(`CARTESI_SEQUENCER_INTERNAL_HTTP_ADDR`), without authentication; keep it off
the public network.

## Errors

Errors use one JSON shape, `{ "ok": false, "code": "<CODE>", "message": "<text>" }`,
with these codes: `400` `BAD_REQUEST` or `INVALID_SIGNATURE`, `413`
`PAYLOAD_TOO_LARGE`, `422` `EXECUTION_REJECTED`, `429` `OVERLOADED`, `500`
`INTERNAL_ERROR`, and `503` `UNAVAILABLE`. A `500` message is fixed and carries
no internal detail. Exceptions: history-policy `409` bodies carry the typed
refusal (`{ "code": "ERA_CHANGED", ... }`), the `/ws/subscribe` pre-upgrade
`400` is plain text, snapshot-route `404` and `500` responses have empty bodies,
and the health probes use their own shapes.

## Address encoding

JSON `sender` fields in successful `POST /tx` and `GET /nonce` responses and
WebSocket messages, and `verifyingContract` in `GET /domain`, use EIP-55
checksum casing. Address fields in `/history` and `sender` fields in
`/historical-l1-inputs` use lowercase hex. Clients must compare decoded 20-byte
addresses and use one normalized encoding for account or projection keys across
these routes.

## `POST /tx`

Request shape:

```json
{
  "message": {
    "nonce": 0,
    "max_fee": 1,
    "data": "0x..."
  },
  "signature": "0x...",
  "sender": "0x..."
}
```

Notes:

- `signature` must be 65 bytes.
- `sender` is required and must match the recovered signer.
- `message.data` is SSZ-encoded method payload bytes.
- size is bounded at ingress, before the hot path: the raw request body is capped at 4096 bytes (`413 PAYLOAD_TOO_LARGE`), and `message.data` at the application's maximum method payload size (`400 BAD_REQUEST`).
- the inclusion lane checks nonce, `max_fee`, and fee balance. A rejected op returns `422 EXECUTION_REJECTED` and persists nothing.
- overload is enforced at queue admission: if the inclusion-lane queue is full, `POST /tx` returns HTTP `429` with code `OVERLOADED` and message `queue full`.
- queue capacity is an internal runtime constant tuned alongside inclusion-lane chunking to absorb short bursts; if this starts triggering persistently, it is a signal to revisit runtime sizing or throughput rather than add another admission layer.
- Browser wallets can call `POST /tx`, `GET /fee`, `GET /nonce`, and `GET /domain` from any origin with any request headers; preflight permits GET and POST and is cached for one hour. CORS is applied only to ingress. Egress routes remain operator-only and require network access controls.

Success response after inclusion:

```json
{
  "ok": true,
  "sender": "0x...",
  "nonce": 0
}
```

## `GET /fee`

Fee quote for setting signed user-op `max_fee` before `POST /tx`. All three fields are log-space exponents (base 129/128), the same encoding as `max_fee`. Inclusion rejects any op with `max_fee` below the open-frame `fee`.

```json
{ "fee": 1356, "recommended_fee": 1356, "suggested_max_fee": 1409 }
```

Notes:

- `fee` is frozen for the lifetime of the open frame (the live inclusion check).
- `recommended_fee` is what the next frame will sample at rotation (currently after five newly-safe L1 blocks, best-effort).
- `suggested_max_fee` is `max(fee, recommended_fee)` plus 1.5× log-space slack. Wallets can copy this into signed `max_fee`; the user pays the frame fee, not this cap. Clients that want their own policy can ignore it and combine the two facts themselves.
- `200` while an open frame exists (the admitted runtime always has one). Responses carry `Cache-Control: no-store`.
- `503` with code `UNAVAILABLE` during shutdown, or if no open frame exists.

## `GET /nonce?sender=<address>`

The nonce to sign into `sender`'s next user op.

```json
{ "sender": "0xAbC...", "next_nonce": 7 }
```

Notes:

- `sender` is parsed like the `POST /tx` field: `0x` plus 40 hex digits in any case, echoed in EIP-55 casing. A missing or malformed parameter is `400` with code `BAD_REQUEST`.
- `next_nonce` is one past the sender's latest included op, or 0 if it has none. It counts soft-confirmed ops: an op acknowledged with `200` before this request is counted. (The `nonce` in a `POST /tx` response or WS message is instead the nonce that op consumed.)
- Keep one op in flight per sender: sign `next_nonce`, submit, and increment on `200`. Concurrent submits from one sender are unsupported; they can reach the sequencer out of order and be rejected. After a submit times out, a `next_nonce` above the op's nonce means it was included; an unchanged one is inconclusive, since the op may still be queued, so resubmit the same signed op rather than signing a new one at that nonce.
- `next_nonce` of 4294967295 (`u32::MAX`) means the account is exhausted: that nonce has no successor, so `POST /tx` can include no further op from it.
- The value is a hint, not a reservation; the application is authoritative. It can go down, because a restart that runs automatic recovery may invalidate soft-confirmed ops. After a `422` bad-nonce rejection, query again rather than incrementing.
- After an operator rebuild from a checkpoint (`setup --recovery`), a sender with no surviving op since the rebuild reads 0 even when its nonce in the rebuilt baseline is higher; this includes a sender whose post-rebuild ops a later recovery invalidated. A `422` bad-nonce rejection names the expected nonce (`bad nonce: expected N, got M`).
- Successful responses carry `Cache-Control: no-store`. `503` with code `UNAVAILABLE` during shutdown.

## `GET /domain`

The EIP-712 domain `POST /tx` verifies signatures against, keyed as `eth_signTypedData_v4` expects:

```json
{ "name": "CartesiAppSequencer", "version": "1", "chainId": 31337, "verifyingContract": "0x..." }
```

Clients pin their own domain and assert that it matches this one, the way a wallet checks `eth_chainId`. `chainId` is a JSON number, exact in JavaScript below 2^53. That covers every chain browser wallets accept (MetaMask refuses IDs above 4503599627370476); a larger ID fails closed, because a signature over a rounded `chainId` recovers a different sender at `POST /tx`. Do not sign with a served domain unchecked: `chainId` and `verifyingContract` are what keep a signature from replaying on another deployment, and the name and version are the same everywhere.

## `GET /ws/subscribe?era_id=<uuid>&recovery_generation=<u64>&next_input=<u64>`

WebSocket stream of the current application history, replaying from the inclusive
`next_input` offset and then following the optimistic tip. Fetch and restore
`/latest_snapshot` first; its headers supply the complete subscription claim.
After each successfully applied input at offset `X`, persist the claim with
`next_input = X + 1` alongside the replica state.

- All three query fields are required. Missing or malformed fields, or missing
  or invalid WebSocket upgrade headers, return HTTP `400` with a plain-text body.
- An era or generation mismatch, an unavailable prefix, or a position ahead of
  the head returns HTTP `409` before upgrade. The JSON body and `X-History-Error`
  header carry the same typed refusal: `ERA_CHANGED`, `STALE_GENERATION`,
  `HISTORY_UNAVAILABLE`, or `AHEAD_OF_HEAD`. Rebootstrap on `ERA_CHANGED` or
  `HISTORY_UNAVAILABLE`. On `STALE_GENERATION`, a compatible saved checkpoint
  can first be selected through `/history` (below). `AHEAD_OF_HEAD` is an
  invalid claim to correct.
- A claim exactly at the head waits for the next input. Replay uses bounded
  pages and queues, with no total catch-up limit. The subscriber cap is `64`.
- Before upgrade, checks run in order: query and upgrade headers (`400`),
  shutdown (`503 UNAVAILABLE`), the subscriber cap (`429 OVERLOADED`), then the
  claim (`409`, or `503 UNAVAILABLE` on an operational subscription failure).
  At capacity, a stale claim sees `429` until a slot frees.
- Messages are JSON text frames; binary fields are `0x`-prefixed hex.
  Direct-input `block_timestamp` values are Unix seconds.
- Batch envelopes are absent. Offsets count executed application inputs,
  including business failures and malformed-direct no-ops.
- Recovery stops the process and disconnects subscribers. A reconnect must
  present its saved claim; offsets alone cannot distinguish a replaced suffix.
- Shutdown or a feed read/send failure may disconnect without a WebSocket
  Close frame. Resume from the saved claim after an unexpected disconnect;
  a clean close is not required for safe replay.

Message shapes:

```json
{ "kind": "user_op", "offset": 10, "sender": "0x...", "nonce": 7, "fee": 1, "data": "0x...", "safe_block": 123, "batch_nonce": 4 }
```

```json
{ "kind": "direct_input", "offset": 11, "sender": "0x...", "block_number": 123, "block_timestamp": 1700000000, "transaction_hash": "0x...", "payload": "0x...", "input_index": 42, "batch_nonce": 4 }
```

## History metadata and historical L1 inputs (internal only)

Readers that maintain additional transfer/order history can reconstruct it from
L1 and then join the application feed. The
[projection replay contract](protocol/projection-replay.md) describes
bootstrap, client checkpoints, pending directs, and terminal drain.

`GET /history` returns one coherent view of the deployment, current application
history, immutable era baseline, and latest accepted checkpoint. Optional
`era_id=<uuid>` requires the selected era; a mismatch returns `409 ERA_CHANGED`.
Example immediately after a rebuild:

```json
{
  "deployment": {
    "chain_id": 31337,
    "app_address": "0x1111111111111111111111111111111111111111",
    "input_box_address": "0x2222222222222222222222222222222222222222",
    "app_deployment_block": 1,
    "batch_submitter_address": "0x3333333333333333333333333333333333333333"
  },
  "history": {
    "version": {
      "era_id": "22222222-2222-4222-8222-222222222222",
      "recovery_generation": 0
    },
    "available_from": 7,
    "head": 7
  },
  "baseline": {
    "l1_stop_block": 1240,
    "l1_end_input_index": 8,
    "next_batch_nonce": 2
  },
  "accepted_checkpoint": null,
  "compatibility": null
}
```

- `history.available_from` is baseline application count `K`; entries `[K,head)`
  are available through WS. Counts include all executed application inputs.
- `baseline` describes the fixed L1 stopping block `C`, exclusive InputBox end
  `R`, and scheduler nonce after recovery's terminal drain. It survives generation
  changes and baseline artifact GC. It is distinct from the moving safe head.
- `accepted_checkpoint`, when available, has `inclusion_block`,
  `executed_input_count`, and `next_batch_nonce`, under `history.version`.
  Genesis supplies the zero checkpoint; a rebuilt baseline is not itself an
  accepted checkpoint. The metadata does not lease or download a native artifact
  and does not certify a client projection. A known divergence returns `503`.
- `compatibility` is `null` unless `from_generation=<u64>` is supplied together
  with `era_id`. It then contains `from_generation` and `preserved_input_count`:
  the prefix that survived every standard recovery since that generation,
  bounded by the current head. A future generation or missing era returns
  `400 BAD_REQUEST`; an era mismatch takes precedence over the generation bound.

For example, `GET /history?era_id=<uuid>&from_generation=0` can return
`"compatibility": {"from_generation": 0, "preserved_input_count": 3}`.
A saved checkpoint from that era/generation is reusable when its count `X`
satisfies `K <= X <= 3`. The boundary is inclusive: the checkpoint has executed
entries before `X`, and resumes at entry `X`. Each checkpoint must be checked
using its own saved generation. With no intervening recovery, the bound is the
current head. The [history contract](protocol/application-history.md#checkpoint-compatibility-after-standard-recovery)
defines the calculation and trust boundary.

Restore an eligible checkpoint, persist the response's current history version
with it, and subscribe using that version and its actual count. A recovery
between lookup and subscription still returns `STALE_GENERATION`; repeat the
lookup using the version associated with the restored state. Compatibility does
not certify the client's application or projection implementation, and cannot
cross a cockroach recovery's new era.

`GET /historical-l1-inputs` requires `era_id` and exactly one starting selector:

- `next_input_index=<u64>`: inclusive per-application InputBox index, starting at 0.
- `after_block=<u64>`: initially seek to the first input strictly after that block;
  continue using the returned `next_input_index`.

The endpoint serves only `[0,R)` through the selected era's `C`. A response to
`next_input_index=5&limit=1` can be:

```json
{
  "era_id": "22222222-2222-4222-8222-222222222222",
  "l1_stop_block": 1240,
  "end_input_index": 8,
  "next_input_index": 6,
  "items": [{
    "input_index": 5,
    "sender": "0x3333333333333333333333333333333333333333",
    "payload": "0x00",
    "block_number": 1230,
    "block_timestamp": 1700014760,
    "transaction_hash": "0x4444444444444444444444444444444444444444444444444444444444444444"
  }]
}
```

Records preserve original inner payloads and authenticated senders, including
malformed/rejected batches; they are not complete `EvmAdvance` envelopes. Indices
are contiguous and ordered. Binary values are hex; timestamps are Unix seconds.
Clients must preserve integer precision. A page may split a block.

Optional `limit` defaults to 256 and accepts 1–256. Pages target 1 MiB of raw
payloads; a larger first input is returned alone, intact. Hex encoding increases
wire size, so this is not a hard response-size limit. Eight historical responses
can be in flight; a permit remains held through body delivery or cancellation.
SQLite read transactions end before network delivery. These limits bound memory
by the page target or largest single input, not total history length.

Only `next_input_index == end_input_index` means EOF; a short page does not.
Requesting `next_input_index=R` or `after_block=C` returns an empty completed page.
Generation changes do not invalidate historical pages; an era change does.

Malformed/unknown query fields, invalid selectors/limits, or positions above
`R`/`C` return the existing `400 BAD_REQUEST` JSON shape. An era mismatch returns
the existing `409 ERA_CHANGED` history-policy body before semantic position
checks. Capacity exhaustion returns `429 OVERLOADED`; shutdown or an operational
read failure returns `503 UNAVAILABLE`. Interrupted bodies are failed pages.
Missing durable rows or other storage invariant failures follow the process's
terminal fault policy, never a successful partial page.

The Rust SDK exposes `history(expected_era, from_generation)` and
`historical_l1_inputs(era, start, limit)` with typed metadata and era refusals.
Both use the configured request deadline, including body transfer; callers may
increase it for large historical inputs. The client owns replay, persistence,
checkpoint selection, and subscription.

## Operator snapshot endpoints (internal only)

These serve application state to the operator's watchdog and indexers.
**They are operator-internal — no auth — and must not be exposed publicly**:
they are served only on the internal listener, behind network controls.

- `GET /finalized_state/inclusion_block` — cheap JSON the watchdog polls:
  `{ "inclusion_block": <u64>, "executed_input_count": <u64> }`.
- `GET /finalized_state/digest` — the SHA-256 of the accepted checkpoint's
  comparison file, as lowercase hex, hashed under the same lease:
  `{ "inclusion_block": <u64>, "executed_input_count": <u64>, "sha256": "<hex>" }`.
  The watchdog replays to that block and compares digests.
- `GET /finalized_state` — streams the accepted checkpoint's comparison file
  (`application/octet-stream`), with `X-Inclusion-Block`,
  `X-Executed-Input-Count`, and `ETag: "block-<n>"` (`If-None-Match` supports `304`).
  The watchdog downloads it only as divergence evidence.
- `GET /latest_snapshot` — streams a restorable tar archive of the newest valid
  batch-close snapshot, or the era baseline. Includes immutable `info.toml`
  and the application's opaque `state` file or directory.
- `GET /finalized_snapshot` — streams the accepted snapshot as a tar archive,
  adding a coherent `checkpoint.toml` receipt with its L1 inclusion block and
  next batch nonce for trusted recovery.

Successful state/archive downloads include `X-History-Era`, `X-Recovery-Generation`,
and `X-Executed-Input-Count`, selected atomically with the artifact lease.
Streaming holds the lease until the response ends or the client disconnects.
Bodies stream without `Content-Length`; a failure after the headers aborts the
transfer, so treat an incomplete body as a failed download.
The accepted endpoints return `404` until a comparable checkpoint exists:
genesis is comparable at block zero; a rebuilt baseline is restorable but only
a later accepted batch establishes a comparison point. Known divergence makes
all four finalized endpoints return `503 UNAVAILABLE`, including conditional
state requests. The check shares the checkpoint-selection transaction, before
any lease or archive is created. See [snapshot lifecycle](snapshots/lifecycle.md).

## Health probes (internal only)

- `GET /livez` returns `200` whenever the handler responds, with an empty body.
- `GET /readyz` returns `200` while the inclusion-lane receiver is open and
  shutdown has not been requested; otherwise `503`. Its body is empty.
- `GET /healthz` uses the same status as `/readyz` and returns JSON:
  `{ "status": "ok", "inclusion_lane": "ok" }`. `status` becomes `"degraded"`
  for either failure condition; `inclusion_lane` becomes `"stopped"` once its
  receiver is closed. The lane closes it soon after shutdown is requested, so it
  can still read `"ok"` early in a shutdown.

These probes cover process reachability, the lane channel, and shutdown state.
They do not certify L1 freshness, submitter balance, or canonical agreement.
