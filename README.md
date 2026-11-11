# Sequencer

A sequencer for Cartesi app-specific rollups. Provides low-latency soft confirmations for user operations, posts them to L1 in batches, and exposes its current application execution order for replica replay.

**Security-critical infrastructure.** Handle every change with the care financial systems demand.

## What It Does

Rollup applications need fast transaction confirmations. Waiting for L1 finality on every user action (minutes) makes interactive applications impractical. The sequencer bridges this gap: it accepts signed user operations, immediately confirms them (soft confirmation), and asynchronously posts batches to L1. The application sees these batches posted on chain.

The protocol objective is that, under supported honest operation, **the
off-chain sequencer predicts the same execution order the rollup's on-chain
scheduler later produces.** Soft confirmations are optimistic and may be
revoked by the recovery described below; L1 remains canonical truth.

## Two Chains Synchronizing

The sequencer maintains an optimistic list of batches, which L1 accepts in order. Each batch contains frames, and each frame contains user operations plus a `safe_block` reference. The `safe_block` is the synchronization primitive: it tells the on-chain scheduler "drain all direct inputs (deposits) up to this L1 block, then execute these user ops." Both sides follow the rule, producing identical state.

```
Sequencer (off-chain)              Scheduler (on-chain)
  frame: safe_block=100               drain directs up to block 100
         user_ops=[A, B, C]           execute A, B, C
  frame: safe_block=105               drain directs up to block 105
         user_ops=[D]                 execute D
```

When things go well, the sequencer's chain and the scheduler's view converge.
When a batch can no longer land before it goes stale, or local state is lost or
untrustworthy, an operator rebuilds the sequencer, as described below.

## Trust Model

The sequencer is a **centralized, single-writer** system. It cannot steal funds or forge invalid state — the rollup validates everything independently, and the proof system later enforces it. But the sequencer can:

- **Censor** — refuse to include a user's operations.
- **Go offline** — stop providing soft confirmations.
- **Diverge** — if batches fail to land on L1 in time, soft confirmations that were issued become invalid.

**Direct inputs** (L1 → L2 messages, used for deposits) bypass the sequencer entirely. They are posted directly to L1 and are **uncensorable** by the sequencer — the scheduler drains them at every `safe_block` boundary. A censoring sequencer can delay when a direct input is executed (up to the application's `max_wait_blocks`, ~4h for the wallet), but cannot prevent it. A direct whose payload is over the application's cap
never executes, on L1 or in the sequencer.

During normal operation the sequencer advances logical frame time after five
newly-safe blocks have accumulated. That clock tick drains every covered direct
before later user ops and may also create an empty-direct frame, so that later
user ops carry a fresher block: each op executes at its frame's safe block. Safe-head publication is best effort: if the node
exposes a multi-block jump, the sequencer creates one frame at the observed tip
and never fabricates intermediate frames.

Soft confirmations are an **optimistic prediction**. A narrow content-identity
check compares each batch that L1 accepts at the sequencer's nonces with the
batch it sealed locally. A foreign or byte-different landing freezes the
accepted frontier and stops the runtime, and every later boot refuses until an
operator performs cockroach recovery. The check does not replace the watchdog;
its bounds are recorded in [`docs/invariants.md`](docs/invariants.md) (I9 and I15).

## Recovery

The sequencer has one recovery, **[cockroach recovery](docs/recovery/cockroach.md)**:
an operator-triggered rebuild when the era has ended or the local database is
lost or cannot be trusted, including after a sequencer bug that corrupted state
or emitted malformed batches. The operator
runs `setup --base <machine>` in a new data directory: setup rebuilds the
history from a trusted canonical machine and L1 through a stop block and
publishes a new era, and each client realigns at its own count.

An era ends when its oldest unaccepted batch can no longer land before it goes
stale, the **void history**: no later batch can then be accepted, so the
sequencer stops and refuses every start
([batch landing and outages](docs/recovery/README.md)).

The [threat model](docs/threat-model/README.md#self-trust) explains the boundary
between normal operation's self-trust and manual repair after a bug.

## Failure Modes

The sequencer is designed to handle:

- **L1 provider outages** — each worker retries on its next tick; the RPC client resends a rate-limited or temporarily unavailable request at most twice, a second apart or after the server's requested wait when that is 5 s or less. The inclusion lane and API continue operating locally. The sequencer acts only on what L1 shows: it reports the safe head's age for monitoring and makes no wall-clock estimate ([outages](docs/recovery/README.md#when-batches-are-not-landing)).
- **Undiagnosed interruptions (OOM, SIGKILL, reboot)** — restart resumes: every boot syncs L1 and checks the terminal facts from SQLite and L1 safe state, never assuming the previous exit was clean ([what the sequencer does](docs/recovery/README.md#what-the-sequencer-does)). Terminal errors returned through a command bracket best-effort record their cause in `terminal_faults`; terminal runtime aborts leave only process diagnostics.
- **Extended downtime** — startup syncs to the current L1 safe head and resumes. A stop longer than the window ends the era, and every start refuses until cockroach recovery, above.
- **Adversarial L1 mempool** — block builders and private mempools are treated as adversarial. No batch nonce is reused inside an era, so a delayed "zombie" submission carries a batch the sequencer still holds. Before a rebuild, setup's flush replaces what the provider's pool holds for the submitter; a submission of the old era that escapes it is refused by setup's escape check or, if it lands later, read as a foreign batch that ends the new era ([the flush and the stop block](docs/recovery/cockroach.md#the-flush-and-the-stop-block)).

## Interfaces

### User Operations

Users submit signed operations via `POST /tx` (JSON). Operations are signed with EIP-712 using the rollup's chain ID and app address. The sequencer validates the signature, executes the operation against the current app state, and returns a soft confirmation. To sign, a wallet reads `GET /fee` (the live frame fee, the next-frame recommendation, and a suggested `max_fee`), `GET /nonce?sender=` (the nonce to sign next), and `GET /domain` (the EIP-712 domain, to assert against the one it pins).

### Sequenced Transaction Feed

Subscribers restore an HTTP snapshot, then use one WebSocket stream to replay
application inputs and follow the optimistic tip. A cockroach rebuild starts
a new era; the snapshot's era and input count bind a resume request
to the state the consumer actually holds. The endpoint serves a small number of
infrastructure subscribers, which serve users directly. See the
[bootstrap workflow](docs/protocol/application-history.md#replica-bootstrap-and-resume)
and [wire contract](#api).

### Batch Submission

The batch submitter posts closed batches to L1's InputBox contract. Each batch carries a sequential nonce for deduplication; L1 wallet nonces guarantee ordering. The submitter is stateless — it derives pending work from SQLite and L1 state each tick.

## Running

The sequencer runs in two phases. **`setup`** creates a data directory: it
pins the deployment identity (including the reviewed fee-oracle source and
the application's staleness window and direct-payload cap),
flushes the submitter's wallet to pin a stop block, syncs L1 through it,
rebuilds the history from a base through it, and publishes the era's baseline.
The base is genesis, or a stored canonical machine with `--base <MACHINE_DIR>`
([cockroach recovery](docs/recovery/cockroach.md)). Setup runs once per data
directory: it refuses one that holds anything, a completed setup included, with
exit 30, and never touches it. `--dry-run` prints what setup would rebuild,
takes no key, and never touches the data directory. **`run`** boots the
sequencer from the set-up DB, reading identity from it (so chain id / app
address are not `run` arguments); it refuses, with exit 30, a binary whose
staleness window or direct-payload cap differs from the pinned one. Both take the signing key:
setup's flush and `run`'s submitter sign for the batch-submitter address.

```bash
# Phase A — set up a new data dir (once; it refuses a non-empty one).
CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT=http://127.0.0.1:8545 \
CARTESI_SEQUENCER_BLOCKCHAIN_ID=31337 \
CARTESI_SEQUENCER_APP_ADDRESS=0x1111111111111111111111111111111111111111 \
CARTESI_SEQUENCER_FEE_ORACLE_FIXED_LOG_GAS_PRICE=0 \
CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS=0xa0Ee7A142d267C1f36714E4a8F75612F20a79720 \
CARTESI_SEQUENCER_AUTH_PRIVATE_KEY=0x2a87...d409c6 \
cargo run -p wallet-sequencer -- setup

# Phase B — run the sequencer.
CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT=http://127.0.0.1:8545 \
CARTESI_SEQUENCER_AUTH_PRIVATE_KEY=0x2a87...d409c6 \
cargo run -p wallet-sequencer -- run
```

`setup` exits 20 (retryable) until the application's deployment block is at or below the L1 safe head, so no reorg can move the block it pins; on Ethereum that is a one-time wait of an epoch or two after deployment, and on Anvil `--slots-in-an-epoch` mined blocks (one in the repo's devnet stack, which sets it to 1; Anvil's default is 32). Setup runs that check and its chain-id check before it writes anything, so a retry in the same directory is safe; a setup that fails after it begins writing, whatever its exit code, leaves a directory to discard ([what setup does](docs/recovery/cockroach.md#what-setup-does)).

The sequencer links the Cartesi Machine emulator statically, to read a base machine: building it needs the emulator's library at `LIBCARTESI_PATH` (the dev shell exports it), and a release binary needs the host libraries its archive's `RUNNING.md` lists, not an emulator install.

A third subcommand, **`flush-mempool`**, settles the batch-submitter wallet
nonce on demand (keyed operator tool). It is flush-only: it requires a
completed setup and no canonical divergence, and it launches no runtime
workers.

`setup` requires: `CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`, `CARTESI_SEQUENCER_BLOCKCHAIN_ID`, `CARTESI_SEQUENCER_APP_ADDRESS`, `CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS`, and the signing key, `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY` or `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE`, which must sign for that address (`--dry-run` takes no key). On chains other than Ethereum mainnet and Sepolia it also requires a fee-oracle source (listed below).
The submitter address must equal the `sequencer_address` compiled into the canonical machine; nothing checks this at runtime ([scheduler semantics](docs/protocol/scheduler-semantics.md#the-one-stream-the-one-classification)).

**Use a dedicated submitter address.** Setup flushes the submitter's wallet: it
replaces whatever that account has pending with no-op transactions, then
refuses with exit 40 if the wallet is still unsettled. Sharing a busy
contract-deployer address would cancel its pending deployment transactions or
trip that refusal. The devnet uses Anvil
account 9 (`DEVNET_SEQUENCER_ADDRESS`), separate from the account-0 deployer.
The Sepolia reference reserves [`SEPOLIA_SEQUENCER_ADDRESS`](examples/app-core/src/application/wallet.rs)
for this deployment's batch submissions and application fee revenue; do not share
its signing key with other app or node instances. Changing that compiled address
requires a rebuilt canonical image and a matching new application deployment,
not just a runtime signing-key change.

`run` and `flush-mempool` require `CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT` and the signing key, `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY` or `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE` (whose first line is the key); both refuse to start until `setup` has completed.

Optional variables, by name; each subcommand's `--help` shows their defaults and validation from [`commands/config.rs`](sequencer/src/commands/config.rs):

- Every subcommand: `CARTESI_SEQUENCER_DATA_DIR` (the SQLite file is `sequencer.db` inside; `setup` creates it and refuses a directory that holds anything but `sequencer.lock`, and `run` and `flush-mempool` refuse a missing one), `CARTESI_SEQUENCER_SECONDS_PER_BLOCK`, and `CARTESI_SEQUENCER_ALLOW_INSECURE_RPC` (below).
- `setup`: the fee-oracle source it pins, either `CARTESI_SEQUENCER_FEE_ORACLE_FIXED_LOG_GAS_PRICE` or a Uniswap source (the overrides `CARTESI_SEQUENCER_FEE_TOKEN_ADDRESS`, `CARTESI_SEQUENCER_WETH_ADDRESS` and `CARTESI_SEQUENCER_UNISWAP_V3_POOL`, given together and required on other chains, and `CARTESI_SEQUENCER_FEE_ORACLE_TWAP_WINDOW_SECS`); and two flags with no variable, `--base <MACHINE_DIR>` (a stored canonical machine to rebuild from; genesis without it) and `--dry-run`.
- `run`: `CARTESI_SEQUENCER_HTTP_ADDR` (public listener), `CARTESI_SEQUENCER_INTERNAL_HTTP_ADDR` (internal listener), `CARTESI_SEQUENCER_MAX_BATCH_OPEN_SECONDS`, `CARTESI_SEQUENCER_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`, `CARTESI_SEQUENCER_BATCH_SUBMITTER_CONFIRMATION_DEPTH` and `CARTESI_SEQUENCER_FEE_ORACLE_POLL_INTERVAL_MS`.

Logs and errors show only the endpoint's scheme, host and port, since its path, query and userinfo can hold an API key or a password, unless the RPC server echoes them inside a JSON-RPC error message or a malformed result, which alloy builds above the redaction layer, or quotes in an error body only part of them or a different URL that carries them, such as a redirect target on another path. An L1 error's text stays on one line, with each cause cut at 256 bytes. The client follows no redirects: an endpoint that answers 3xx fails with that status, so configure the provider's final URL (setup exits 30 at its chain-id check, since a retry gets the same redirect). Debug-level logging for the `alloy_transport_http` target records the full URL, so keep that target at `info` where logs leave the host.

By default the blockchain endpoint must be `https://` unless its host is loopback (`localhost`, `127.0.0.0/8`, `::1`) — a guard against accidentally sending L1 traffic to a public RPC in the clear. Set `CARTESI_SEQUENCER_ALLOW_INSECURE_RPC=true` (or `--allow-insecure-rpc`) to permit plaintext `http://` to a non-loopback host on a **trusted private network** — e.g. a Docker Compose / Kubernetes service name (`http://anvil:8545`), `host.docker.internal`, or a private-VPC IP.

The flag is **per-invocation, not pinned into the DB**: set it on every subcommand that dials L1 (`setup`, `run`, and `flush-mempool`). Setting it only on `setup` and then omitting it on `run` is refused at boot with `remote RPC must use https` — that is by design (each keyed/read path re-validates the endpoint), not a bug. In a container deployment, put it in the shared environment for all sequencer commands. Example (Docker Compose):

```yaml
environment:
  CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT: "http://anvil:8545"
  CARTESI_SEQUENCER_ALLOW_INSECURE_RPC: "true"
```

Process exit codes follow the orchestrator exit-code contract: `0` clean shutdown, `20` transient refusal (retry with backoff), `30` terminal command refusal (operator required — e.g. setup not complete, setup on a non-empty data directory, identity mismatch, a base machine that disagrees with L1, canonical divergence, a void history, persistent storage/application invariant failure), `40` setup's escape check found the old instance's work past the stop block (do not restart; stop the old instance, discard the data directory, run setup again), and `1` for an unclassified operational failure; `10` is not used. A usage error from argument parsing, such as an unknown flag, exits `2` before any command runs. Diagnosed terminal runtime faults, including worker panics, an undecodable batch landed by the submitter key, and a divergence or void history seen by a running process, immediately call `abort()` (SIGABRT, status 134), without worker drain or database settlement. Supervisors must treat SIGABRT as terminal class. Ordinary shutdown and transient errors still drain gracefully. Startup panics caught by the command harness are projected to `30`; panics after runtime scope creation abort; `101` remains possible before the command harness starts. The constants live in `sequencer/src/commands/error.rs`; supervisor recipes are in [`docs/watchdog/operator-deployment.md`](docs/watchdog/operator-deployment.md).

The [EIP-712 domain](docs/protocol/scheduler-semantics.md#user-op-signatures) is fixed except `chainId` and `verifyingContract`, which come from `CARTESI_SEQUENCER_BLOCKCHAIN_ID` and `CARTESI_SEQUENCER_APP_ADDRESS`.

## API

This section owns the wire contract. [`openapi.yaml`](openapi.yaml) renders it
as OpenAPI 3.1 for client generation and documentation tools; change both
together.

The sequencer serves two listeners. The public one
(`CARTESI_SEQUENCER_HTTP_ADDR`) serves only `POST /tx`, `GET /fee`,
`GET /nonce`, and `GET /domain`. Every other route, health probes included, is
internal and served only on the internal listener
(`CARTESI_SEQUENCER_INTERNAL_HTTP_ADDR`), without authentication; keep it off
the public network.

Errors use one JSON shape, `{ "ok": false, "code": "<CODE>", "message": "<text>" }`,
with these codes: `400` `BAD_REQUEST` or `INVALID_SIGNATURE`, `413`
`PAYLOAD_TOO_LARGE`, `422` `EXECUTION_REJECTED`, `429` `OVERLOADED`, `500`
`INTERNAL_ERROR`, and `503` `UNAVAILABLE`. A `500` message is fixed and carries
no internal detail. Exceptions: history-policy `409` bodies carry the typed
refusal (`{ "code": "ERA_CHANGED", ... }`), the `/ws/subscribe` pre-upgrade
`400` is plain text, snapshot-route `404` and `500` responses have empty bodies,
and the health probes use their own shapes.

JSON `sender` fields in successful `POST /tx` and `GET /nonce` responses and
WebSocket messages, and `verifyingContract` in `GET /domain`, use EIP-55
checksum casing. Address fields in `/history` use lowercase hex. Clients must
compare decoded 20-byte addresses and use one normalized encoding for account or
projection keys across these routes.

### `POST /tx`

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

### `GET /fee`

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

### `GET /nonce?sender=<address>`

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
- It is read from the era's live ops only. After a setup, a sender whose last op lies in the [rebuilt history](docs/protocol/application-history.md#the-rebuilt-history) or below the base count reads 0 until its first live op; sign the nonce the `422` names.
- Successful responses carry `Cache-Control: no-store`. `503` with code `UNAVAILABLE` during shutdown.

### `GET /domain`

The EIP-712 domain `POST /tx` verifies signatures against, keyed as `eth_signTypedData_v4` expects:

```json
{ "name": "CartesiAppSequencer", "version": "1", "chainId": 31337, "verifyingContract": "0x..." }
```

Clients pin their own domain and assert that it matches this one, the way a wallet checks `eth_chainId`. `chainId` is a JSON number, exact in JavaScript below 2^53. That covers every chain browser wallets accept (MetaMask refuses IDs above 4503599627370476); a larger ID fails closed, because a signature over a rounded `chainId` recovers a different sender at `POST /tx`. Do not sign with a served domain unchecked: `chainId` and `verifyingContract` are what keep a signature from replaying on another deployment, and the name and version are the same everywhere.

### `GET /ws/subscribe?era_id=<uuid>&next_input=<u64>`

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

### History metadata (internal only)

Readers that maintain additional transfer/order history follow the feed from
the era's base count and keep their own checkpoints, which they restore to
realign after a recovery
([the client's playbook](docs/recovery/cockroach.md#the-clients-playbook)).

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
`next_input = X`. The [history contract](docs/protocol/application-history.md#identity-and-available-history)
owns this rule.

A client of an older era (its claim refused with `ERA_CHANGED`) holding state at
count `X` adopts the current era when `K <= X <= E`: it subscribes with the
current `history.era_id` and `next_input = X`. Below `K`, the era's base is
newer than its state; above `E`, its state holds rows the old instance served
that never landed by `C`, even when `head` is past `X`. Both need another
starting state. The check compares counts only: a state that is wrong below
its count passes it, and only comparing the feed's rows from `K` with its own
exposes that. The [history contract](docs/protocol/application-history.md#adopting-a-new-era)
defines the rule; the server adds no check of its own.

The Rust SDK exposes `history(expected_era)` and
`adopt_current_era(count)` with typed metadata, adoption and era refusals.
Both use the configured request deadline, including body transfer. The client
owns persistence, checkpoint selection, and subscription.

### Operator snapshot endpoints (internal only)

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
lease or archive is created. See [snapshot lifecycle](docs/snapshots/lifecycle.md).

### Health probes (internal only)

- `GET /livez` returns `200` whenever the handler responds, with an empty body.
- `GET /readyz` returns `200` while the inclusion-lane receiver is open and
  shutdown has not been requested; otherwise `503`. Its body is empty.
- `GET /healthz` uses the same status as `/readyz` and returns JSON:
  `{ "status": "ok", "inclusion_lane": "ok", "landing_lag_blocks": 40, "max_wait_blocks": 1200, "safe_head_age_seconds": 780 }`.
  `status` becomes `"degraded"`
  for either failure condition; `inclusion_lane` becomes `"stopped"` once its
  receiver is closed. The lane closes it soon after shutdown is requested, so it
  can still read `"ok"` early in a shutdown.
- `landing_lag_blocks`, `max_wait_blocks` and `safe_head_age_seconds` are the
  [landing lag](docs/recovery/README.md#what-the-sequencer-does), the
  application's window, and the safe block's age by the wall clock.
  The history is void when `landing_lag_blocks` reaches `max_wait_blocks`.
  `landing_lag_blocks` and `safe_head_age_seconds` are `null` until the first
  reading after start, about two seconds. They do not change the status code.

These probes cover process reachability, the lane channel, and shutdown state,
and report the landing lag and the safe head's age for monitoring to alert on;
they set no threshold of their own. They do not certify submitter balance or
canonical agreement.

## Storage Model

- `batches`: one row per batch, keyed by its nonce, with the safe input that carried its accepted landing (`accepted_input`)
- `frames`: frame boundaries within each batch
- `frames.fee`: committed fee for each frame
- `user_ops`: included user operations
- `application_inputs`: current application sequence keyed by mandatory pre-execution offset; each row references a user op or an external direct input and its owning batch/frame
- `safe_inputs`: every raw InputBox observation, including batch envelopes
- `era`: one immutable row (base count, stop block, rebuilt history end, first queued input, anchor batch nonce); it exists exactly when setup has completed
- `rebuilt_history`: the transactions setup executed from the base through the stop block, each with its count; the feed serves them before the live rows
- `snapshots`: one immutable row per batch-close or baseline artifact (its directory and count); accepted status is read from `batches.accepted_input`, and download leases live in the serving process
- `batch_policy`: singleton fee and batch-sizing knobs; `batch_policy_derived` exposes `recommended_fee` and `batch_size_target` ([formula](sequencer/src/storage/migrations/0001_schema.sql)). A batch closes when its `batch_size_target` byte budget fills or `CARTESI_SEQUENCER_MAX_BATCH_OPEN_SECONDS` elapses, whichever comes first. The [threat model](docs/threat-model/README.md#fee-oracle) owns how the oracle prices and refreshes.

## Project Layout

The sequencer is a library: an application crate composes it with its
`Application` implementation into a binary, as
[`examples/wallet-sequencer/`](examples/wallet-sequencer/) does with the
placeholder wallet in [`examples/app-core/`](examples/app-core/). Native engines
bind through the [C ABI adapter](bindings/c-app-engine/README.md). A C or C++
guest reaches the canonical scheduler through the proposed
[`scheduler.h`](bindings/c-scheduler/README.md), and a port of it is held to the
[conformance vectors](docs/protocol/scheduler-semantics.md#conformance). The
[architecture map](AGENTS.md#architecture-map) covers the workspace and the
sequencer's modules.

The watchdog ships as a multi-arch container image per release tag; the
[operator deployment guide](docs/watchdog/operator-deployment.md) gives the
image names and how to run it.

## Prototype Limits

- The `Application` trait defines dump/load behavior ([format](docs/snapshots/format.md)). Every batch close registers a durable snapshot atomically with the seal. Restart restores the latest valid snapshot and replays application inputs from its count. Acceptance determines the comparison checkpoint and garbage-collection frontier without mutating artifact metadata. [Snapshot lifecycle](docs/snapshots/lifecycle.md) documents leases and crash ordering.
- Schema and migrations are still in prototype mode and may change.

## Development

The shared [development commands](AGENTS.md#shell-and-commands) cover Rust
toolchain selection, Nix/direnv tooling, compilation, tests, formatting, and
linting. Read the [testing guidance](AGENTS.md#testing-guidance) before choosing
validation for a change; some tests require Anvil or libslirp.

## Further Reading

[`AGENTS.md`](AGENTS.md) is the developer guide: architecture, conventions, and
rules. Its [Reading Routes](AGENTS.md#reading-routes) name the document that
owns each contract.

## License

Apache-2.0. See [`LICENSE`](LICENSE). Authors in [`AUTHORS`](AUTHORS).
