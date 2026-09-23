# Remote application engine protocol — design sketch

**Status: proposal, not implemented.** Nothing on `main` speaks this protocol.
The sequencer reaches its application through the Rust `Application` trait and
the in-process [C ABI](../protocol/c-application-binding.md). This sketch records what a language-neutral, out-of-process binding
would have to specify, so that it can be judged against the C ABI and against
the [application contract](../protocol/application-contract.md) it would
carry. The app-developer guide points here from
[`integration-paths.md`](../app-developers/integration-paths.md) and
[`typescript.md`](../app-developers/typescript.md).

## 1. Motivation and the analogy

The sequencer separates *ordering* (its own job: ingress, frames, batches, L1,
recovery) from *execution* (the application's job: validate, apply, checkpoint).
Ethereum made the same cut between consensus clients and execution clients and
bridged it with the Engine API — a small JSON-RPC surface
(`engine_newPayload`, `engine_forkchoiceUpdated`, `engine_getPayload`,
`engine_exchangeCapabilities`) that let any execution client in any language
plug into any consensus client. The analogy is exact in role: the sequencer is
the ordering side, the application engine is the execution side, and the
boundary between them is already a narrow, fully specified contract. The only
thing missing is a transport-neutral binding of it.

The differences matter for the design and are listed in §8: the sequencer
builds batches itself (no `getPayload`), it has no fork choice (rollback is
"reload a checkpoint"), and it needs a read-only validation call the Engine
API has no equivalent for.

## 2. Scope

The protocol covers **the sequencer-host half only**. Inside the Cartesi
Machine the application still runs under a scheduler — the reference Rust one
linked through a shim, or its own port — exactly as today. What changes is
that the sequencer-host engine becomes a separate process, in the
application's native runtime, speaking JSON-RPC over a local socket instead
of being linked into the sequencer binary.

## 3. Shape

- **JSON-RPC 2.0** over a Unix domain socket or loopback TCP. Not exposed
  beyond the host: the engine trusts the sequencer completely, and the
  sequencer trusts the engine as it trusts any application code
  (self-trust, [threat model](../threat-model/README.md)).
- **One method per `Application` method**, plus instance lifecycle and a
  capabilities handshake. Nothing application-specific crosses.
- **Instances are explicit.** `from_dump` is a constructor in the trait; here
  the server is long-lived and `app_open` returns an instance id that every
  execution call names. The sequencer holds one live instance in production
  and opens others transiently (startup catch-up, recovery fold, tests).
- **Encoding follows Ethereum JSON-RPC conventions**: `DATA` as `0x`-hex
  bytes, addresses as `0x`-hex 20 bytes, `QUANTITY` (u64, u32, u16, U256) as
  `0x`-hex without leading zeros. This keeps u64 exact in every language and
  reuses conventions every wallet library already implements.

## 4. Methods

| Method | Params | Result |
|---|---|---|
| `app_exchangeCapabilities` | `{ protocol_versions: [..] }` | `{ protocol_version, max_method_payload_bytes, state_file_name, engine: {name, version} }` |
| `app_open` | `{ dump_prefix }` | `{ instance, progress }` |
| `app_close` | `{ instance }` | `{}` |
| `app_validateUserOp` | `{ instance, sender, nonce, max_fee, data, current_fee }` | `{ outcome: "accept" }` or `{ outcome: "reject", reason, ...values }` |
| `app_applyUserOp` | `{ instance, sender, fee, data, safe_block }` | `{ outputs: [..], progress }` |
| `app_applyDirectInput` | `{ instance, sender, block_number, payload }` | `{ outputs: [..], progress }` |
| `app_progress` | `{ instance }` | `{ progress }` |
| `app_createDump` | `{ instance, dump_prefix }` | `{}` |

Where:

- `progress` is `{ executed_input_count, last_executed_safe_block }`.
- `reason` is one of `invalid_nonce` (`expected`, `got`) or
  `insufficient_fee_balance` (`required`, `available`). `invalid_max_fee` is
  never produced by an engine — the protocol guard belongs to the sequencer —
  but is reserved so the vocabulary matches `InvalidReason`.
- An output is `{ kind: "notice", payload }` or
  `{ kind: "voucher", destination, value, payload }`, in emission order.
- `state_file_name` is the path, relative to a dump prefix, of the canonical
  state file (or `""` when the prefix itself is that file). It replaces the
  trait's pure `state_file_in_dump` with a constant the sequencer learns once.
- There is no `app_deleteDump`: the sequencer removes checkpoints with
  ordinary recursive deletion, as the C ABI already decided. Engines must keep
  every checkpoint artifact at or below the prefix.

## 5. Semantics the schema does not carry

These are the contract's obligations restated for a socket. A binding that
gets the schema right and any of these wrong is not compliant.

1. **One call in flight per instance, strictly ordered.** The sequencer never
   pipelines. The engine may reject a second concurrent call as a protocol
   error.
2. **Rejection is a result; failure is an error.** A validation rejection is
   a normal `result` with `outcome: "reject"`. A JSON-RPC `error` from any
   execution method means the engine failed. Two codes are reserved:
   `INTERNAL` (engine invariant failure, terminal) and `IO` (operational,
   retryable after restart). `app_open` adds `NOT_FOUND` and `INVALID_DUMP`,
   which startup uses to tell a missing checkpoint from a corrupt one. Error
   messages are diagnostics and never determine classification.
3. **Every failure discards the instance.** After an `error`, a transport
   failure, or a **timeout**, the sequencer treats the instance's state as
   unknown: it closes it (or abandons the connection) and, if it continues at
   all, opens a fresh instance from the last checkpoint and replays from its
   own records. The sequencer never retries an execution call on the same
   instance — an `apply` whose reply was lost may or may not have happened.
   Out-of-process, "discard the instance" becomes literal: kill and restart.
4. **Progress rides on every apply reply.** The sequencer verifies the
   transition (`count + 1`, `clock = max(clock, block)`) from the reply, so
   the common path costs one round trip per call; `app_progress` exists for
   open and for tests.
5. **`app_createDump` returns only after the checkpoint is durable** — files
   and directory entries fsynced, including the parent of the prefix — and
   fails if the prefix exists. Later execution must not modify it; instances
   opened from it must stay usable after it is deleted.
6. **The filesystem is shared.** Dump prefixes are paths meaningful to both
   processes; the sequencer registers them in SQLite, serves the state file
   over its own HTTP routes, and deletes them. "Remote" therefore means
   *another process on the same host or volume*, as with an execution client
   and consensus client. Moving checkpoint bytes through the protocol is
   possible but out of scope: checkpoints can be large and the sequencer's
   serving and GC paths assume files.
7. **Validation is read-only** — self-trusted, as it is for a native engine.
8. **Timeouts are policy the sequencer owns**, per method, and a timeout is a
   failure (rule 3), because slow and hung are indistinguishable and the
   500 ms acknowledgement budget makes "slow" already a fault.
9. **Capabilities are exchanged once** at connect. The engine reports its
   payload bound; the sequencer pins it at `setup` beside the deployment
   identity and refuses at `run` if it changed, since batch sizing and
   ingress admission depend on it.

## 6. Lifecycle

- **`setup`** connects, exchanges capabilities, and opens the genesis dump the
  application supplies (as the C bridge's `--state-file` does); configuration
  never crosses the protocol. The sequencer creates its first checkpoint from
  that instance.
- **`run`** opens the latest finalized checkpoint, replays persisted inputs
  through `app_apply*`, then serves. Batch close → `app_createDump`.
- **Recovery fold** (`fold_replay`) opens the checkpoint it is given and drives
  the reference scheduler over the remote instance, unchanged.
- **Supervision.** Either process may be restarted independently. The
  sequencer's exit-code contract already distinguishes "restart me" from
  "page an operator"; a lost engine maps to a transient refusal on the next
  boot until the engine is back. Running the engine as a child process of the
  sequencer is a simplification an operator may choose, not a requirement.

## 7. Cost

A local socket round trip is tens of microseconds; encoding a user operation
is comparable. Two calls per operation add well under a millisecond against a
500 ms budget, and a ceiling in the thousands of operations per second that
the lane's own SQLite commits already sit near. Direct-input backlog
reconciliation pays the same per-input cost. Checkpoint cost is the engine's,
as today. The engine also runs in its own runtime, so a Node or Go engine
executes at native speed — the exact opposite of the
[machine-hosted](../app-developers/machine-hosted.md) trade.

## 8. Compared with the alternatives

| | Rust trait | C ABI | JSON-RPC (this sketch) |
|---|---|---|---|
| Languages | Rust | Anything with a C ABI | Anything with a socket |
| Process | Same | Same | Separate |
| Per-call overhead | None | Function call | Socket + JSON |
| "Discard the instance" | Drop | Destroy handle | Kill/close + reopen |
| Engine crash | Takes the sequencer down | Takes the sequencer down | Isolated; sequencer decides |
| Native runtime (GC, event loop) | n/a | Embedded in the sequencer process | Engine's own process |
| Checkpoint files | Shared | Shared | Shared filesystem required |

Against the Engine API specifically: there is no `getPayload` (the sequencer
assembles batches from what it validated, and any future ordering policy
would be a new, optional method, not a redesign); there is no
`forkchoiceUpdated` (rollback after recovery is "open an older checkpoint and
replay", so the engine never needs to hold more than one state); and
`app_validateUserOp` is a read-only pre-check the Engine API delegates to the
transaction pool. Those simplifications are what keep the surface to eight
methods.

## 9. What the repository would need

1. A **`remote-app-engine`** crate: `RemoteApp` implementing `Application`
   over a JSON-RPC client, with the timeout and discard policy of §5, and a
   host binary equivalent to the C bridge's `c-app-sequencer`.
2. **No trait change for the bound or the state file.**
   `max_method_payload_bytes()` and `state_file_in_dump()` are already
   instance-independent; the remote adapter answers them from the
   capabilities exchange it performs at process start, before the sequencer
   constructs anything, exactly as the C adapter answers from the linked
   archive.
3. A **conformance suite** that any engine can run against: the C bridge's
   mixed-input/rejection/no-op/dump-round-trip/error-classification tests,
   driven over the socket, plus a reference server for the wallet (Rust) and
   ideally one in TypeScript to prove the point.
4. The **protocol document** proper, under `docs/protocol/`, once the shape
   is agreed: methods, encodings, error codes, and §5 verbatim.

## 10. Open questions

- Unix socket only, or also loopback TCP with token auth for container
  deployments where the two run in different containers on one volume?
- Should `app_createDump` take a deadline hint so an engine with expensive
  checkpoints can refuse early rather than stall the lane?
- Is a binary framing (CBOR, MessagePack) worth the loss of transparency for
  large direct-input payloads, or is hex JSON adequate at the sizes ingress
  admits?
- Does any consumer want checkpoint bytes over the protocol (no shared
  filesystem), and what would the serving and GC paths look like if so?
