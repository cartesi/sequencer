# Settled design — cockroach-recovery batch-tree rooting (2026-06-25)

Design session (with an adversarial panel) for how `setup --recovery` roots
the rebuilt batch tree at the resume nonce `N'`.

**Decision: the `batch_tree_anchor` singleton** — the parentless root carries
the anchor nonce (0 for genesis, `N'` for recovery), validated exactly by the
contiguity trigger and frozen once setup completes. No sentinel batch row.
The design now lives in [I16](../invariants.md) and
[`docs/recovery/cockroach.md`](../recovery/cockroach.md); the rejected
sealed-sentinel alternative and the "`N` is trusted — no recovery-time
verifier" resolution (the proposed cross-check was circular) are recorded in
[`register.md`](register.md).

The follow-up e2e round trip caught a real bug — the content-identity check
self-diverged during recovery against the empty rebuilt tree — fixed by the
anchor-aware frontier (recorded on I15) with frontier population deferred to
`run`'s first sync.
