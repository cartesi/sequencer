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
| InputBox contract | Trusted | Authenticates `msg_sender` on `addInput`. Must be rollups-contracts **v3+**: the reader anchors its scan at the application's deployment block, sound only because the v3 InputBox reverts `addInput` for not-yet-deployed apps. Bootstrap witnesses this via `version()` and refuses pre-v3 boxes. Use correctly; do not model forgery. |
| Our Ethereum node | Trusted, fail-stop | Inside our infra. May become unreachable; will never lie. |
| RPC endpoint (`CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`) | Trusted, fail-stop — **must be one consistent node** | The code supports exactly **one** endpoint, shared by reader, submitter, poster, flusher, and fee oracle; no fallback tier exists yet. Behind a load-balanced fleet, lagging replicas can silently truncate `get_logs` ranges and desynchronize the flush/re-sync views. The reader fails loud on an incomplete InputAdded set: a per-app index contiguity check (right prefix) plus a `getNumberOfInputs` count witness pinned at the scanned safe block (complete prefix), so a dropped/clamped/truncated-tail input is detected before the safe head advances rather than silently skipped, and recovery refuses if the re-sync lags the flush view. (The count witness is fetched before the scan; when it matches the stored input count the reader advances the safe head without a `get_logs` crawl — same trust base, since a fail-stop node's pinned-block count cannot understate without lying.) The residual fleet exposure is a node that lies *consistently* about both its logs and its input count — outside the fail-stop model. A semi-trusted fallback tier (Infura/Alchemy) is future work. |
| Operator env / CLI flags | Trusted, **mistakes foot-gun-guarded** | Setup configuration is authoritative — including the reviewed Uniswap V3 WETH/fee-token pool the fee oracle quotes. The complete source is pinned in deployment identity; run cannot replace it. The operator is trusted, not infallible: the supported operator-mistake class is *accidental concurrent or stale use of one data directory* — two processes on one dir (kernel process lock), a mistyped `--data-dir` (open refuses paths with no database). Deliberate operator subversion, copied-directory coordination, and distributed fencing remain out of scope (see the ADR's non-goals); mechanisms defending this class are judged against this boundary rather than re-litigated per review. |
| Uniswap V3 pool (fee oracle) | Semi-trusted L1 state | Spot manipulation of a deep pool is mitigated by a 30-minute TWAP plus 10× slack in `batch_policy.log_slack`. Residual risks: TWAP lag during real moves and thin/wrong pool misconfiguration. Multi-hop pricing is out of scope. Setup writes the first Uniswap quote (same hard L1 requirement as the rest of setup). `run` tolerates transient connect/refresh failures and continues from the persisted `batch_policy.log_gas_price`, matching the warm-boot policy for unreachable RPC once identity is pinned; non-transient misconfig (`WrongTokenPair`, `MissingPoolCode`, chain-id mismatch) stays terminal. Freshness is bounded by a persisted `log_gas_price_updated_at_ms` stamped on every successful write, enforced at boot and in `run_forever` against the same max-age as L1 read-staleness (`l1_read_stale_after_blocks * seconds_per_block`). A pool/`observe` failure while the RPC and input reader stay healthy therefore does **not** trip the L1 stale-view danger detector — under the trusted-pool assumption that is accepted residual risk until the fee-oracle max-age fires. |
| Batch-submitter private key | Private | Held in operator infra. Not reachable by the network. |
| Sequencer's own code | Trusted (bug-free is a precondition) | Bugs are prevented through tests/review and contained by fail-loud runtime invariant checks; they are not treated as adversarial behavior that the protocol can recover around. See "self-trust" below. |
| **L1 mempool and block builders** | **Fully adversarial** | May reorder, delay, drop, or selectively include submitted transactions. Private mempools mean "dropped" is indistinguishable from "delayed indefinitely." |
| HTTP clients at `POST /tx` | Untrusted | Arbitrary public callers. May submit malformed, malicious, or replay payloads. |
| WebSocket subscribers at `/ws/subscribe` | Internal, but untrusted for data-exposure | Intended for internal indexers. Treat as public for what is exposed. |
| Direct-input senders on L1 | Untrusted | Arbitrary L1 accounts calling InputBox. May submit any calldata. |

### Self-trust

The sequencer trusts its own code in a specific sense: **impossible states are never *handled*.** There are no graceful fallback paths, no re-validation of a neighbor module's answer, no code that keeps running past a violated internal contract. If the sequencer emits a malformed batch, frame, or user op, it is in a bug state that requires manual intervention; normal preemptive recovery addresses liveness failures (infrastructure outages, network partitions, gateway failure), not bug-induced malformed state. Cockroach recovery is the separate operator-directed rebuild path when durable state cannot be trusted.

This is **not** a prohibition on checking. Internal invariants are enforced loudly wherever a check is near-free — the type system, SQL constraints and triggers, boundary assertions — because failing loud preserves safety, while a silently-tolerated bug that externalizes (a signed batch, an ack, a feed event) is state divergence: as severe as theft and undefendable at runtime. Loud failure is not automatically self-healing: transient faults may clear on restart, but persistent invalid state is terminal and may require inspection or cockroach recovery. The rule, in short: **assert real invariants, fail loud, never absorb silently, never handle gracefully.** The decision test and the register of cross-module invariants live in [`docs/invariants.md`](../invariants.md).

Inputs from untrusted actors are validated rigorously, as ever.

## In-scope failure modes

- L1 provider outages (primary and fallback), minutes to hours
- Process crashes at arbitrary points, including mid-transaction
- **Restart after a terminal exit (accepted residual window).** There
  is no boot gate on a prior terminal verdict: a deliberate restart after
  exit 30 boots through the fact-derived reducer. Every fault whose evidence
  the boot path reads re-refuses before the first soft confirmation —
  canonical divergence (persisted fact), misconfiguration (re-checked every
  boot), boot-path storage corruption, incomplete setup — and the
  batch/frame spine is re-inspected by the runtime danger detector within
  seconds of launch. The accepted residual is narrow: corrupt payload bytes
  in rows at/below the lane's resume checkpoint re-trip only when the WS
  feed pages them (bounded by its catch-up window) or the submitter
  re-encodes a pending batch, and a fault with no durable evidence (a panic
  whose trigger does not recur) does not re-trip at all. The window is
  entered only by a deliberate operator restart after an exit-30 page, and
  it is bounded by backstops that never depended on a boot gate:
  rollbackable soft confirmations, the watchdog byte-compare, and the I15
  divergence freeze.
- **Adversarial mempool:** reorder, delay, drop, selective inclusion by builders
- **Zombie transactions:** a submitted batch may sit in a private mempool indefinitely and land long after we believed it was gone. Two load-bearing defenses: the recovery flusher consumes every wallet-nonce slot this deployment ever used (anchored by the persisted watermark, I14) so zombies cannot claim them; and the content-identity check (I9/I15) compares every at/above-anchor *simulated-accepted* landing against the valid closed batch we sealed at that nonce. A foreign or byte-different landing records divergence when it becomes safe and is ingested, freezes the accepted frontier, and requires cockroach recovery. This is trust-boundary validation of external input (the mempool replaying our own stale transactions at times we don't control), not defense-in-depth against self-bugs or a general canonical-state oracle. In cockroach recovery the watermark does not survive the wipe, so that flush is best-effort by construction; the content-identity check is what keeps the residual zombie detected-and-frozen rather than silent (see `docs/recovery/cockroach.md`, step 2).
- L1 reorgs up to safe depth
- Malicious `POST /tx` callers: malformed signatures, spoofed sender, replay across chains or apps, nonce manipulation
- Malicious direct-input senders: arbitrary payload, any intent; sender authenticity is guaranteed by InputBox
- Scheduler/sequencer protocol divergence of any kind (ordering, nonce rules, signature validity, fee semantics) is an in-scope correctness consequence. The content-identity check detects accepted-batch identity failures only; there is no complete runtime detector for the broader class. Shared semantics, review, and tests are preventative, and cockroach recovery is the remedy only after another signal or operator investigation diagnoses divergence.

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

The wall-clock fallback in [`sequencer/src/recovery/mod.rs`](../../sequencer/src/recovery/mod.rs) estimates missed blocks as:

```
estimated_missed_blocks = (now - last_sync_ms) / CARTESI_SEQUENCER_SECONDS_PER_BLOCK
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
