# Client integration

How frontends, backends, and indexers talk to a sequencer-backed application.
The endpoint contract — exact shapes, caps, close codes — is owned by the
project [`README.md`](../../README.md#api); this page explains how to use it
from an application's point of view.

## What changes for a frontend

| Task | Before | With the sequencer |
|---|---|---|
| Send a transaction | `InputBox.addInput(app, payload)` as an L1 transaction | `GET /fee`, sign typed data, `POST /tx` |
| Wallet prompt | "Confirm transaction" with gas | "Sign message" — no gas, no ETH needed |
| Know it worked | Wait for L1, then for the node | HTTP `200` in under a second |
| Deposit | Portal transaction on L1 | **Unchanged** |
| Withdraw | Operation → voucher → execute after settlement | Same, but the operation is a signed message |
| Read balances, history | Inspect, GraphQL, your node | Your indexer, fed by the sequencer's feed |

## Submitting a user operation

A user operation is three fields:

| Field | Type | Meaning |
|---|---|---|
| `nonce` | `uint32` | The sender's next nonce in your application (starts at 0) |
| `max_fee` | `uint16` | The highest fee exponent the user accepts |
| `data` | `bytes` | Your application payload |

The user signs it as EIP-712 typed data. The domain is fixed by the protocol
except for the two values that identify your deployment:

| Domain field | Value |
|---|---|
| `name` | `CartesiAppSequencer` |
| `version` | `1` |
| `chainId` | The L1 chain id |
| `verifyingContract` | Your application contract address |

Binding the signature to the chain and application address is what stops a
signed operation from being replayed against another deployment. All four
fields must be present, and the type definition must match the sequencer's
exactly — field names, order, and integer widths. A mismatch does not produce a
helpful error: the signature simply recovers to some other address and the
request fails with `INVALID_SIGNATURE`.

### TypeScript (viem)

```ts
import { createWalletClient, custom, type Hex } from "viem";

const types = {
  UserOp: [
    { name: "nonce", type: "uint32" },
    { name: "max_fee", type: "uint16" },
    { name: "data", type: "bytes" },
  ],
} as const;

export async function submitUserOp(opts: {
  sequencerUrl: string;      // e.g. https://sequencer.example.com
  chainId: number;
  appAddress: Hex;
  nonce: number;
  data: Hex;                 // your encoded payload
}) {
  const wallet = createWalletClient({ transport: custom(window.ethereum) });
  const [sender] = await wallet.requestAddresses();

  // Ask the sequencer what to sign as the fee cap (see "Choosing max_fee").
  const quote = await fetch(`${opts.sequencerUrl}/fee`).then((r) => r.json());
  const message = {
    nonce: opts.nonce,
    max_fee: quote.suggested_max_fee as number,
    data: opts.data,
  };

  const signature = await wallet.signTypedData({
    account: sender,
    domain: {
      name: "CartesiAppSequencer",
      version: "1",
      chainId: opts.chainId,
      verifyingContract: opts.appAddress,
    },
    types,
    primaryType: "UserOp",
    message,
  });

  const res = await fetch(`${opts.sequencerUrl}/tx`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ message, signature, sender }),
  });

  const body = await res.json();
  if (!res.ok) throw new Error(`${res.status} ${body.code}: ${body.message}`);
  return body as { ok: true; sender: Hex; nonce: number };
}
```

Request notes:

- `signature` is the 65-byte `r ‖ s ‖ v` hex string wallets return. `v` may be
  `27/28` or `0/1`.
- `sender` is required and must equal the address the signature recovers to.
- `nonce` and `max_fee` are JSON numbers; `data` is `0x`-prefixed hex.
- `POST /tx` and `GET /fee` accept browser requests from any origin.

### Go

```go
typed := apitypes.TypedData{
    Types: apitypes.Types{
        "EIP712Domain": {
            {Name: "name", Type: "string"},
            {Name: "version", Type: "string"},
            {Name: "chainId", Type: "uint256"},
            {Name: "verifyingContract", Type: "address"},
        },
        "UserOp": {
            {Name: "nonce", Type: "uint32"},
            {Name: "max_fee", Type: "uint16"},
            {Name: "data", Type: "bytes"},
        },
    },
    PrimaryType: "UserOp",
    Domain: apitypes.TypedDataDomain{
        Name:              "CartesiAppSequencer",
        Version:           "1",
        ChainId:           math.NewHexOrDecimal256(chainID),
        VerifyingContract: appAddress.Hex(),
    },
    Message: apitypes.TypedDataMessage{
        "nonce":   fmt.Sprint(nonce),
        "max_fee": fmt.Sprint(maxFee), // suggested_max_fee from GET /fee
        "data":    data, // []byte
    },
}

hash, _, err := apitypes.TypedDataAndHash(typed)
// ...
sig, err := crypto.Sign(hash, privateKey) // 65 bytes, v in {0,1} — accepted as is
```

Then `POST` the same JSON body as above. A Rust client covering `get_fee`,
`submit_tx`, and `subscribe` is available in
[`sdk/rust-client/`](../../sdk/rust-client/); the test harness signs with it in
[`tests/harness/src/wallet.rs`](../../tests/harness/src/wallet.rs).

## Responses

Success is `200` with `{ "ok": true, "sender": "0x…", "nonce": <n> }`. By the
time you receive it, the operation has been validated, executed against the
sequencer's copy of your application, given a position in the order, and
written to disk.

Errors share one shape: `{ "ok": false, "code": "<CODE>", "message": "<text>" }`.

| HTTP | `code` | Meaning | What the client should do |
|---|---|---|---|
| 400 | `BAD_REQUEST` | Malformed JSON, wrong hex lengths, payload over your application's size limit | Fix the request; do not retry |
| 400 | `INVALID_SIGNATURE` | Signature invalid, or recovers to an address other than `sender` | Check domain values and type definition |
| 413 | `PAYLOAD_TOO_LARGE` | HTTP body too large | Fix the request |
| 422 | `EXECUTION_REJECTED` | Your application's validation rejected it: wrong nonce, `max_fee` below the current fee, or not enough balance for the fee | Read `message`; refresh the nonce or fetch `/fee` again; re-sign |
| 429 | `OVERLOADED` | The sequencer's queue is full | Back off and retry the same signed request |
| 503 | `UNAVAILABLE` | The sequencer is shutting down or restarting | Retry with backoff |
| 500 | `INTERNAL_ERROR` | Sequencer fault | Retry with backoff; alert if persistent |

A rejected operation changed nothing: no nonce was consumed and no fee was
charged. It is safe to correct and resubmit.

Remember from [`application-model.md`](application-model.md#the-three-outcomes)
that `200` means *included*, not *succeeded*. A transfer the user cannot afford
returns `200`, consumes the nonce, charges the fee, and moves no funds. Catch
what you can in the UI before asking for a signature, and show the real outcome
from your indexer afterwards.

## Nonces

The sequencer has no "get nonce" endpoint — the nonce lives in your
application's state, and the sequencer does not interpret that state. Provide
it from your indexer (which sees every included operation and its nonce on the
feed), and have the frontend:

1. fetch the next nonce when the session starts;
2. increment locally after each `200`;
3. on `422` with a nonce message (`bad nonce: expected 7, got 5`), resynchronize.

Operations from one sender must be submitted in nonce order. Wait for each
response before sending the next; an operation that arrives ahead of its
predecessor is rejected.

## Choosing `max_fee`

`max_fee` is a cap, not a price. The sequencer sets the fee per frame from L1
gas prices; an operation pays the **frame's** fee, never more than the
`max_fee` the user signed, and is rejected if the frame's fee is higher. The
user signs before knowing which frame the operation lands in, so the cap needs
some headroom.

`GET /fee` gives the numbers:

```json
{ "fee": 1356, "recommended_fee": 1356, "suggested_max_fee": 1409 }
```

| Field | Meaning | Use it for |
|---|---|---|
| `fee` | What an operation pays if it is included right now. Fixed while the current frame is open | Showing the user the expected cost |
| `recommended_fee` | What the next frame will charge when it opens (frames rotate every few L1 blocks) | Showing a trend; your own policy |
| `suggested_max_fee` | The higher of the two, plus headroom for a 1.5× rise | Copying into the signed `max_fee` |

All three are exponents in the same encoding as `max_fee`: the amount is
`floor((129/128)^n)` of your fee token's smallest unit, so +1 is about +0.78 %
and the suggested headroom is +53. For most frontends the policy is simply:
fetch `/fee` immediately before signing, sign `suggested_max_fee`, display
`fee`. If signing takes long enough for fees to move, a `422`
(`max fee 1409 below base fee 1420`) tells you to fetch again and re-sign.

`/fee` returns `503 UNAVAILABLE` while the sequencer is restarting; retry with
backoff, as for `/tx`.

To display an exponent as a token amount in the browser:

```ts
// (129/128)^(2^i) in fixed point with 64 fractional bits — the same table and
// the same truncating multiply as sequencer-core/src/fee.rs.
const TABLE: bigint[] = [129n << 57n];
for (let i = 1; i < 15; i++) TABLE.push((TABLE[i - 1] * TABLE[i - 1]) >> 64n);

export function feeToLinear(exponent: number): bigint {
  let r = 1n << 64n;
  for (let i = 0; i < 15; i++) if (exponent & (1 << i)) r = (r * TABLE[i]) >> 64n;
  return r >> 64n;                       // smallest units of the fee token
}

feeToLinear(1356); // 38276n  → 0.038276 of a 6-decimal token
```

This helper reproduces the values pinned in the reference tests. That is
enough for display. If you reuse it inside a TypeScript *engine*, where a
one-unit difference forks state, first test it against the Rust
`fee_to_linear` over the full exponent range
([`application-model.md`](application-model.md#fees)).

## What a soft confirmation promises

A `200` means: *the sequencer has ordered this operation, and if its batch
reaches L1 in time, the machine will execute it at exactly this position.*
Under normal operation that is what happens. It is still a prediction, and a
frontend for anything valuable should show progress honestly:

| Stage | Meaning | Typical delay |
|---|---|---|
| Soft-confirmed | Ordered and executed by the sequencer | < 1 s |
| Posted | The batch is on L1 | Minutes |
| Safe / finalized | The batch is in L1 blocks that will not reorganize | More minutes |
| Settled | Rollup outputs validated; vouchers executable | Your rollup's settlement period |

If the sequencer suffers a long outage, batches it could not post in time are
discarded by the scheduler, and the operations in them **are rolled back** —
as if they had never been submitted. The sequencer detects this ahead of time,
stops accepting operations, and recovers on restart. The feed does not yet
carry an explicit rollback signal, so an indexer must treat only operations in
L1-finalized batches as irreversible, and should be able to rebuild from a
snapshot. Design flows where a rolled-back soft confirmation would be costly —
releasing goods, crediting an external system — to wait for L1.

Withdrawals are safe by construction: vouchers exist only in the machine and
execute only after settlement.

## Deposits

Deposits are unchanged for the client: an L1 transaction to the relevant
portal. What changes is timing. The deposit is credited when the sequencer's
next frame covers the deposit's block, once that block is *safe* on L1 —
minutes, under normal operation. If the sequencer is down or censoring, the
machine credits it anyway after about four hours (1200 blocks). Show deposits
as pending until your indexer sees the corresponding `direct_input` on the
feed.

Never send deposits through `POST /tx`. Assets enter only through L1.

## Reading state

The sequencer orders and executes operations; it is not a query server. It has
no balance endpoint, no inspect, and returns no application outputs. Reads are
served by an **indexer** you run, which follows the sequencer's feed:

```
                 POST /tx
  frontend ─────────────────────────► sequencer
     │                                   │  GET /ws/subscribe   (internal network)
     │  your own API                     ▼
     └──────────────────────────────► indexer ──► database
                                         ▲
                                         └─ runs the same application logic
```

`GET /ws/subscribe?from_offset=<n>` streams every sequenced transaction in
execution order, as JSON:

```json
{ "kind": "user_op", "offset": 10, "sender": "0x…", "nonce": 7, "fee": 131,
  "data": "0x…", "safe_block": 123, "batch_nonce": 4 }

{ "kind": "direct_input", "offset": 11, "sender": "0x…", "block_number": 123,
  "block_timestamp": 1700000000, "transaction_hash": "0x…", "payload": "0x…",
  "input_index": 42, "batch_nonce": 4 }
```

The feed carries **inputs, not results**. To know balances, order books, or
which operations were no-ops, the indexer applies each message to its own
instance of your application logic — the same engine described in
[`application-model.md`](application-model.md), used as a library. This is a
real advantage of writing the engine as an I/O-free state machine: a TypeScript
engine can run unchanged in a Node indexer.

To start an indexer without replaying from the beginning, fetch
`GET /latest_snapshot` (your canonical state file), read the
`X-L2-Tx-Index` response header, and subscribe from that offset.

Two operational rules:

- **The feed and snapshot endpoints are operator-internal.** They have no
  authentication and a small subscriber limit. Do not expose them to browsers
  or the internet; put your indexer on the private network and expose *its*
  API.
- The feed replays a bounded window. An indexer that falls too far behind is
  disconnected with the offset to resume from, and should rebuild from a
  snapshot.

`block_timestamp` and `transaction_hash` appear on the feed for display
purposes. Your engine does not receive them and must not depend on them.

## Backends and bots

A backend that transacts on its own behalf (a market maker, a keeper) is just
another client: hold a key, read `/fee`, sign typed data, `POST /tx`, track
the nonce.
Because responses arrive in milliseconds, submit sequentially per account and
treat `429` as backpressure.
