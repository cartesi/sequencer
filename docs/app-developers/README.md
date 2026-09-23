# Adopting the sequencer in a Cartesi application

This guide is for developers who already have — or are designing — a Cartesi
rollup application and want its users to transact through the sequencer instead
of sending every transaction to L1. It assumes you know how a Cartesi
application works today (advance/inspect, InputBox, portals, notices and
vouchers). It does **not** assume you know how the sequencer works inside.

Your application can be written in any language that runs on the Cartesi
Machine (riscv64): TypeScript, Go, Python, C++, Rust. The protocol is
language-neutral. The tooling in this repository is not yet: read
[What the repository gives you](#what-the-repository-gives-you) before
estimating the work.

The guide has six parts:

| Document | Answers |
|---|---|
| This page | What changes, what stays, how much work it is |
| [`application-model.md`](application-model.md) | How must my application logic be reshaped? |
| [`client-integration.md`](client-integration.md) | How do my frontend and backend talk to the sequencer? |
| [`integration-paths.md`](integration-paths.md) | How does my TypeScript / Go / C++ / Rust code actually get hosted? |
| [`typescript.md`](typescript.md) | What exactly would I have to write for an all-TypeScript application? |
| [`machine-hosted.md`](machine-hosted.md) | Could the sequencer just run my Cartesi Machine image? (Path C3, in depth) |

## The idea in one minute

Without a sequencer, every user transaction is an L1 transaction. The user pays
L1 gas and waits for L1 before the application reacts.

With the sequencer, a user **signs** a message with their wallet and sends it
over HTTP. The sequencer runs your application logic immediately, answers in
well under a second, and later posts many users' messages to L1 as one batch.
Inside the Cartesi Machine, a component called the **scheduler** unpacks the
batch and hands your application the same operations, in the same order.

```
BEFORE                                   AFTER

user wallet                              user wallet
   │ L1 tx: InputBox.addInput               │ signs EIP-712 message (no gas)
   ▼                                        ▼
L1 InputBox                              sequencer ──► answers in < 1 s
   │ one input per user tx                  │          ("soft confirmation")
   ▼                                        │ posts ONE batch for many users
Cartesi Machine                             ▼
   advance(msg_sender = user, payload)   L1 InputBox
                                            │ input from the sequencer address
                                            ▼
                                         Cartesi Machine
                                            scheduler unpacks the batch
                                              └► your app: execute(sender, data)
```

The answer the sequencer gives is a **soft confirmation**: a prediction of what
the machine will compute once the batch reaches L1. L1 and the machine stay the
source of truth. The sequencer cannot steal funds or invent state; at worst it
can ignore a user or go offline. Deposits never go through it, so it cannot
block them (see [Deposits](#deposits-and-other-l1-inputs)).

## What stays the same

- Your application still runs in the Cartesi Machine and still settles through
  the same rollups contracts. Dispute resolution is unaffected.
- Deposits still arrive through the portals, as L1 inputs.
- Notices and vouchers are still emitted by the machine and validated on L1.
  Withdrawals still execute as vouchers after settlement.
- Your payload encoding is still yours. The sequencer treats the payload as
  opaque bytes.

## What changes

| Area | Before | With the sequencer |
|---|---|---|
| How users transact | L1 transaction to `InputBox.addInput` | EIP-712 signed message, `POST /tx` to the sequencer (`GET /fee` for the fee cap to sign) |
| Who the machine sees as sender | `msg_sender` of the input is the user | `msg_sender` of a batch is the **sequencer**; each user is identified by their recovered signature |
| Replay protection | L1 account nonce, for free | A per-user **nonce kept in your application state** |
| Cost to the user | L1 gas | A **fee charged by your application**, in your application's token |
| Latency | L1 block time and up | Sub-second soft confirmation; L1 finality later |
| Deposits | Executed as soon as the input is processed | Queued, executed at the next batch that covers their block — minutes, not seconds |
| Input your code receives | One advance request per transaction | A user operation (`sender`, `data`, `fee`, a block number) or a direct input (`sender`, `block_number`, `payload`) |
| Notion of time | Block number **and timestamp** on every input | A block number only |
| Where your logic runs | In the machine | In the machine **and** beside the sequencer; both must agree byte for byte |
| Reading state | Inspect / your node's APIs | An indexer fed by the sequencer's ordered transaction feed |

The last row but one is the heart of the migration. To answer in under a
second, the sequencer must *execute your application* — check the nonce, check
the fee, apply the operation — before anything touches L1. So your logic runs
twice: once ahead of time beside the sequencer, and once canonically inside the
machine. If the two ever disagree, the soft confirmations were wrong. Most of
the requirements in this guide exist to keep them identical.

## The three pieces of work

1. **Reshape the application logic** into a deterministic state machine with a
   small, fixed set of entry points: validate a user operation, apply it, apply
   an L1 input, save and restore state. Add nonces and fees to your state.
   → [`application-model.md`](application-model.md)

2. **Change the clients.** The frontend signs typed data instead of sending L1
   transactions. Reads move to an indexer that follows the sequencer's feed.
   → [`client-integration.md`](client-integration.md)

3. **Host the logic in both places.** Inside the machine it runs under the
   scheduler; outside, it plugs into the sequencer as an `Application`. How
   hard this is depends on your language.
   → [`integration-paths.md`](integration-paths.md)

## What the repository gives you

| Piece | Status |
|---|---|
| The sequencer itself (HTTP API, batching, L1 submission, recovery) | Provided, as a Rust library you compose into a binary |
| The scheduler (canonical ordering inside the machine) | Provided in Rust: [`sequencer-core/src/scheduler/`](../../sequencer-core/src/scheduler/mod.rs) |
| The application interface | A Rust trait: [`Application`](../../sequencer-core/src/application/mod.rs) |
| A complete worked example | The wallet: [`examples/app-core/`](../../examples/app-core/) (logic), [`examples/wallet-sequencer/`](../../examples/wallet-sequencer/) (sequencer binary), [`examples/canonical-app/`](../../examples/canonical-app/) (machine image) |
| A client library | Rust only: [`sdk/rust-client/`](../../sdk/rust-client/) |
| A C ABI for the sequencer host | [`bindings/c-app-engine/`](../../bindings/c-app-engine/README.md): a header any language with a C ABI can implement, a generic host binary, a reference wallet engine, and conformance tests |
| Bindings for interpreted languages | None. No TypeScript or Python binding; no adapter that hosts a Cartesi Machine as the sequencer-side application |
| A state watchdog | Provided: [`docs/watchdog/`](../watchdog/README.md). It compares the sequencer's state against the machine's and raises an alarm on divergence |

In practice: a **Rust** application can follow the wallet example end to end. An
application in **Go, C, or C++** implements the C header, links its archive into
the provided host, and still needs a small Rust wrapper inside the machine. A
**TypeScript** (or Python, or any interpreter-hosted) application has no ready
path today and needs either a port of its state machine or new hosting work.
[`integration-paths.md`](integration-paths.md) lays out the options and their
costs honestly.

## Deposits and other L1 inputs

Anything sent to your application's InputBox by an address other than the
sequencer is a **direct input**. Portal deposits are the common case. Direct
inputs bypass the sequencer entirely, which is what makes them uncensorable.

They are not executed the moment they land. The scheduler parks them in a queue
and executes them, in L1 order, when a batch arrives whose frame covers their
block. In normal operation that is a matter of minutes (the design target is
under ten). If the sequencer stalls or misbehaves, the scheduler executes them
anyway once they are `MAX_WAIT_BLOCKS` old (1200 blocks, about four hours).

This ordering rule — "queued L1 inputs up to block N, then these user
operations" — is what lets the sequencer and the machine compute the same
state. Your application does not implement it; the scheduler does. You only
need to know that a deposit becomes spendable a few minutes after L1, and not
instantly.

## Migration checklist

Application logic:

- [ ] State transition logic is separated from I/O (no HTTP rollup loop inside it).
- [ ] Every user has a nonce in application state; an operation with the wrong nonce is rejected without changing anything.
- [ ] A fee is charged per operation, in a token your application holds balances of; an operation the sender cannot pay for is rejected without changing anything.
- [ ] Validation is a pure read. All mutation happens in the apply step.
- [ ] A malformed or failing operation is an **included no-op** (nonce consumed, fee charged), never a crash.
- [ ] Execution is deterministic: no clock, no randomness, no unordered iteration, no floating point.
- [ ] No dependence on input timestamp, `prev_randao`, or input index.
- [ ] State carries two counters (`executed_input_count`, `last_executed_safe_block`) and updates them on every applied input.
- [ ] State serializes to deterministic bytes, identical in the machine and outside it.
- [ ] The machine answers the `state` inspect query with exactly those bytes.
- [ ] A tool turns a trusted machine checkpoint into a sequencer checkpoint (state, counters, next batch nonce), and the rebuild has been rehearsed.

Clients:

- [ ] Frontend signs the `UserOp` typed data and posts to `/tx`.
- [ ] Frontend reads `GET /fee` before signing and uses its `suggested_max_fee`.
- [ ] Frontend gets the user's next nonce from your indexer and tracks it locally.
- [ ] UI distinguishes "soft-confirmed" from "final on L1".
- [ ] Reads come from an indexer that restores `/latest_snapshot`, subscribes with its history claim, and keeps the claim with its checkpoints — not from the sequencer directly.

Hosting and operations:

- [ ] Machine image runs the scheduler in front of your logic.
- [ ] A sequencer binary embeds the same logic.
- [ ] Tests run the same inputs through both and compare state bytes.
- [ ] The watchdog is deployed against the production pair.

## Vocabulary

| Term | Meaning |
|---|---|
| **User operation** (user op) | A signed message from a user: `nonce`, `max_fee`, `data`. `data` is your payload. |
| **Direct input** | An L1 InputBox input from anyone other than the sequencer. Deposits are direct inputs. |
| **Soft confirmation** | The sequencer's immediate answer that an operation was accepted and ordered. A prediction, not finality. |
| **Batch** | Many user ops posted to L1 as one input by the sequencer. |
| **Frame** | A section of a batch. It names a **safe block** and carries the user ops executed after all direct inputs up to that block. |
| **Safe block** | An L1 block number the sequencer commits to having accounted for. It is also the only clock user ops see. |
| **Scheduler** | The code inside the machine that unpacks batches, queues direct inputs, enforces ordering, and calls your application. |
| **Canonical** | What the machine computes from L1. The truth the sequencer tries to predict. |
| **Fee exponent** | Fees travel as a small integer `n` meaning `(129/128)^n` token units. See [`application-model.md`](application-model.md#fees). |
| **Checkpoint / dump** | A durable copy of your application state that the sequencer can restart from. |
| **History claim** | `(era, recovery generation, next input)`: names exactly which version of the sequencer's history a replica holds. Required to subscribe to the feed. |
| **Cockroach recovery** | Rebuilding the sequencer from a trusted machine checkpoint when its own data is lost or untrusted. Your application must supply the checkpoint conversion. |
| **Watchdog** | An independent process that checks the sequencer's state against the machine's. |

## Going deeper

This guide restates the contracts in plain terms. The authoritative versions:

- [`docs/protocol/application-contract.md`](../protocol/application-contract.md) — the exact contract your logic must satisfy.
- [`docs/protocol/scheduler-semantics.md`](../protocol/scheduler-semantics.md) — the exact ordering algorithm.
- [`README.md`](../../README.md) — the HTTP/WebSocket API contract, configuration, and trust model.
- [`docs/protocol/application-history.md`](../protocol/application-history.md) — history coordinates and the replica bootstrap/resume procedure.
- [`docs/protocol/c-application-binding.md`](../protocol/c-application-binding.md) — the C ABI for native engines.
- [`docs/snapshots/lifecycle.md`](../snapshots/lifecycle.md) — checkpoint artifacts, acceptance, and retention; [`format.md`](../snapshots/format.md) for the wallet's bytes.
- [`docs/recovery/cockroach.md`](../recovery/cockroach.md) — the rebuild procedure and what your application must provide for it.
- [`docs/threat-model/README.md`](../threat-model/README.md) — what the sequencer is and is not trusted for.
