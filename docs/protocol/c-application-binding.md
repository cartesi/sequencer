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
  nonces, admission, the count, deterministic outputs, and checkpoint
  obligations.
- [Scheduler semantics](scheduler-semantics.md) defines canonical ordering and
  the acceptance boundary. The native engine does not acquire an ordering role
  through this ABI.
- The [C header](../../bindings/c-app-engine/include/application-engine.h)
  defines record layout, statuses, pointer ownership, and call lifetimes. An
  engine archive and its generated Rust bindings must agree on that header.
- [Snapshot lifecycle](../snapshots/lifecycle.md) owns checkpoint registration,
  acceptance-derived selection, reader leases, and garbage collection. The
  application owns its checkpoint representation within the supplied prefix.

## Execution and ownership

Each opaque engine handle owns its mutable state. It may move between threads,
and calls on that handle are exclusive. `EngineApp` first checks
`application_engine_abi_version()` against the header's
`APPLICATION_ENGINE_ABI_VERSION`, before any other call, so an archive built
against another header is refused rather than run under changed rules.

The calls map onto the [execution methods](application-contract.md#the-execution-methods)
and [admission](application-contract.md#3-admission--policy-never-consensus):

- `application_engine_user_nonce` reports a sender's next nonce; the shared
  executor, not the engine, compares it with each op and checks that an
  execution advanced it by exactly one.
- `application_engine_execute_user_op` and
  `application_engine_execute_direct_input` are total: every op handed to the
  engine executes, a fee its sender cannot cover included, and every execution
  advances the count by one. A direct arrives with its input index and
  timestamp.
- `application_engine_l2_tx_count` reports the engine's executed-input count,
  which the executor checks against each successful transition.
- `application_engine_admit_user_op` is lane-only policy. It answers `INVALID`
  with `INSUFFICIENT_FEE_BALANCE`, or with `REFUSED_BY_POLICY` and its text in
  the last error message, which reaches the client verbatim. Retired reasons
  keep their numbers, and the adapter treats them, or an unknown one, as fatal.
- `application_engine_scheduler_limits` and
  `application_engine_max_method_payload_bytes` take no instance and return
  stable values for the linked engine. Zero is a valid payload bound.
- `application_engine_from_canonical_state` opens an engine over a canonical
  machine's comparison bytes at that machine's count
  ([canonical recovery](application-contract.md#7-canonical-recovery-integration)).

An admission, nonce-read, execution, or output-drain failure discards the
engine; the host owns process termination and distinguishes terminal faults
from retryable operational failures. Exceptions must not cross the ABI. The
header defines the lifetimes of borrowed output and diagnostic buffers; the
adapter copies them before reuse.

Fee fields carry base-129/128 exponents. The shared max-fee comparison operates
in log space; an application checking balances or charging fees uses a linear
amount. The exact conversion contract lives on `fee_to_linear` in
[`sequencer-core/src/fee.rs`](../../sequencer-core/src/fee.rs): ports must match
the table, intermediate flooring, and ascending bit order, not just the nominal
exponential formula. [`build.rs`](../../sequencer-core/build.rs) generates the
table and exponent bound. Native and canonical execution must agree on the
conversion, since different amounts can change balances and admission decisions.

The same implementation may be compiled for native execution and the canonical
machine. This does not establish equivalent behavior across targets: the
application must preserve deterministic state and output bytes, including the
checkpoint's canonical comparison file. Reference wallet ABI tests exercise
the adapter contract; the `c_host_` process scenarios exercise snapshot replication,
restart, stale-batch recovery, and canonical-machine comparison as described in
the [build guide](../../bindings/c-app-engine/README.md#reference-wallet).
These tests establish reference-wallet agreement for their workloads, not
private DEX conformance.
