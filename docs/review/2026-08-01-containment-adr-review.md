# Containment ADR review (2026-08-01)

Two review rounds on the terminal-containment cutover branch. The
architectural turn was accepted, then re-evaluated with the maintainer
(2026-08-02): the unimplemented `LiveKernel`/reader-mailbox design was
rejected on the actual completeness/cost boundary — the content-identity
check is a narrow backstop, not a divergence oracle, and SQLite remains the
durable coordination plane.

Every mechanism this review shaped now lives in the
[authority-boundary ADR](../plans/2026-08-authority-boundary-adr.md)
(including the rejected `RunEpoch`/`EffectGate`/`LiveKernel` alternatives and
the accepted divergence-window bound recorded on
[I15](../invariants.md)); the intermediate marker-file protocol it hardened
was later deleted wholesale. Per-finding dispositions are in
[`register.md`](register.md).
