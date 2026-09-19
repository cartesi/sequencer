# Feed and replay integration (Track 3)

The application-history protocol is implemented. Its current contracts live in:

- [Application history and replay](../protocol/application-history.md): coordinates,
  storage/recovery boundaries, snapshot bootstrap, and consumer resume.
- [README API](../../README.md#api): routes, wire messages, refusal codes, and limits.
- [Snapshot lifecycle](../snapshots/lifecycle.md): durable artifacts, accepted
  comparisons, recovery exports, and leases.

## Remaining integration gates

1. Validate the native reference adapter's snapshot-to-live replica workflow;
   repeat against the private DEX bridge when available. Reference-engine
   conformance does not establish private-engine correctness.
2. Measure submit-to-matching-WS-event latency in the representative deployment,
   including checkpoint creation and L1 reconciliation under the supported load.

The [validation record](../review/2026-09-16-track3-validation.md) records the
wallet's nonempty cold bootstrap, concurrent replay/live delivery, recovery and
rebootstrap, canonical-machine gates, and local latency measurements. Those
results do not replace the consumer/environment gates above.

## Revisit only with a consumer need

- Resumable snapshot transfer: when artifact size makes interrupted downloads costly.
- Retained client checkpoints: when full rebootstrap cost matters.
- Archival HTTP replay or raw L1 feeds: for an identified consumer.
- Session fencing: if history can mutate within an admitted process or multiple
  local writers become supported.

These are triggers for design work, not promised APIs. The
[coordination plan](2026-07-coordination-tracks.md) owns cross-track priorities.
