# Watchdog Design Notes

The watchdog is an independent off-chain safety monitor. It advances a
canonical Cartesi Machine from L1 inputs, inspects the resulting SSZ snapshot,
and byte-compares it with the sequencer's `GET /finalized_state` response at
the same finalized `inclusion_block`.

Its value is independence: the sequencer serves state derived from its own
safe-acceptance simulation, while the watchdog re-derives the canonical
scheduler result from L1.

## Current Shape

The watchdog has one executable with two subcommands:

```bash
sequencer-watchdog init
sequencer-watchdog tick
```

`init` records the watchdog's canonical starting state. `tick` runs one compare
cycle and exits; infra schedules `tick` with a timer or CronJob. Runtime
non-overlap is enforced by `sequencer-watchdog` with a kernel `flock`, and
Kubernetes/systemd deployments should use their native non-overlap guard.

Each tick:

1. Loads the watchdog checkpoint selected by `head.json`.
2. Reads `GET /finalized_state/inclusion_block`.
3. Exits cheaply if the finalized block is unchanged.
4. Fetches L1 `InputAdded` logs for the open block range.
5. Advances the CM, inspects state, fetches `GET /finalized_state`, and compares
   raw SSZ bytes.
6. Writes a new checkpoint only after a successful compare.

There is no advance-only mode. Advancing the CM is just an implementation step
inside a compare cycle.

## Detection boundary: watchdog versus R2

The watchdog is the broad independent detector for application-state
divergence at a finalized checkpoint. It is not the sequencer's R2
accepted-batch wire-identity detector, and neither mechanism subsumes the
other.

R2 runs inside the input reader's atomic safe-input sync. For every
at/above-anchor landing the mirrored scheduler accepts, it requires a
byte-identical valid local sealed batch at that nonce. A foreign or mismatched
landing persists `canonical_divergence` and structurally freezes the accepted
frontier and finalized-snapshot promotion. The offending landing therefore
normally never produces a newer `/finalized_state/inclusion_block` for the
watchdog to compare. Under the unchanged-head optimization above, a watchdog
tick legitimately exits idle. Distinct wire bytes can also be application-state
equivalent, which a byte comparison of resulting snapshots would not expose.

Conversely, R2 shares the sequencer's off-chain acceptance predicate and does
not independently replay application execution. The watchdog can catch
direct-input, user-op, scheduler, or application-state divergence outside R2's
narrow predicate once a comparable finalized checkpoint is published.
`DangerDetector`, not the watchdog, owns prompt process-wide reaction to the
durable R2 marker; the inclusion lane also refuses the poisoned projection
opportunistically if its existing frontier read wins first.

## Watchdog State

The watchdog state is canonical from the watchdog's point of view. The
sequencer is what gets verified.

`init` stores the operator-provided bootstrap CM snapshot into the normal
checkpoint layout. `tick` never bootstraps from env; missing `head.json` is an
operator error.

`config.json` stores stable deployment identity (`sequencer_url`,
`input_box_address`, `app_address`, retry knobs). `CARTESI_WATCHDOG_BLOCKCHAIN_HTTP_ENDPOINT` is read
at tick time rather than persisted, because provider URLs and credentials are
operational inputs that may rotate.

Tradeoff accepted: if the watchdog is initialized while the sequencer is already
serving an incorrect finalized state at the exact same block, and the block does
not advance before the next tick, the unchanged-block skip can delay detection
until a future finalized block. This keeps the runtime model simple.

Unreadable, malformed, or incomplete watchdog state means stop and let the
operator repair the state directory.

## Checkpoint Crash Model

Checkpoint writes use a pointer-swap model:

1. Store the CM snapshot under `checkpoints/<safe_block>/snapshot`.
2. Write `manifest.json` next to it.
3. Write `head.json.tmp`.
4. Rename `head.json.tmp` over `head.json`.
5. Best-effort delete the superseded checkpoint.

Crash before the rename leaves the previous checkpoint selected. Crash after
the rename leaves the new checkpoint selected and may leave an old directory to
clean up later. The code checks write and close errors for the JSON files, but
it does not currently fsync files or directories; if production needs
power-loss-grade durability, use a small SQLite state store or add explicit
fsync support.

## State Layout

```text
state/
  config.json
  head.json
  status.prom    # last tick metrics (Prometheus textfile)
  run.lock       # advisory lock handle in the production container
  checkpoints/
    00000000000000000042/
      manifest.json
      snapshot/
```

`config.json` is written by `init` and read by every `tick`. `head.json` is the
small mutable pointer. Checkpoints are block-named directories; after a
successful pointer flip, the superseded checkpoint is pruned best-effort.

## Memory Notes

The L1 fetch path consumes successful provider partitions immediately:

- JSON-RPC still reads one `eth_getLogs` response body at a time;
- each successful partition response is sorted locally;
- logs are decoded into an input chunk;
- the runner advances the CM for that partition and then discards the chunk.

This preserves the operational assumption that one provider response fits in
memory, while avoiding whole-range `logs` plus whole-range decoded `inputs`.
The Cartesi binding may still queue one partition internally while feeding it to
the machine.

## Open Questions Before Merge

- Is the current crash model sufficient for a watchdog sidecar, or do operators
  need fsync/SQLite durability?
- If provider responses themselves become too large, add provider pagination or
  a smaller fixed scan window in a separate change.
