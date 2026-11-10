# Inside the sequencer: what remains

The sequencer's side of this plan is built: loading a machine, the rebuild
with the scheduler alone, the rebuilt history table, the era, the feed across
the rebuilt and live rows, and the lane draining from the first queued input.
[Cockroach recovery](../../recovery/cockroach.md) describes them, with a code
map. This page keeps only what is not built yet.

- **A large application state.** Setup reads the base's state as one `state`
  inspect report, which holds at most 2 MiB: 27,593 wallet accounts that hold
  a balance and a nonce, or 40,328 that hold only a balance. That size, not the loader's
  2^36-cycle bound per query, is the limit that binds
  ([measurements](../../review/2026-11-10-recovery-measurements.md#b-reading-a-base-machine)).
  A larger state needs another state source, such as a labeled memory range,
  or a guest that splits its answer
  ([the loader's limits](../../cartesi-machine.md#reading-a-stored-machines-export)).
- **Which side links the scheduler.** `bindings/c-scheduler` exports the Rust
  scheduler through `scheduler.h`; whether a C guest links it or a C port
  implements the header, and so whether the sequencer would link a foreign
  scheduler for the rebuild, is open
  ([questions for Bart](todo.md#questions-for-bart)).
- **Names.** The storage columns call the L2 tx count `l2_tx_count`; the HTTP
  API still calls it `executed_input_count`, which changes with the
  [egress redesign](todo.md#redesign-the-egress-api).
