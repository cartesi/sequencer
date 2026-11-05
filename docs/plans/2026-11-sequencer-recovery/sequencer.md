# Inside the sequencer

**Status:** proposed; nothing here is implemented. What the sequencer builds
for this plan. [Operations](operations.md) says how it is used; outlines in
Rust show roles, and their names are proposals.

## Loading a machine

Setup links the emulator to load a base machine and run its
[export queries](application.md#the-state-export). Today only
`sdk/guest/testsi` links it, through the `cartesi-machine` crate, so the
sequencer's build and image grow.

## The rebuild

Setup runs the same two pieces the canonical guest runs, the scheduler and then
the application, but one after the other over the whole rebuilt range instead
of input by input. The scheduler needs no application: what it hands out
depends only on its state and the inputs
([scheduler](scheduler.md#the-schedulers-state)). Its interface is pending
([scheduler](scheduler.md#the-schedulers-interface)). What it hands out, in
order, is one of:

- **a direct**, with the batch nonce of the frame that drained it, or none when
  the backstop or an overflow drained it;
- **a user op**, with its sender, nonce, fee, data, safe block and batch nonce.

Today's `Scheduler<A>` executes as it schedules, so it splits into the
scheduler and the executor, which runs scheduled transactions through the
shared execution boundary. The guest, the tests and setup use the pair, and
today's `fold_replay` and its terminal drain go.

## The rebuilt history table

One row per scheduled transaction, in order:

| Column | Holds |
|---|---|
| `l1_input_index` | The input whose processing scheduled it |
| `position` | Its place among that input's transactions |
| The transaction | A direct's input index and drain batch nonce, or a user op's sender, nonce, fee, data, safe block and batch nonce |
| `l2_tx_count` | Its count once executed; empty for a skipped user op |

Rows come from safe L1 blocks, so they never change: no standard recovery cuts
below the stop block.

## The era and the feed

The new era records:

| Field | Value |
|---|---|
| Base count | The base's L2 tx count; `/history` reports it as `available_from` |
| Rebuilt history end | The L2 tx count at the end of the stop block |
| `base_safe_block` | The stop block, as today; acceptance scans only blocks after it |
| First queued input | Where the lane starts draining directs; an era without one drains from the first input after `base_safe_block`, as today |
| Anchor batch nonce | The next batch nonce at the stop block, as today |

The feed serves the executed rows from the base count: from the rebuilt history
table up to its end, then from the live table. A direct row takes its timestamp
and transaction hash from the input table, by input index. Its `batch_nonce`
becomes optional, since a direct the backstop or an overflow drained belongs to
no batch.

## The lane and the predicate

The next template's rules reach the sequencer's live side too:

- **The lane** drops the directs the direct filter refuses, drains from the
  first queued input, and refuses an underfunded op at `POST /tx`.
- **Frame ordering** ([I3](../../invariants.md#i3-frame-safe_blocks-are-non-decreasing-along-the-spine))
  becomes consensus-critical: a frame below the scheduler clock gets its batch
  rejected.
- **`scheduler_accepts`**, which skips gate d today, must check gate f or
  cover it in the same self-trust exemption.
