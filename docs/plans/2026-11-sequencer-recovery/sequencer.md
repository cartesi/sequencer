# Inside the sequencer: what remains

The sequencer's side of this plan is built: loading a machine, the rebuild
with the scheduler alone, the rebuilt history table, the era, the feed across
the rebuilt and live rows, and the lane draining from the first queued input.
[Cockroach recovery](../../recovery/cockroach.md) describes them, with a code
map. This page keeps only what is not built yet.

- **A large application state.** Setup reads the base's state as one `state`
  inspect report, which holds at most 2 MiB; the wallet fits about 27,000
  accounts. A larger state needs another state source, such as a labeled
  memory range, or a guest that splits its answer
  ([the loader's limits](../../cartesi-machine.md#reading-a-stored-machines-export)).
- **The loader's cycle bound.** Each inspect query may run 2^36 cycles before
  the loader gives up; nobody has measured what a large state's encoding costs
  against it ([measure](todo.md#measure)).
- **Which side links the scheduler.** `bindings/c-scheduler` exports the Rust
  scheduler through `scheduler.h`; whether a C guest links it or a C port
  implements the header, and so whether the sequencer would link a foreign
  scheduler for the rebuild, is open
  ([questions for Bart](todo.md#questions-for-bart)).
- **Names.** Code still says `ExecutedInputCount`, `executed_input_count()`
  and `Executed::Done { offset }` for the L2 tx count; a rename-only change
  follows ([promote to real docs](todo.md#promote-to-real-docs)).
