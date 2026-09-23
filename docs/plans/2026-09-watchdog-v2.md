# Watchdog v2

**Status:** implemented (2026-09-22). The design lives with its owners; this
file keeps the remaining work.

| Topic | Owner |
|---|---|
| Runtime contract, state sources, commands, configuration, state directory, metrics, and why it is built this way | [Watchdog README](../watchdog/README.md) |
| Divergence playbook and drills | [Incident runbook](../watchdog/incident-runbook.md) |
| Emulator behavior and measurements | [Cartesi Machine facts](../cartesi-machine.md) |
| Comparison bytes an application must produce | [Application contract §6](../protocol/application-contract.md#6-checkpoint-lifecycle) |
| `GET /finalized_state/digest` | [README API](../../README.md#operator-snapshot-endpoints-internal-only) |

## Remaining work

- **CI on GitHub.** The workflow changes (emulator install for the `rust`
  job, native module build, `luacheck`, the test guest build in the
  `rollups-e2e` job) have not run on GitHub; their commands pass locally.
- **Staging drill.** Run the divergence drill once against staging with the
  operators who will be paged ([runbook](../watchdog/incident-runbook.md#drills)).
- **Measure at scale.** Per-input snapshot and per-tick clone costs on the
  deployment filesystem with a DEX-sized state, and the digest's peak memory
  (about twice the range). The two-level digest (SHA-256 over SHA-256 of fixed
  64 MiB chunks) is the fallback if memory binds.
- **Guest exit codes.** The canonical image lacks a musl `xhalt`, so a
  panicking canonical application halts with payload 0 and the watchdog reports
  exit code 0. The test guest's `xhalt` can be shared.
- **trolley exceptions.** Add an exception call to `sdk/guest/trolley`; the
  test guest raises its exception through `libcmt-sys` meanwhile.

## Open questions for the DEX integration

- Is `M` exactly the whole labeled NVRAM or drive, zero tail included, with the
  same length as the native comparison file?
- Which Cartesi Machine version does the DEX image pin?
- Does the guest ever reject an input? The watchdog handles it either way.
- How large is `M` expected to grow?
