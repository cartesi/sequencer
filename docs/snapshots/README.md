# Snapshots

Application snapshots are immutable, durable copies of application state at a
known execution boundary. They let the inclusion lane resume with load and
replay. A snapshot may contain optimistic state; L1 acceptance determines which
artifact can back a canonical comparison or recovery export.

Two documents, split by concern:

- **[`format.md`](format.md)** — the on-disk *format*: the `Application` dump
  trait (`from_dump` / `create_dump` / `state_file_in_dump`) and
  the toy wallet's SSZ wire encoding. What a dump *is*.

- **[`lifecycle.md`](lifecycle.md)** — creation at batch close, restart selection,
  acceptance-derived comparison checkpoints, recovery exports, retention,
  download leases, and crash safety. Acceptance is a separate durable fact;
  artifacts are never promoted or rewritten.

For automatic startup repair, see [standard recovery](../recovery/README.md).
For rebuilding after database loss or a sequencer bug, see
[cockroach recovery](../recovery/cockroach.md). The root
[README](../../README.md) owns endpoint shapes.
