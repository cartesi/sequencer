# Threat Model

The security posture this codebase defends against. Defines what is in scope for security review, what is out of scope, and the trust level assigned to each actor and interface.

See [`../recovery/README.md`](../recovery/README.md) for the recovery subsystem, which operationalizes parts of this threat model (adversarial mempool, fail-stop L1 provider).

## Assets

What we are protecting:

- **Rollup state integrity.** The canonical on-chain state must reflect a deterministic replay of user operations and direct inputs. Any divergence between the sequencer's off-chain view and the scheduler's on-chain execution is a state-integrity failure.
- **Soft-confirmation honesty.** Every soft confirmation issued by the sequencer must land on L1 as promised, or be explicitly revoked via recovery.
- **User funds.** No user operation, replay, or protocol break can cause users to lose funds.
- **Batch-submitter key.** Held in operator infra; not hijackable by network attackers.

## Actors and trust

| Actor | Trust | Capabilities |
|-------|-------|--------------|
| InputBox contract | Trusted | Authenticates `msg_sender` on `addInput`; must be rollups-contracts v3+ ([InputBox version](#inputbox-version)). |
| Our Ethereum node | Trusted, fail-stop | Inside our infra; may become unreachable but never lies. |
| RPC endpoint (`CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`) | Trusted, fail-stop — **must be one consistent node** | One endpoint serves every L1 client, with no fallback tier ([RPC endpoint](#rpc-endpoint)). |
| Operator env / CLI flags | Trusted, **mistakes foot-gun-guarded** | Setup configuration, including the reviewed fee-oracle pool, is authoritative ([Operator mistakes](#operator-mistakes)). |
| Uniswap V3 pool (fee oracle) | Semi-trusted L1 state | Prices L1 gas in the fee token; a stale or skewed price is an accepted economic risk ([Fee oracle](#fee-oracle)). |
| Batch-submitter private key | Private | Held in operator infra, out of the network's reach. |
| Sequencer's own code | Trusted during normal operation | Tests and review prevent bugs; runtime invariant checks fail loud ([Self-trust](#self-trust)). |
| **L1 mempool and block builders** | **Fully adversarial** | May reorder, delay, drop, or selectively include submitted transactions; with private mempools, "dropped" is indistinguishable from "delayed indefinitely". |
| HTTP clients at `POST /tx`, `GET /fee`, `GET /nonce`, and `GET /domain` | Untrusted | Arbitrary public callers that may submit malformed, malicious, or replay payloads; the `GET` routes are [public reads](#public-reads). |
| WebSocket subscribers at `/ws/subscribe` | Internal, but untrusted for data-exposure | Intended for internal indexers; treat as public for what is exposed. |
| Direct-input senders on L1 | Untrusted | Arbitrary L1 accounts calling InputBox with any calldata. |

### InputBox version

The reader's scan starts at the application's deployment block. That is sound
only because a v3 InputBox reverts `addInput` for an application that does not
exist yet; bootstrap checks `version()` and refuses an older box. Setup pins
the block only once it is at or below a safe head sampled before the read, so
no reorg can move the floor. Do not model InputBox forgery.

### RPC endpoint

The code supports exactly one endpoint, shared by the reader, submitter,
poster, flusher, and fee oracle. Behind a load-balanced fleet, lagging replicas
can silently truncate `get_logs` ranges and desynchronize the flush and re-sync
views; recovery refuses if the re-sync lags the flush view.

The reader fails loud on an incomplete `InputAdded` set
([I6](../invariants.md#i6-the-frame-clock-accounts-for-a-complete-l1-interval)):
index contiguity proves the right prefix, and the InputBox count pinned at the
scanned safe block proves it complete. When that count equals the stored count,
the reader advances without a `get_logs` crawl; a fail-stop node's pinned-block
count cannot understate without lying.

Each ingested input's `EvmAdvance` chain id and application must match the
pinned identity; the endpoint's chain id is checked once per reader process.
Two residuals are accepted: a mid-life chain swap behind the same URL that
advances the head only through the count-equality skip, and a node that lies
consistently about both its logs and its input count, outside the fail-stop
model. A semi-trusted fallback tier (Infura/Alchemy) is future work;
[L1 chain semantics](#l1-chain-semantics) says what to pin before adopting one.

### Operator mistakes

The operator is trusted, not infallible. The complete fee-oracle source is
pinned in deployment identity, and `run` cannot replace it. The supported
mistake class is accidental concurrent or stale use of one data directory: two
processes on one directory (a kernel process lock) or a mistyped `--data-dir`
(open refuses a path with no database). Deliberate subversion,
copied-directory coordination, and distributed fencing are out of scope; the
lock is a local foot-gun guard
([ADR mechanism 1](../plans/2026-08-authority-boundary-adr.md#1-runtimescope-structured-process-ownership)).
Judge mechanisms for this class against this boundary.

### Fee oracle

A TWAP (30 minutes by default, pinned at setup) and the tenfold margin in
`batch_policy.log_slack` ([formula](../../sequencer/src/storage/migrations/0001_schema.sql))
mitigate spot manipulation of a deep pool. The margin does not bound arbitrary
market movement. Residual risks are TWAP lag during real moves and a thin or
wrong pool; multi-hop pricing is out of scope.

Setup writes the first quote under the same hard L1 requirement as the rest of
setup. `run` gates neither recovery nor admission on another quote: it starts
from the persisted `log_gas_price`, builds the source from the pinned identity
without RPC, and refreshes in the background, retrying transient failures
indefinitely and keeping the last price. `log_gas_price_updated_at_ms` is
telemetry, not an expiry gate.

A shared-endpoint outage or stale view is already caught by safe-head progress.
A pool-specific failure with a healthy reader is an accepted, unbounded economic
risk: a stale-low price subsidizes DA and weakens the spam barrier, and a
stale-high price rejects users. Neither changes canonical execution, because
the frame's persisted fee is immutable and enforced by both sides. Setup-time
source misconfiguration (`WrongTokenPair`, `MissingPoolCode`, chain-id
mismatch), a TWAP tick outside the Uniswap range, and persistent storage faults
are terminal; a price above the representable range saturates at `MAX_EXPONENT`.

### Public reads

The `GET` routes are intentional public reads: the open-frame fee, the signing
domain, and any sender's soft-confirmed nonce. The nonce counts a sender's
included ops before their batches reach L1 (up to `max_batch_open`, 2 h by
default); otherwise only the egress feed carries it. It reveals activity
timing, not op contents, and moves only after the op is sequenced, so a watcher
cannot front-run the op it counts.

### Self-trust

The sequencer trusts its own code in a specific sense: **impossible states are never *handled*.** There are no graceful fallback paths, no re-validation of a neighbor module's answer, no code that keeps running past a violated internal contract. Normal preemptive recovery addresses liveness failures such as infrastructure outages, network partitions, and gateway failure.

If a bug corrupts local state or causes the sequencer to emit malformed batches,
frames, or user operations, the operator must diagnose and fix it. Then
[cockroach recovery](../recovery/cockroach.md) can rebuild from a trusted canonical
application checkpoint and L1. The checkpoint must be trusted independently of
the faulty local state. Historical inputs, including malformed batches, receive
the canonical scheduler's normal treatment during replay.

This is **not** a prohibition on checking. Internal invariants are enforced loudly wherever a check is near-free — the type system, SQL constraints and triggers, boundary assertions — because failing loud preserves safety, while a silently-tolerated bug that externalizes (a signed batch, an ack, a feed event) is state divergence: as severe as theft and undefendable at runtime. Loud failure is not automatically self-healing: transient faults may clear on restart, but persistent invalid state is terminal and may require inspection or cockroach recovery. The rule, in short: **assert real invariants, fail loud, never absorb silently, never handle gracefully.** The decision test and the register of cross-module invariants live in [`docs/invariants.md`](../invariants.md).

Inputs from untrusted actors are validated rigorously, as ever.

The supported host process is dedicated to the sequencer. A diagnosed
terminal runtime fault logs its cause and aborts the process, including
when another worker is already draining. Orderly completion of terminal
requests, snapshots, or in-memory application work is not required;
acknowledged user operations are already durably committed. The configured
logging subscriber is assumed to return promptly. There is no hard
termination deadline if logging itself blocks. Shared-process hosting or
blocking production diagnostics would require revisiting that assumption
([authority-boundary ADR](../plans/2026-08-authority-boundary-adr.md)).

## In-scope failure modes

- L1 provider outages at the single configured endpoint, minutes to hours
- Process crashes at arbitrary points, including mid-transaction
- **Restart after a terminal exit (accepted residual window).** There
  is no boot gate on a prior terminal verdict: a deliberate restart after
  exit 30 or SIGABRT boots through fact-derived recovery. Every fault whose evidence
  the boot path reads re-refuses before the first soft confirmation —
  canonical divergence (persisted fact), misconfiguration (re-checked every
  boot), boot-path storage corruption, incomplete setup — and the
  batch/frame spine is re-inspected by the runtime danger detector within
  seconds of launch. The accepted residual is narrow: corrupt payload bytes
  in rows at/below the lane's resume checkpoint re-trip only when the WS
  feed pages them or the submitter re-encodes a pending batch. The feed can
  read the whole available history from era base `K` in bounded pages, with no
  total catch-up limit; this detection path requires a subscriber to read the
  affected rows. A fault with no durable evidence (a panic
  whose trigger does not recur) does not re-trip at all. The window is
  entered by restarting after a terminal exit (including supervisors that
  restart regardless of exit status), and
  it is bounded by backstops that never depended on a boot gate:
  rollbackable soft confirmations, the watchdog's digest comparison, and the I15
  divergence freeze.
- **Adversarial mempool:** reorder, delay, drop, selective inclusion by builders
- **Zombie transactions:** a submitted batch may sit in a private mempool indefinitely and land long after we believed it was gone. Two load-bearing defenses: the recovery flusher consumes every wallet-nonce slot this deployment ever used (anchored by the persisted watermark, I14) so zombies cannot claim them; and the content-identity check (I9/I15) compares every *simulated-accepted* landing strictly after baseline block `C` against the valid closed batch we sealed at that nonce. The complete prefix through `C` is opaque and is never reinterpreted using the recovered nonce. A foreign or byte-different landing records divergence when it becomes safe and is ingested, freezes the accepted frontier, and requires cockroach recovery. This is trust-boundary validation of external input (the mempool replaying our own stale transactions at times we don't control), not defense-in-depth against self-bugs or a general canonical-state oracle. Cockroach recovery does not read the previous database's watermark, so that flush relies on the provider's pool view; the content-identity check is what keeps the residual zombie detected-and-frozen rather than silent (see [the flush boundary](../recovery/cockroach.md#flush-and-stopping-block)).
- L1 reorgs up to safe depth
- Malicious `POST /tx` callers: malformed signatures, spoofed sender, replay across chains or apps, nonce manipulation
- Malicious direct-input senders: arbitrary payload, any intent; sender authenticity is guaranteed by InputBox
- Scheduler/sequencer protocol divergence of any kind (ordering, nonce rules, signature validity, fee semantics) is an in-scope correctness consequence. The content-identity check detects accepted-batch identity failures only; there is no complete runtime detector for the broader class. Shared semantics, review, and tests are preventative, and cockroach recovery rebuilds state after another signal or operator investigation diagnoses divergence and its cause has been corrected.

## Out of scope

- **DoS, rate limiting, resource exhaustion.** Handled by infrastructure (WAF, load balancer, connection limits). Not addressed at the Rust layer.
- **Byzantine L1 provider.** Our own node; honest by assumption.
- **Byzantine InputBox.** Audited L1 contract; trusted.
- **Memory safety.** Rust eliminates this class.
- **Secrets-at-rest security.** Handled by operator infra (secrets manager, file permissions, encrypted volumes).
- **Supply-chain compromise of dependencies.** Tracked via dependency pinning and out-of-band vulnerability feeds, not by code review.
- **Sequencer self-bugs as an attack vector.** Addressed via correctness review, tests, and manual intervention when they occur — see "Self-trust" above.

## External assumptions we rely on

These are preconditions the sequencer takes as given. They are neither "trust" nor "threat" — they are invariants about the environment that must hold for the design to be sound. If they break, the sequencer's safety guarantees degrade.

### L1 block-time coupling

The wall-clock fallback in [`sequencer/src/storage/recovery.rs`](../../sequencer/src/storage/recovery.rs) estimates missed blocks as:

```
estimated_missed_blocks = (now - last_safe_progress_ms) / CARTESI_SEQUENCER_SECONDS_PER_BLOCK
```

This assumes a **known, bounded-variance relationship** between elapsed wall-clock time and mined L1 block count. The assumption has three parts:

1. **Known average block time** — `CARTESI_SEQUENCER_SECONDS_PER_BLOCK` (default 12s, Ethereum mainnet) accurately reflects the target chain's block cadence.
2. **Bounded variance** — over the danger-threshold window (~4h on mainnet), the delta between `elapsed_seconds / avg_block_time` and actual mined blocks is small. On Ethereum mainnet this holds: slot proposers occasionally skip, but >99% of slots produce a block.
3. **Wall clock is accurate enough for elapsed-time estimation.** A discrete jump of a full block-time or more against either persisted safety baseline is detected and makes the L1 view unusable until the clock or a new safe-head observation catches up; sub-block skew is tolerated as quantization noise, and the fault is evaluated only after the observed-safe danger checks. Gradual or systematic drift remains an external assumption.

**Where it matters.** The missed-block estimate is a fallback for a safe head that stops advancing, whether the RPC is unreachable or still answers with a stalled view. Clock usability is also checked whenever persisted safety baselines are aged: observed block numbers remain authoritative and their danger verdicts run first, but a clock a full block-time or more out of step with either baseline still makes that view unusable for estimation and for worker admission.

**Violation modes.**
- **Chain with unstable block time.** A chain where average block time drifts substantially (e.g., PoW networks under major hashrate swings) would make the estimate less reliable. Mitigation: `CARTESI_SEQUENCER_SECONDS_PER_BLOCK` should be tuned conservatively (overestimate block time → underestimate missed blocks → more cautious recovery triggers).
- **Operator misconfigures `CARTESI_SEQUENCER_SECONDS_PER_BLOCK`.** Typo or copy-paste error pointing at the wrong chain's cadence. Operator-trust scope.
- **Significant host clock drift.** A sequencer host whose clock lags or leads the real-world by minutes per day could slowly desynchronize its danger estimates from reality. A detectable backward crossing of a persisted baseline refuses operation; gradual drift may not.

**Corollary for test design.** To deterministically exercise the wall-clock fallback, tests must maintain this coupling: when advancing the L1 block count, they should also advance (or simulate) the corresponding wall-clock interval. Our e2e harness does the reverse — it rewinds `l1_safe_head.synced_at_ms` to an older timestamp, which is semantically equivalent to advancing the wall clock.

### L1 chain semantics

The input reader, the frame clock, and the canonical scheduler use L1 block numbers and the `safe` tag as Ethereum defines them:

1. **Safe blocks are permanent.** The reader ingests through the sampled safe head and never rereads below it, and frames commit to that head. The sequencer reads `safe`, not `finalized`, for deposit latency: on Ethereum, finalized trails safe by about one more epoch (6.4 minutes), a large share of the [deposit-availability target](../../AGENTS.md#invariants).
2. **EVM `block.number` is the RPC block number.** The application's deployment block, which floors the scan, and each input's `EvmAdvance.blockNumber`, by which the scheduler drains and ages it, are EVM values. The reader uses them as log and safe-head coordinates, and frames carry RPC block numbers.

**Violation modes.**
- **A safe block is replaced**: an Ethereum justified checkpoint that never finalizes, or an L2 whose safe head rests on unfinalized parent-chain data (the OP Stack). Frames and soft confirmations already account for inputs L1 no longer has. The reader then stalls on an index or count mismatch, unless the replacement keeps the input count, in which case nothing detects it. The reader pins no block hash: it logs a warning only when a provider's safe head is numerically behind the persisted one.
- **Arbitrum and Orbit chains** return the parent chain's number from `block.number`. The scan floor lands far below the deployment, and the first ingested input fails the reader's block cross-check on every retry, after setup completes and, at the latest, once our first batch lands, with soft confirmations already issued.

**Revisit** before targeting any chain other than Ethereum L1 or its testnets. For an Arbitrum-family chain, first add a setup check that `block.number` at the sampled safe block equals that block's number. Before adopting a load-balanced or fallback endpoint tier, also pin the reader's count read to the sampled block's hash, so a backend that clamps a future block number errors instead of answering for another block.

## How to apply this doc in code review

For each code path under review:

1. **Where does the input come from?** Map the source to the actor table. Untrusted sources require validation; trusted sources do not.
2. **What are the downstream effects?** DB write, signed L1 submission, WS broadcast, process control. The more consequential the effect, the tighter the validation must be.
3. **Does the code assume any actor behaves better than the table says?** Common mistakes:
   - Assuming the mempool won't hold a tx indefinitely.
   - Assuming a tx we "gave up on" is permanently dead.
   - Assuming `safe_block` is current during an RPC outage.
   - Assuming the sequencer's own code is correct where a bug would breach a trust boundary (e.g., emit signed state to L1).
4. **Correctness or exploitation?** Both are in scope. Under rollup semantics, a correctness bug that causes state divergence is as severe as a direct exploit.

## Related documents

- [`../recovery/README.md`](../recovery/README.md) — recovery design, TLA+ formal verification
- [`../../AGENTS.md`](../../AGENTS.md) — architecture, coding conventions, hot-path invariants
