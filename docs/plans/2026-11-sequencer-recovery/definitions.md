# Definitions

These belong in real docs, not in a plan. Each section names its target, and
[todo](todo.md#promote-to-real-docs) tracks the move. A rule marked *next
template* is not true today; it lands with the [template rules](template.md).

## The L2 transaction sequence

Target: a new opening section of
[scheduler semantics](../../protocol/scheduler-semantics.md), which already
owns the rules below piece by piece.

The canonical application executes one sequence of **L2 transactions**, in an
order every implementation must agree on. An L2 transaction is a direct input
or a user op. The scheduler builds the sequence from L1 inputs, one input at a
time:

1. **Every input first runs the backstop.** Directs queued for at least
   `MAX_WAIT_BLOCKS` execute, oldest first. This happens even when the input
   is a batch the gates then reject.
2. **A direct input**, from any sender but the sequencer, joins the fridge, the
   queue of directs not yet executed. *Next template:* only a well-formed one
   joins; the rest are dropped and never execute.
3. **A batch**, the sequencer's input, is accepted or changes nothing
   ([batch validity](#batch-validity)).
4. **Each frame of an accepted batch** first executes every queued direct up to
   its safe block, oldest first, then offers its user ops in order.
5. **A user op executes** when its signature recovers a sender, its max fee
   covers the frame fee, the application accepts it, and its nonce is not
   `u32::MAX`, which has no successor. Today the application accepts an op
   whose nonce is the sender's next and whose sender can pay the fee.

### What counts

A state's **L2 tx count** is how many L2 transactions it has executed. It
names the state, and it names the L2 transaction that state executes next, so
the feed numbers its rows with it. The code calls it `executed_input_count`,
though it counts transactions, not L1 inputs.

- **Every executed direct counts**, whatever the application does with it.
- **Every executed user op counts**, including a business failure: an op the
  application accepted whose effect then fails, such as a transfer larger than
  the sender's balance after the fee. It still advances the sender's nonce and
  pays the fee.
- **Nothing else counts**: not a queued direct, not a skipped user op, not a
  batch envelope, not anything in a rejected or stale batch.

On the cases that come up most:

| User op | Today |
|---|---|
| Wrong nonce, or a signature that recovers no sender | Skipped; no count, no nonce, no fee |
| Max fee below the frame fee | Skipped |
| Its sender cannot pay the frame fee | Skipped. The open authorization rule would include it instead, charging what the sender has ([template](template.md#open-rules)) |
| Its effect needs more than the sender has after the fee | A business failure: counts, advances the nonce, pays the fee |
| In a rejected or stale batch | Never offered; no count |

## Batch validity

Target: [scheduler semantics](../../protocol/scheduler-semantics.md#processing-an-input),
which owns gates a to f today.

A batch is accepted when all of these hold, checked in this order:

1. its payload decodes as a batch;
2. its batch nonce is the next one expected;
3. if it has frames, they are well formed: each frame's safe block is at or
   below the batch's inclusion block and at or above the previous frame's, and
   each fee price is in range;
4. it is not stale: its inclusion block is less than `MAX_WAIT_BLOCKS` past its
   first frame's safe block;
5. *next template, if the [floor gate](template.md#open-rules) is kept:* its
   first frame's safe block is at or above the floor block.

A batch with no frames is accepted and executes nothing. An accepted batch
advances the expected batch nonce by one; any other keeps it.

## Two counts

| Count | Names | Who knows it |
|---|---|---|
| **L2 tx count** | a state, and the next L2 transaction | every application state; a client knows only this one |
| **L1 input count** | a machine's position: how many InputBox inputs it has consumed | the machine; the watchdog's checkpoint names hold it |

They differ, because a batch envelope or a queued direct is an L1 input but not
an L2 transaction. The sequencer's database bridges them: every L2 transaction
it stores records the L1 input that executed it.

## Names

This plan names things the way the code would.

| Name | Meaning |
|---|---|
| base machine | the Cartesi machine image `setup --recovery` starts from |
| base count | the base machine's L2 tx count, where the new era's history starts |
| stop block | the safe L1 block setup pins after the flush; the rebuilt history ends with it |
| rebuilt history | the L2 transactions from the base count through the stop block, rebuilt from L1 |
| scheduler state | the next batch nonce, the fridge and, with the floor gate, the floor block |
| scheduled transaction | a transaction the scheduler hands the application, which may still reject a user op |
| executed transaction | a scheduled transaction the application executed; it has an L2 tx count |
| first queued input | the InputBox index of the oldest direct still queued at the end of the stop block |
