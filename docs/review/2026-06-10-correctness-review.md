# Whole-project correctness review (2026-06-10)

Twelve parallel module reviews plus line-by-line passes over the core files,
every medium/high concern independently adversarially verified. **Headline:**
the sequencer/scheduler duality itself was in good shape; the confirmed
problems clustered at the boundary with the infrastructure underneath —
fsync semantics, the local node's mempool memory, RPC fleet coherence, and
the subscriber protocol.

Ten findings (F1–F10) and five design resolutions (R1–R5) came out of it;
all findings except the WS invalidation contract (owned by the Track 3
handoff) are fixed, and the resolutions became the write-before-broadcast
watermark (I14), the content-identity check (I9/I15), `synchronous=FULL`,
the exit-code contract, and the fail-loud check policy that now opens
[`docs/invariants.md`](../invariants.md).

Everything still actionable — the remaining robustness/hygiene backlog and
the refuted concerns that must not be re-litigated — lives in
[`register.md`](register.md); the full original ledger is in git history.
