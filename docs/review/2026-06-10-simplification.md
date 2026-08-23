# Simplification & refactoring review (2026-06-10)

Companion to the correctness review: what was *heavier than it needed to
be*, ranked and sequenced. **Verdict: no architectural restructure** — the
module layout (one file per writer role, `*_in(tx)` free functions composing
into larger transactions, storage-owns-SQLite / lane-owns-filesystem) is
sound and should be defended, not redesigned. The weight traced to unpinned
cross-file invariants (answered by creating
[`docs/invariants.md`](../invariants.md)), test-only surface presenting as
production API, and duplicated semantics — each with a single-home fix.

The queue's durable outcomes: the scheduler-mirroring logic homed beside
`scheduler_accepts`, the shared recovery tail (`cascade_and_reopen`), the
fail-loud conversion of the storage decode layer, and the
**do-not-simplify list**, which now lives beside the invariants it protects
([`docs/invariants.md`](../invariants.md), "Do-not-simplify"). Open
remnants and deliberate declines-with-reasons are in
[`register.md`](register.md).
