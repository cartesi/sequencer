# Snapshots

Application snapshots are immutable, durable copies of application state at a
known execution boundary. They let the inclusion lane resume with load and
replay. A snapshot may contain optimistic state; L1 acceptance determines which
artifact can back a canonical comparison or recovery export.

Keep three boundaries separate: the engine embeds its executed-input count and
safe-block clock; SQLite associates an artifact with a local batch or era
baseline; safe L1 acceptance selects a comparison checkpoint. Counts and clocks
alone do not establish acceptance. The
[history guide](../protocol/application-history.md) explains these coordinates
and snapshot-plus-replay bootstrap.

- [Application contract](../protocol/application-contract.md#6-checkpoint-lifecycle)
  — engine dump methods, durability, immutable artifacts, independent restore,
  and the canonical comparison file.
- [Wallet format](format.md) — the wallet's layout, deterministic SSZ encoding,
  and decode rules.
- [Lifecycle](lifecycle.md) — creation at batch close, restart selection,
  acceptance-derived comparison checkpoints, recovery exports, retention,
  download leases, and crash safety. Acceptance is a separate durable fact;
  artifacts are never promoted or rewritten.

For automatic startup repair, see [automatic recovery](../recovery/README.md).
For rebuilding after database loss or a sequencer bug, see
[cockroach recovery](../recovery/cockroach.md). The root
[README](../../README.md) owns endpoint shapes.
