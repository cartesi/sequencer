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
| InputBox contract | Trusted | Authenticates `msg_sender` on `addInput`. Use correctly; do not model forgery. |
| Our Ethereum node | Trusted, fail-stop | Inside our infra. May become unreachable; will never lie. |
| Fallback RPC (Infura / Alchemy) | Semi-trusted, fail-stop | Liveness fallback during primary outages. May withhold or delay. Never byzantine. |
| Operator env / CLI flags | Trusted | Configuration is authoritative. |
| Batch-submitter private key | Private | Held in operator infra. Not reachable by the network. |
| Sequencer's own code | Trusted (bug-free is a precondition) | Bugs are caught via tests and review, not defended against at runtime. See "self-trust" below. |
| **L1 mempool and block builders** | **Fully adversarial** | May reorder, delay, drop, or selectively include submitted transactions. Private mempools mean "dropped" is indistinguishable from "delayed indefinitely." |
| HTTP clients at `POST /tx` | Untrusted | Arbitrary public callers. May submit malformed, malicious, or replay payloads. |
| WebSocket subscribers at `/ws/subscribe` | Internal, but untrusted for data-exposure | Intended for internal indexers. Treat as public for what is exposed. |
| Direct-input senders on L1 | Untrusted | Arbitrary L1 accounts calling InputBox. May submit any calldata. |

### Self-trust

The sequencer trusts that its own code is correct. If the sequencer emits a malformed batch, frame, or user op, it is already in a bug state that requires manual intervention — we do not layer runtime defenses against sequencer self-misbehavior. Recovery addresses liveness failures (infrastructure outages, network partitions, gateway failure), not bug-induced malformed state.

This is not an excuse to skip validation at trust boundaries. Inputs from untrusted actors are validated rigorously. Internal invariants are enforced by type system, SQL constraints, and tests — not by defensive runtime checks against hypothetical self-misbehavior.

## In-scope failure modes

- L1 provider outages (primary and fallback), minutes to hours
- Process crashes at arbitrary points, including mid-transaction
- **Adversarial mempool:** reorder, delay, drop, selective inclusion by builders
- **Zombie transactions:** a submitted batch may sit in a private mempool indefinitely and land long after we believed it was gone. The recovery flusher is load-bearing for this threat: it consumes every pending `w_nonce` slot with a no-op so zombies cannot claim them.
- L1 reorgs up to safe depth
- Malicious `POST /tx` callers: malformed signatures, spoofed sender, replay across chains or apps, nonce manipulation
- Malicious direct-input senders: arbitrary payload, any intent; sender authenticity is guaranteed by InputBox
- Scheduler/sequencer protocol divergence of any kind (ordering, nonce rules, signature validity, fee semantics)

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
estimated_missed_blocks = (now - last_sync_ms) / SEQ_SECONDS_PER_BLOCK
```

This assumes a **known, bounded-variance relationship** between elapsed wall-clock time and mined L1 block count. The assumption has three parts:

1. **Known average block time** — `SEQ_SECONDS_PER_BLOCK` (default 12s, Ethereum mainnet) accurately reflects the target chain's block cadence.
2. **Bounded variance** — over the danger-threshold window (~4h on mainnet), the delta between `elapsed_seconds / avg_block_time` and actual mined blocks is small. On Ethereum mainnet this holds: slot proposers occasionally skip, but >99% of slots produce a block.
3. **Wall clock is monotonic and accurate** — the host's `SystemTime::now()` does not jump backward significantly or drift. Handled by saturating subtraction against clock backward jumps, but not against systematic drift.

**Where it matters.** Only on the fallback path — when L1 is unreachable and we cannot observe block numbers directly. When L1 is up, observed block numbers are authoritative and this assumption is not consulted.

**Violation modes.**
- **Chain with unstable block time.** A chain where average block time drifts substantially (e.g., PoW networks under major hashrate swings) would make the estimate less reliable. Mitigation: `SEQ_SECONDS_PER_BLOCK` should be tuned conservatively (overestimate block time → underestimate missed blocks → more cautious recovery triggers).
- **Operator misconfigures `SEQ_SECONDS_PER_BLOCK`.** Typo or copy-paste error pointing at the wrong chain's cadence. Operator-trust scope.
- **Significant host clock drift.** A sequencer host whose clock lags or leads the real-world by minutes per day could slowly desynchronize its danger estimates from reality.

**Corollary for test design.** To deterministically exercise the wall-clock fallback, tests must maintain this coupling: when advancing the L1 block count, they should also advance (or simulate) the corresponding wall-clock interval. Our e2e harness does the reverse — it rewinds `l1_safe_head.synced_at_ms` to an older timestamp, which is semantically equivalent to advancing the wall clock. See [`tests/TEST_PLAN.md`](../../tests/TEST_PLAN.md) §7.8 and tool T7.

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
- [`../../SECURITY_TODO.md`](../../SECURITY_TODO.md) — open findings from staged security review
