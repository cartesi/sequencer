# One recovery: when batches stop landing

**Status:** steps 1 to 3 of five are code. The sequencer has one recovery,
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

## What remains

Each step is its own commit, compiles and passes on its own, and carries its
docs. A step that touches a guest crate moves the template hash, so the images
are rebuilt before the machine and devnet suites run.

4. **A history's identity is its era.** No operation advances the recovery
   generation, so it leaves every client interface and the database:
   `RecoveryGeneration`, `STALE_GENERATION`, `from_generation` and
   `compatibility` on `/history`, `X-Recovery-Generation`, the generation in
   the WS claim, the SDK's handling, and `history_generation_cuts`.
   `HistoryVersion` goes: a claim is an era id and a count. The subscribe
   query refuses a field it does not know, so a client that still sends a
   generation fails loudly.
5. **A last pass over the docs**, once 3 and 4 are in: the invariant register,
   the threat model and `AGENTS.md`.

## Still to decide

- **The wallet's window.** At 1200 blocks with the default two-hour batch, a
  quiet wallet deployment has under two hours to land each batch, and a
  posting stall past that ends the era. Tests do not need the short window:
  they mine it in one call. Moving the wallet to 7200 after step 4 touches
  two things. The consensus vectors: four of the 41 guest scenarios pin the
  window's boundary, and for them ten landing blocks, one batch payload, two
  expected clocks and the config value move, while outcomes, transactions,
  outputs and states do not. And one acceptance test with literal blocks,
  which should derive them from the window. The worked-boundary rebuild test
  also needs a second stop block right after its malformed batch: it
  documents the backstop firing, but its rows cannot tell a backstop drain
  from a frame's.
- **The batch's key.** A batch nonce names one batch for the life of an era,
  so `BatchId` and the nonce are two names for one thing. Making the nonce the
  key touches every table that points at a batch; it is decided after step 3,
  on what the schema looks like then.
