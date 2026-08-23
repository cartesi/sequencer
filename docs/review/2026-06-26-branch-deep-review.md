# Deep branch review — setup/run split → cockroach recovery (2026-06-26)

Whole-branch review of the setup/run split, scheduler-library extraction,
fold engine, and `setup --recovery`, plus a follow-on multi-agent adversarial
sweep. **Eight findings confirmed, all fixed; three refuted.**

The lasting outcomes live elsewhere: the recovery spec is
[`docs/recovery/cockroach.md`](../recovery/cockroach.md); the authoritative
protocol contracts written during this review are
[`docs/protocol/scheduler-semantics.md`](../protocol/scheduler-semantics.md)
and
[`docs/protocol/application-contract.md`](../protocol/application-contract.md);
per-finding dispositions and the deferred-with-reason items (e.g. the
`FoldInputSource` abstraction, declined as over-abstraction over a single
call site) are in [`register.md`](register.md).

Notable fixed findings, for the record: partial-recovery re-anchoring and
genesis-over-residue guards; the recovery drain capped at `C` so `(C, H1]`
deposits are led exactly once by `run`; wrong-chain RPC during recovery sync
classified terminal instead of restart-looping.
