# One recovery: when batches stop landing

**Status:** steps 1 to 4 of six are code. The sequencer has one recovery,
[cockroach recovery](../../recovery/cockroach.md), and repairs nothing by
itself. This page keeps what remains.

[Batch landing and outages](../../recovery/README.md) is the home of what was
built: the rule that ends an era, what the sequencer does at start and while
running, the two numbers it reports, the scenarios with their devnet drills,
the operator's playbook, and why there is no automatic repair.

## What was built

1. **The window is the application's.** `max_wait_blocks` is a field of the
   application's scheduler limits, setup pins it with the deployment identity,
   and the C engine's ABI is version 3 for it
   ([application contract](../../protocol/application-contract.md#3-admission--policy-never-consensus)).
2. **The sequencer stops for good on a void history.** At start and while
   running it checks two terminal facts, a recorded divergence and a void
   history, and refuses on either
   ([what the sequencer does](../../recovery/README.md#what-the-sequencer-does)).
   The danger detector with its margin and wall-clock estimates, the flush at
   startup, the cascade and the open-batch repair are gone, with exit code 10,
   `--preemptive-margin-blocks` and `--l1-read-stale-after-blocks`. The two
   TLA+ models of that mechanism are [archived](../../recovery/history/README.md).
3. **The batches are an append-only list.** No batch, frame, user op or
   application input is ever replaced or deleted, and the schema carries no
   invalidation ([I5](../../invariants.md#i5-batches-and-application-history-only-append)).
   A nonce then names one batch for the life of the database, so it is the
   batch's key: the separate local index and its `BatchId` type are gone.
4. **A history's identity is its era.** The recovery generation is gone from
   every client interface and from the database: a claim is an era id and a
   count, and the subscribe query and `/history` refuse a field they do not
   know, so a client that still sends a generation fails loudly
   ([history contract](../../protocol/application-history.md#identity-and-available-history)).

## What remains

Each step is its own commit, compiles and passes on its own, and carries its
docs. A step that touches a guest crate moves the template hash, so the images
are rebuilt before the machine and devnet suites run.

5. **The wallet's window becomes 7200 blocks**, about 24 hours. At 1200 with
   the default two-hour batch, a quiet wallet deployment has under two hours
   to land each batch, and a posting stall past that ends the era. Tests do
   not need the short window: they mine it in one call. Two things move with
   it. The consensus vectors: four of the 41 guest scenarios pin the window's
   boundary, and for them ten landing blocks, one batch payload, two expected
   clocks and the config value change, while outcomes, transactions, outputs
   and states do not. And one acceptance test with literal blocks, which
   derives them from the window instead. The worked-boundary rebuild test
   also gains a second stop block right after its malformed batch: it
   documents the backstop firing, but its rows cannot tell a backstop drain
   from a frame's.
6. **A last pass over the docs**, once the rest is in: the invariant register,
   the threat model and `AGENTS.md`.
