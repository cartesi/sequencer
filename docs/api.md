# Sequencer API

This document owns the sequencer's wire contract: listeners, routes, message
shapes, limits, and status codes. [`openapi.yaml`](../openapi.yaml) renders it
as OpenAPI 3.1 for client generation and documentation tools; change both
together. The [application history guide](protocol/application-history.md)
explains the era and offset coordinates the feed routes use.

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
checksum casing. Address fields in `/history` use lowercase hex. Clients must
compare decoded 20-byte addresses and use one normalized encoding for account or
projection keys across these routes.

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
- the inclusion lane checks, in order, `max_fee` against the open-frame fee, the nonce, then the application's admission policy (the wallet refuses a sender that cannot pay the fee). A refused op returns `422 EXECUTION_REJECTED` with the refusal's text and persists nothing.
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
- `200` while an open frame exists (a running sequencer always has one). Responses carry `Cache-Control: no-store`.
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
- The value is a hint, not a reservation; the application is authoritative. It does not go down within an era; a new era starts its own count (below). After a `422` bad-nonce rejection, which names the expected nonce (`bad nonce: expected N, got M`), query again rather than incrementing.
- It is read from the era's live ops only. After a setup, a sender whose last op lies in the [rebuilt history](protocol/application-history.md#the-rebuilt-history) or below the base count reads 0 until its first live op; sign the nonce the `422` names.
- Successful responses carry `Cache-Control: no-store`. `503` with code `UNAVAILABLE` during shutdown.

## `GET /domain`

The EIP-712 domain `POST /tx` verifies signatures against, keyed as `eth_signTypedData_v4` expects:

```json
{ "name": "CartesiAppSequencer", "version": "1", "chainId": 31337, "verifyingContract": "0x..." }
```

Clients pin their own domain and assert that it matches this one, the way a wallet checks `eth_chainId`. `chainId` is a JSON number, exact in JavaScript below 2^53. That covers every chain browser wallets accept (MetaMask refuses IDs above 4503599627370476); a larger ID fails closed, because a signature over a rounded `chainId` recovers a different sender at `POST /tx`. Do not sign with a served domain unchecked: `chainId` and `verifyingContract` are what keep a signature from replaying on another deployment, and the name and version are the same everywhere.

## `GET /ws/subscribe?era_id=<uuid>&next_input=<u64>`

WebSocket stream of the current application history, replaying from the inclusive
`next_input` offset and then following the optimistic tip. Fetch and restore
`/latest_snapshot` first; its headers supply the complete subscription claim.
After each successfully applied input at offset `X`, persist the claim with
`next_input = X + 1` alongside the replica state.

- Both query fields are required. Missing, malformed or unknown fields, or
  missing or invalid WebSocket upgrade headers, return HTTP `400` with a
  plain-text body.
- An era mismatch, an unavailable prefix, or a position ahead of the head
  returns HTTP `409` before upgrade. The JSON body and `X-History-Error`
  header carry the same typed refusal: `ERA_CHANGED`, `HISTORY_UNAVAILABLE`,
  or `AHEAD_OF_HEAD`. `ERA_CHANGED` names the server's current era, as in
  `{"code": "ERA_CHANGED", "era_id": "22222222-2222-4222-8222-222222222222"}`;
  adopt it at the saved count when `/history` allows it (below), or
  rebootstrap. Rebootstrap on `HISTORY_UNAVAILABLE`. `AHEAD_OF_HEAD` is an
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
  including business failures and malformed-direct no-ops. A direct whose
  payload is over the application's cap never executes and has no offset.
- The era's history starts at its base count: the rows setup rebuilt from L1
  up to `baseline.rebuilt_history_end`, then the live rows, in one offset
  sequence with the same message shapes.
- A stopped process disconnects subscribers. A reconnect must present its
  saved claim; offsets alone cannot distinguish another era's history.
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

## History metadata (internal only)

Readers that maintain additional transfer/order history follow the feed from
the era's base count and keep their own checkpoints, which they restore to
realign after a recovery
([the client's playbook](recovery/cockroach.md#the-clients-playbook)).

`GET /history` returns one coherent view of the deployment, current application
history, immutable era baseline, and latest accepted checkpoint. Optional
`era_id=<uuid>` requires the selected era; a mismatch returns `409 ERA_CHANGED`.
Any other query parameter returns `400`.
Example immediately after a setup from a machine stopped at the stop block:

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
    "era_id": "22222222-2222-4222-8222-222222222222",
    "available_from": 7,
    "head": 7
  },
  "baseline": {
    "l1_stop_block": 1240,
    "l1_end_input_index": 8,
    "next_batch_nonce": 2,
    "rebuilt_history_end": 7
  },
  "accepted_checkpoint": {
    "inclusion_block": 1240,
    "executed_input_count": 7,
    "next_batch_nonce": 2
  }
}
```

- `history.available_from` is the era's base count `K`, the base's L2 tx
  count (0 for a genesis base); entries `[K,head)` are available through WS.
  Counts include all executed application inputs.
- `baseline` describes where setup's rebuild ended: the stop block `C` it
  pinned after flushing the submitter's wallet, the exclusive InputBox end `R`
  through `C`, the scheduler's next batch nonce at the end of `C`, and
  `rebuilt_history_end` `E`, the count at the end of `C`. The rebuilt history
  covers `[K,E)` and the live rows follow. It survives baseline artifact GC,
  and it is distinct from the moving safe head.
- `accepted_checkpoint`, when available, has `inclusion_block`,
  `executed_input_count`, and `next_batch_nonce`, under `history.era_id`.
  Until a batch of the era is accepted, it is the era baseline: block `C`,
  count `E` and the baseline's nonce, since the canonical machine reaches the
  same state at the end of `C`. The metadata does not lease or download a native artifact
  and does not certify a client projection. A known divergence returns `503`.

Inside an era the history only appends, so a saved state at any count `X`
with `K <= X <= head` resumes with the claim it was saved with: its era id and
`next_input = X`. The [history contract](protocol/application-history.md#identity-and-available-history)
owns this rule.

A client of an older era (its claim refused with `ERA_CHANGED`) holding state at
count `X` adopts the current era when `K <= X <= E`: it subscribes with the
current `history.era_id` and `next_input = X`. Below `K`, the era's base is
newer than its state; above `E`, its state holds rows the old instance served
that never landed by `C`, even when `head` is past `X`. Both need another
starting state. The check compares counts only: a state that is wrong below
its count passes it, and only comparing the feed's rows from `K` with its own
exposes that. The [history contract](protocol/application-history.md#adopting-a-new-era)
defines the rule; the server adds no check of its own.

The Rust SDK exposes `history(expected_era)` and
`adopt_current_era(count)` with typed metadata, adoption and era refusals.
Both use the configured request deadline, including body transfer. The client
owns persistence, checkpoint selection, and subscription.

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
- `GET /latest_snapshot` — streams a restorable tar archive of the newest
  batch-close snapshot, or the era baseline. Includes immutable `info.toml`
  and the application's opaque `state` file or directory.
- `GET /finalized_snapshot` — streams the accepted snapshot as a tar archive,
  adding a coherent `checkpoint.toml` receipt with its L1 inclusion block and
  next batch nonce: an operator backup, which no command consumes.

Successful state/archive downloads include `X-History-Era` and
`X-Executed-Input-Count`, selected in the read that takes the artifact lease.
Streaming holds the lease until the response ends or the client disconnects.
Bodies stream without `Content-Length`; a failure after the headers aborts the
transfer, so treat an incomplete body as a failed download.
A set-up sequencer always has a comparable checkpoint: until a batch of the era
is accepted, it is the era baseline at the stop block, genesis base included,
so a `404` from the accepted endpoints means the wrong listener or a database
setup never completed. Known divergence makes
all four finalized endpoints return `503 UNAVAILABLE`, including conditional
state requests. The check shares the checkpoint-selection read, before any
lease or archive is created. See [snapshot lifecycle](snapshots/lifecycle.md).

## Health probes (internal only)

- `GET /livez` returns `200` whenever the handler responds, with an empty body.
- `GET /readyz` returns `200` while the inclusion-lane receiver is open and
  shutdown has not been requested; otherwise `503`. Its body is empty.
- `GET /healthz` uses the same status as `/readyz` and returns JSON:
  `{ "status": "ok", "inclusion_lane": "ok", "landing_lag_blocks": 40, "max_wait_blocks": 7200, "safe_head_age_seconds": 780 }`.
  `status` becomes `"degraded"`
  for either failure condition; `inclusion_lane` becomes `"stopped"` once its
  receiver is closed. The lane closes it soon after shutdown is requested, so it
  can still read `"ok"` early in a shutdown.
- `landing_lag_blocks`, `max_wait_blocks` and `safe_head_age_seconds` are the
  [landing lag](recovery/README.md#what-the-sequencer-does), the
  application's window, and the safe block's age by the wall clock.
  The history is void when `landing_lag_blocks` reaches `max_wait_blocks`.
  `landing_lag_blocks` and `safe_head_age_seconds` are `null` until the first
  reading after start, about two seconds. They do not change the status code.

These probes cover process reachability, the lane channel, and shutdown state,
and report the landing lag and the safe head's age for monitoring to alert on;
they set no threshold of their own. They do not certify submitter balance or
canonical agreement.
