# Snapshots

Application snapshots are durable copies of the app's canonical state at a known
point in the L2-tx stream. They let the inclusion lane resume on startup with a
single *load-then-replay* instead of replaying all history, and they back the
operator's watchdog (`/finalized_state`) and indexers (`/latest_snapshot`).

Two documents, split by concern:

- **[`format.md`](format.md)** — the on-disk *format*: the `Application` dump
  trait (`from_dump` / `create_dump` / `delete_dump` / `state_file_in_dump`) and
  the toy wallet's SSZ wire encoding. What a dump *is*.

- **[`lifecycle.md`](lifecycle.md)** — the *lifecycle* and its rationale: take at
  batch close, pending → finalized promotion (per-range, atomic with the drain),
  garbage collection, HTTP leasing, recovery interaction, and the crash-safety
  reasoning (including the promote/drain wedge and why the design closes it).
  When and how dumps move through the system, and *why*.

Related: [`../recovery/README.md`](../recovery/README.md) (danger-zone recovery,
which clears cascade-doomed pendings), [`../../AGENTS.md`](../../AGENTS.md)
(architecture), and the root [`../../README.md`](../../README.md) (endpoint
shapes).
