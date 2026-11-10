# Recovery measurements — 2026-11-10

Scope: what `setup` and the canonical guest cost on the recovery path, for the
[recovery plan's measure items](../plans/2026-11-sequencer-recovery/todo.md#measure).
Four costs: setup's rebuild and execution of the history, reading a base
machine's export, the guest draining a full fridge in one input, and the devnet
recovery drills end to end.

Retained for the register's
[sequencer recovery follow-ups](register.md#sequencer-recovery-follow-ups) and
its [fridge bound](register.md#bound-the-canonical-direct-input-fridge) entry.
Measured on the uncommitted working tree over `f83b51f` that adds `setup
--base`, with the devnet image `examples/canonical-app/out/canonical-machine-image`
built from it. Replace the guest numbers when libdex's guest exists, since
every one of them is the reference wallet's; re-measure the rebuild if its
signature recovery changes; delete this record when no register entry cites it.

## Environment

- Apple M5 Max (18 cores in two performance levels, 6 and 12), 36 GiB, macOS
  26.6.2, APFS on the internal SSD.
- Rust 1.99.0 from the dev shell, release profile (the workspace sets no
  `[profile.release]`, so opt-level 3, no LTO). SQLite bundled with
  `libsqlite3-sys` 0.38.2, with the pragmas `Storage` sets in production.
- Signature recovery is alloy's `Signature::recover_address_from_prehash` on
  `k256` 0.13.4, the pure-Rust backend, as the scheduler runs it.
- Cartesi Machine emulator 0.22.0 (libcartesi from the dev shell), through the
  `cartesi-machine` Rust bindings the sequencer links.

The harnesses were throwaway code, not committed; the methods below say enough
to rebuild them.

## Summary

| What | Result | For an operator |
|---|---|---|
| Rebuild and execute a history | 12,000 to 12,300 rows/s and 718 to 733 inputs/s end to end, from 10^4 to 10^6 inputs, from genesis or a base | Recovery time is the number of user ops past the base times about 85 µs |
| What dominates the rebuild | Signature recovery: 78 µs of each op's 83 µs of rebuild (95%) | Only parallel recovery would shorten setup much |
| Read a base machine's export | 32 ms at the largest state that fits | Never the slow step |
| Largest `state` report that fits | 40,328 accounts that hold only a balance; 27,593 when every account also has a nonce | This is the limit that binds first |
| Cycle headroom of the 2^36 inspect cap | at least 3,269 times the cycles of the largest `state` report that fits | Comfortable |
| Drain 16,384 queued directs in one input | 1.94 G cycles, 2.1 s in the emulator, 145,000 times under the per-input budget | Comfortable |
| Devnet recovery drills | 36 to 41 s per round trip | Mostly the scenario's own waits, not setup |

## a. Setup's rebuild and execution

### Method

A temporary `#[ignore]` test in `sequencer::commands::setup`, run with
`cargo test -p sequencer --release --lib`, with the rebuild's test-only page
size raised to the production 1,024 rows for the run.

- **The input table.** A fresh setup database with the test deployment
  identity pinned. The inputs come in cycles of four: three ERC-20 deposits
  from the portal at block `b`, then a batch from the submitter at block
  `b + 1` with two frames at safe block `b`, each of 32 signed ops. The
  deposits fund 1,024 senders in turn. Each op is a wallet transfer of 1 to
  one of 100,000 recipients, at fee price 10 with `max_fee` 10. One op in 20
  repeats the op before it, so the executor skips it for its nonce. A cycle
  makes 67 rows, 3 deposits and 64 ops, and 3.2 ops per cycle are skipped.
- **The sync.** Excluded, because its time is the RPC provider's. Its database
  part was timed: the inputs were appended through
  `append_ingested_safe_inputs_with_timestamp`, the input reader's write, in
  chunks of 1,024 inputs.
- **Genesis.** The base is the genesis export and the genesis wallet.
- **A base in the middle.** The native `CanonicalRunner` runs the first
  `4 × (cycles / 2) + 3` inputs and its export and wallet become the base, so
  three deposits are queued in the base's fridge. Building that base is not
  timed.
- **The timed steps** are those of `rebuild_and_publish`, each timed alone:
  `check_export`, `count_submitter_inputs`, `rebuild_history`,
  `execute_history` with the wallet, and `fill::publish`. "Total" is their
  sum. The stop block is the last block.
- **The decomposition.** After setup, the same pages fed to a resumed
  `Scheduler` with nothing written ("scheduler only"); then 25,600 distinct ops
  hashed (`eip712_signing_hash`) and recovered one by one.

### Results

Each row is one run. A second 10^6 run, made to sample memory, gave
1,336.3 s of rebuild and 1,392.7 s in all (12,027 rows/s). Durations come from
Rust's `Instant`, which on macOS does not count system sleep; the host slept
during the long runs.

| Inputs in the table | Base | Inputs replayed | Rows (skipped) | Rebuild | Execute | Publish | Total | Inputs/s | Rows/s |
|---|---|---|---|---|---|---|---|---|---|
| 1,000 | genesis | 1,000 | 16,750 (800) | 1.36 s | 0.04 s | 0.02 s | 1.42 s | 706 | 11,828 |
| 10,000 | genesis | 10,000 | 167,500 (8,000) | 13.2 s | 0.42 s | 0.02 s | 13.7 s | 731 | 12,242 |
| 10,000 | input 5,003 | 4,997 | 83,750 (4,000) | 6.6 s | 0.21 s | 0.02 s | 6.8 s | 733 | 12,284 |
| 100,000 | genesis | 100,000 | 1,675,000 (80,000) | 132.1 s | 4.3 s | 0.08 s | 136.6 s | 732 | 12,264 |
| 100,000 | input 50,003 | 49,997 | 837,500 (40,000) | 65.9 s | 2.3 s | 0.05 s | 68.3 s | 732 | 12,263 |
| 250,000 | genesis | 250,000 | 4,187,500 (200,000) | 332.5 s | 11.0 s | 0.16 s | 343.8 s | 727 | 12,180 |
| 1,000,000 | genesis | 1,000,000 | 16,750,000 (800,000) | 1,329.4 s | 52.8 s | 1.74 s | 1,384.6 s | 722 | 12,097 |

`check_export` and `count_submitter_inputs` took at most 0.7 s together. The
10^6 runs and the 250,000 run, which sampled memory, skipped the
decomposition.

What else the runs showed:

- **Signature recovery dominates.** One recovery costs 78 µs (78.2 to
  80.7 µs across runs) and the EIP-712 hash 1.2 µs. At 10^5 inputs the 1.6
  million recoveries come to about 125 s of the 132 s rebuild. The scheduler
  alone over the same pages took 129.3 s; writing the rows took the other
  2.9 s (2%), and reading the input pages 0.07 s.
- **Execution is the application's cost.** The wallet ran 320,000 to 450,000
  rows/s, 3 to 4% of the total. Another application's execution must be
  measured with it.
- **A base in the middle halves the time** because it halves the inputs
  replayed: the rate is the same, so setup's time follows the work past the
  base, not the history's length.
- **Memory stays bounded.** Sampled every 2 to 5 s with `ps`, the test
  process stayed between 26 and 55 MB resident at 250,000 and at 10^6 inputs;
  it grew only during execution and publication, with the wallet. One reading
  is unexplained: `/usr/bin/time -l` around the first 10^6 run's whole
  `cargo test` command reported a 2.9 GB peak, against 141 MB at 10^4 and
  10^5; the sampling would miss a spike shorter than 5 s.
- **Disk.** The input table took 244 MB at 10^5 inputs and 2.4 GB at 10^6;
  after a genesis setup the database was 510 MB and 5.1 GB: the rebuilt
  history about doubles it. The sync's database writes took 0.47 s for 10^5
  inputs and 5.3 s for 10^6.
- **The wallet's state.** With 100,000 recipients the native wallet reached
  5.3 MB, more than a machine's `state` report can carry (b).

## b. Reading a base machine

### Method

A temporary example binary in the `sequencer` crate drove the devnet image
through the emulator bindings as `testsi` does: one `EvmAdvance` per input, the
root hash sent as the revert hash, then a run to the next manual yield. To
grow the wallet it queued deposits, each from a new owner at one block, then
sent one batch whose single empty frame drains them, in rounds of at most
16,384. It stored the machine at each size. For each stored machine it then:

- loaded it as `read_machine_export` does, sent the `scheduler` and then the
  `state` query, and read `mcycle` before and after each run to the manual
  yield, with the wall time of that run;
- called `sequencer::machine::read_machine_export` on it and timed the whole
  call: the load, both queries and the state file's write.

Only balances grow this way: a deposit's owner has no nonce entry.

### Results

The `scheduler` query took about 4,400 cycles and 0.1 ms at every size
(12,623 on the template). The `state` query:

| Accounts | `state` report | Cycles | Wall | Under the 2^36 cap | `read_machine_export` |
|---|---|---|---|---|---|
| 0 (template) | 76 B | 4,851 | under 0.1 ms | 14 million times | 2 ms |
| 1,024 | 53,324 B | 574,395 | 1.6 ms | 119,638 times | 3 ms |
| 4,096 | 213,068 B | 2,176,863 | 4.3 ms | 31,568 times | 5 ms |
| 16,384 | 852,044 B | 8,531,035 | 14.2 ms | 8,055 times | 15 ms |
| 24,576 | 1,278,028 B | 12,809,394 | 20.9 ms | 5,365 times | 21 ms |
| 32,768 | 1,704,012 B | 17,070,474 | 27.5 ms | 4,026 times | 27 ms |
| 40,000 | 2,080,076 B | 20,913,146 | 33.0 ms | 3,286 times | 32 ms |
| 40,328 | 2,097,132 B | 21,020,105 | 32.4 ms | 3,269 times | 32 ms |
| 40,329 | does not fit | 29,843,541, then halts | 54.5 ms | | 56 ms, refused |
| 65,536 | does not fit | 47,672,415, then halts | 86.0 ms | | 88 ms, refused |

Loading a stored machine took 3 ms.

What the table means:

- **The report limit is exactly 2 MiB of report bytes.** The wallet's report
  is `76 + 52 × balances + 24 × nonces` bytes; 40,328 balances make 2,097,132
  bytes and fit, and 40,329 make 2,097,184 and do not. When the report does
  not fit, the guest's `emit_report` fails, the guest panics and the machine
  halts; `read_machine_export` then refuses the base with "the machine did not
  answer the `state` query: the machine halted". Only the in-memory copy
  halts, since the loader maps the stored files copy-on-write.
- **The account count where it stops fitting** depends on how many accounts
  hold a nonce. Every account that has sent an op holds both entries, 76 bytes,
  so 27,593 such accounts fit.
- **The cycle cap is not what binds.** The query costs about 520 cycles per
  account, so the largest report that fits uses 1/3,269 of the 2^36 cap. The
  halting queries used under 30 million cycles at 40,329 accounts and under 48
  million at 65,536, both far below it.

## c. Draining a full fridge in one input

### Method

The same driver, from the template: N deposits from new owners queued at one
block, then one batch at the next block whose single frame, with no ops, names
that block as its safe block. That frame drains the whole fridge, as the first
frame after an outage and the era's root frame both do. The cycles and the wall
time are those of the batch's run to its manual yield. Each size refills from
the previous size's stored full fridge. Variants: 1,024-byte payloads (the cap)
instead of 72 bytes, and directs from a sender that is not the portal, which
the wallet counts but does not credit, so they emit no notice.

All sizes fill in seconds, so the template's bound of 16,384 and four times it
were measured directly; nothing is extrapolated.

### Results

| Queued directs | Cycles of the drain | Per direct | Wall | Notices |
|---|---|---|---|---|
| 1,024 | 123,952,092 | 121,047 | 0.14 s | 1,024 |
| 4,096 | 483,428,709 | 118,025 | 0.52 s | 4,096 |
| 16,384 (the bound) | 1,936,703,678 | 118,207 | 2.07 s | 16,384 |
| 65,536 (four times the bound, one block) | 7,798,563,505 | 118,997 | 8.37 s | 65,536 |
| 16,384, 1,024-byte payloads | 1,982,798,459 | 121,020 | 2.15 s | 16,384 |
| 16,384, not from the portal | 16,696,478 | 1,019 | 0.02 s | 0 |

- **The cost is linear,** about 118,000 cycles per drained deposit, and the
  emulator ran about 930 million cycles per second.
- **The deposit's execution and its notice dominate.** Draining directs that
  execute without crediting or emitting costs about 1,000 cycles each; payload
  size barely matters.
- **Queuing is cheap:** about 4,400 cycles per queued 72-byte direct and 6,000
  per 1,024-byte one, under 0.05 ms of emulation each.
- **The per-input budget** is 2^48 − 1 cycles from each delivered advance
  ([emulator facts](../cartesi-machine.md)). A drain at the bound uses
  1/145,000 of it, and four times the bound also completed: memory and the
  accumulated outputs held too.

## d. The devnet recovery drills

From the drills' run on the same tree (the devnet agent's report): the local
Anvil devnet, debug builds as the e2e harness runs them, each recovery scenario
run three times, the six interleaved, with
`target/debug/rollups-e2e <scenario> --exact`, and the divergence drill once,
every run passing. The times are each scenario's whole wall time.

| Scenario | Wall time |
|---|---|
| `recovery_from_genesis_round_trip_test` | 36 to 37 s |
| `c_host_recovery_from_genesis_round_trip_test` | 36 to 37 s |
| `recovery_from_stored_machine_round_trip_test` | 40 to 41 s |
| `c_host_recovery_from_stored_machine_round_trip_test` | 37 to 41 s |
| `setup_refuses_a_set_up_data_directory_test` | 0.5 to 0.8 s |
| `c_host_setup_refuses_a_set_up_data_directory_test` | about 0.5 s |
| `watchdog_divergence_drill_test` | about 23 s, one run |

The whole devnet suite, 53 scenarios, ran in 512 s.

A round trip runs an old instance and pumps traffic, stops it, queues directs,
runs setup, starts the new era, lands its first batch and checks two fresh
watchdogs. The drill does not time setup alone. Its history is a few dozen
blocks of devnet traffic, which by (a) rebuilds in a small fraction of the
time; the rest is the scenario's steps and its waits, bounded as follows: mining until a block is safe (at most 64 mines,
100 ms apart), waiting for finality to advance (at most 40 tries, 1 s apart),
and watchdog commands (at most 120 s each). The stored-machine round trips
also run the watchdog's `init` and `replay` to make their base machine.

## Conclusions

- **The `state` report's 2 MiB is the limit that binds.** It holds 27,593 to
  40,328 wallet accounts, and the synthetic history in (a) outgrew it within
  10,000 inputs. Setup refuses a base machine whose state passes it, so such a
  deployment can recover only from genesis or an older base; that is the
  register's
  [range-read follow-up](register.md#sequencer-recovery-follow-ups); this
  record gives that entry its exact numbers.
- **The inspect cycle cap is comfortable** at more than 3,000 times the cost
  of the largest report that fits. A range read would answer smaller queries,
  so its headroom must be measured when it is built.
- **The fridge's largest drain is comfortable for the wallet:** 2 s and
  1/145,000 of the per-input budget at the bound. libdex's per-direct cost is
  its own to measure, one of the questions for Bart.
- **Setup's time is about 85 µs per user op past the base, on one core.**
  That is about 42 million ops per hour on this machine. A base whose age holds
  30 million ops, a week at a sustained 50 ops/s, takes about 43 minutes to
  rebuild; genesis takes as long as the whole history. A recent base is what
  keeps a recovery short.
- **One optimization would matter**, if rebuild time ever does. The
  scheduler reads nothing from the application, so the ops of a page could be
  recovered on all cores before they are handed out in order; that would
  divide the recovery's 95% of the rebuild by up to the number of cores
  (unmeasured). Nothing else in setup takes more than about 3% of its time.

Limits of these numbers: one machine, macOS, an SSD; the synthetic mix is one
choice, so scale by rows and ops rather than inputs; the guest numbers are the
reference wallet's; the emulator ran in this process, not inside a node.
