# C application binding

An application exposes the
[`application-engine.h`](../../bindings/c-app-engine/include/application-engine.h)
C ABI in a static archive. `c-app-engine::EngineApp` adapts that engine to the
Rust `Application` trait. These reusable bindings live under `bindings/`;
`c-app-sequencer` supplies an optional CLI host. The
[build guide](../../bindings/c-app-engine/README.md) covers downstream dependencies,
linking, genesis, host commands, and the reference wallet under `examples/`.

## Contract ownership

- The [Application contract](application-contract.md) defines execution,
  rejection, progress, deterministic outputs, and checkpoint obligations.
- [Scheduler semantics](scheduler-semantics.md) defines canonical ordering and
  the acceptance boundary. The native engine does not acquire an ordering role
  through this ABI.
- The [C header](../../bindings/c-app-engine/include/application-engine.h)
  defines record layout, statuses, pointer ownership, and call lifetimes. An
  engine archive and its generated Rust bindings must agree on that header.
- [Snapshot lifecycle](../snapshots/lifecycle.md) owns checkpoint registration,
  promotion, reader leases, and garbage collection. The application owns its
  checkpoint representation within the supplied prefix.

## Execution and ownership

Each opaque engine handle owns its mutable state. It may move between threads,
and calls on that handle are exclusive. Progress is returned together as
`ApplicationEngineProgress`; the shared execution boundary checks it against the
successful transition. The payload-bound getter takes no instance and returns a
stable value for the linked engine. Zero is a valid bound.

Expected validation rejection leaves state unchanged. An execution, validation,
or output-drain failure discards the engine; the host owns process termination
and distinguishes terminal faults from retryable operational failures.
Exceptions must not cross the ABI. The header defines the lifetimes of borrowed
output and diagnostic buffers; the adapter copies them before reuse.

Fee fields carry base-129/128 exponents. The shared max-fee comparison operates
in log space; an application checking balances or charging fees uses a linear
amount. The reference conversion lives in
[`sequencer-core/src/fee.rs`](../../sequencer-core/src/fee.rs). Native and
canonical execution must agree on that conversion, since different amounts can
change rejection decisions and resulting balances.

The same implementation may be compiled for native execution and the canonical
machine. This does not establish equivalent behavior across targets: the
application must preserve deterministic state and output bytes, including the
checkpoint's canonical comparison file. Reference wallet ABI tests exercise
the host integration; they do not establish private DEX conformance or
equivalence between native and machine execution.
