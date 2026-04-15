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
