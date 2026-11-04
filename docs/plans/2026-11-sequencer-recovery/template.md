# Rules for the next template

Each rule changes the guest, so each needs Bart's agreement and ships with a
new template, ours and his port's. A deployed application keeps its template
for life ([Cartesi Machine facts](../../cartesi-machine.md#versions-are-all-or-nothing)).

## The rule this plan needs

- **Scheduler state export.** The guest answers the `scheduler` and `fridge`
  inspect queries ([application](application.md#the-canonical-machine)). For
  that it records the inputs it has consumed, and each queued direct keeps its
  input index.

## Rules that ride along

Recovery does not depend on these, but they ship with the same template.

- **Fridge bound.** Only well-formed directs, by the application's direct
  filter, enter the fridge; the rest are dropped and never execute. A count
  bounds the fridge, and an overflow drains just enough of its front to make
  room. The sequencer's lane drops the same directs with the same filter. The
  sequencer cannot predict an overflow, so the batches it already sealed
  diverge, and a manual recovery answers that.
- **Progress is the L2 tx count alone.** `last_executed_safe_block` leaves the
  shared progress contract, the C binding's progress and the wallet's
  comparison bytes. Nothing reads it in this design. Today two paths read it,
  `setup --recovery` and a client's projection replay, and both go.
- **Direct inputs carry what libdex reads.** A user op never sees its batch's
  context. Which of a direct's index, timestamp and prevRandao reach the
  application waits on Bart's answer. The index and timestamp cost nothing,
  since the sequencer's input table and the feed carry them; prevRandao needs
  a column and a feed field, and the block proposer can bias it, so it comes
  only on a stated need.

## Open rules

Both were settled on 2026-10-29 for the shared recovery tool. This plan needs
neither, so each stays only if its semantics are wanted for their own sake.

- **The floor gate.** The scheduler keeps the latest safe block an accepted
  frame used, raised by an overflow, and rejects a batch whose first frame lies
  below it. Then a transaction's block never goes backwards along the L2
  sequence, and applications may read blocks as time. If the gate stays, its
  block is one more field of the exported scheduler state. It costs a
  consensus gate, I3 becoming consensus-critical, and a self-trust exemption
  in `scheduler_accepts`. Keep it only if libdex reads blocks as time.
- **Authorization decides inclusion.** An op whose signature, nonce and fee
  cap check out always executes, advancing the nonce and paying what its sender
  can, even when the sender cannot pay the whole fee. Today such an op is
  skipped. The rule let a recovery tool that ran without the application know
  which ops execute; here the application decides. It would make every
  application's apply charge what the sender has, where the wallet's treats an
  unpayable fee as fatal today, and let a faulty sequencer make libdex store a
  nonce for every unfunded sender it posts, at no fee.

## Dropped from the earlier plans

These served a fridge rebuilt from L1, which this plan reads from the machine
instead:

- strict frame clocks, which would also have rewritten 18 of the 19 frozen
  guest scenarios;
- whole-block overflow drains, and the condition that the bound hold a
  block's directs;
- a scheduler clock raised by every drain;
- a direct floor kept as a block, and the scheduler query's four values.
