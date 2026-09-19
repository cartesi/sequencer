# Stack review closeout validation

Evidence for reviewing and landing the closing change on
`codex/stack-review-fixes`, above reviewed tip
`62ec150f18a27697220158f1ca4d84ec4a6fee06`. The implementation and regression tests
are at `18e99e4fe2d03426954f72df5641196d12103e99`; the closing documentation commit
adds contract corrections, this record, and a storage rustdoc correction only.
Retire this record after stack landing when no ongoing review decision uses it.

## Environment and results

Run on macOS arm64 through the project's parent Nix/direnv shell. Cargo and
rustc were both 1.95.0, Anvil was 1.5.1, and the locally rebuilt canonical guest
used the repository-pinned Cartesi Machine 0.20.0. Local Anvil differs from CI's
1.4.3 pin; these are local results, not a new CI run.

| Check | Result |
|---|---|
| `cargo check --locked --workspace --all-targets` | Passed |
| `cargo fmt --all -- --check` and `git diff --check` | Passed |
| `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | Passed |
| `cargo test --locked --workspace --exclude canonical-test -- --test-threads=1` | 754 passed; one existing ignored doc test |
| `lua watchdog/tests/run.lua` | 62 passed |
| `just canonical build-machine-image`, then `cargo run --locked -p canonical-test` | Fresh image; 10 guest scheduler tests passed |
| `bash scripts/ci-c-application-smoke.sh` | External archive, generic host, and independent downstream consumer built and ran their CLI smoke checks |

The process binaries were rebuilt after the final SDK change. Each of these
scenario filters passed for both the Rust wallet host and the C wallet host:

- `cold_replica_snapshot_backlog_live_recovery_test`
- `restart_and_replay_test`
- `recovery_after_stale_batches_test`
- `setup_recovery_round_trip_test`

These eight process runs include canonical watchdog comparison after recovery
and rebuild. Their prerequisites were the checksum-verified rollups-contracts
Anvil fixture, locally built test contracts, and watchdog Lua dependencies.
The first attempted process run lacked the fixture and stopped before startup;
setup supplied it before the successful runs.

## Discriminating regressions and independent review

- A matching local batch followed by a foreign accepted batch in the same block
  returned HTTP 200 before the finalized-selection guard. The canonical scheduler
  advances beyond that local snapshot. With the guard, all three finalized routes
  return 503, including a conditional state request, without acquiring a lease.
- With checkpoint clock 900, checkpoint block 901, and stop 899, the unguarded
  deterministic rebuild phase reached artifact creation. The new check refuses
  with exit 20 before replay or publication. A later resync reaching the checkpoint
  does not substitute for the fixed stop; equality and genesis still succeed.
- Without the SDK header timeout, the stalled-header regression exceeded its
  outer two-second limit. With the configured header deadline restored, all
  13 SDK tests passed, including a body that outlives that deadline.

Separate reviewers checked finalized selection and streamed I/O classification,
and the manual-recovery guard and exit classification. No blocking findings
remained. Persistent SQLite errors still reach terminal classification through
the new error wrapper; operational filesystem errors remain nonterminal.

## Limits and landing work

A repeated parallel host run reproduced the already registered
`dropped_runtime_scope_keeps_lock_until_detached_worker_stops` failure: final
reacquisition returned `Locked`. Its isolated rerun passed, and the complete
serial suite passed. This does not resolve the concurrency investigation; it
remains in the [review register](register.md#bounded-investigations-and-cleanup).

The full 49-scenario rollups suite and Sepolia-image scenarios were not rerun.
Private-engine conformance/export and representative deployment capacity remain
in their existing integration plans. The two current recovery TLA+ models were
read but not changed or rerun; neither models these new boundary checks.

No lower stack branch was rewritten. Reconcile the known C-host test ancestry
once at landing, retaining the evolved coverage at the reviewed tip and the
new independent post-recovery assertions. No push, PR creation, or merge was
performed during this implementation.
