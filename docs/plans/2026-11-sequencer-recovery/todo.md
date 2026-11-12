# To do

What this plan still needs. The scheduler, the shared executor, the
`Application` trait, the state export, `setup [--base] [--dry-run]`, the
rebuilt history and the feed's realignment are code in this tree.
[Cockroach recovery](../../recovery/cockroach.md) states the procedure and the
playbooks, and the
[history contract](../../protocol/application-history.md#era-baseline) the
era, the rebuilt history and the adopt rule. The
[register](../../review/register.md#sequencer-recovery-follow-ups) holds the
limits this work accepts and the paths it left unverified.

## Promote to real docs

- **[scheduler.md](scheduler.md)**: [scheduler semantics](../../protocol/scheduler-semantics.md)
  states the rules the reference runs, so this page shrinks to its open items,
  as [operations](operations.md) and [sequencer](sequencer.md) have.
- **[application.md](application.md)**: the
  [application contract](../../protocol/application-contract.md) and the
  [C binding](../../protocol/c-application-binding.md) state the trait, the
  export and the projection; this page shrinks to the guest's open duties and
  the questions below.
- **[interfaces.md](interfaces.md)** is code, except the outline of the
  scheduler's C header, which stays until a foreign scheduler links through
  it ([build](#build)).

## Build

- **A foreign scheduler in the rebuild.** An application's author may bring
  their own scheduler: one library, compiled into their guest, and natively
  behind [`scheduler.h`](../../../bindings/c-scheduler/include/scheduler.h)
  for setup's rebuild, as libdex will. The reference scheduler is then the
  specification's implementation and the wallet's scheduler, and its export
  is the header's reference. Setup calls the Rust `Scheduler` directly today
  ([`rebuild.rs`](../../../sequencer/src/commands/setup/rebuild.rs)); it needs
  a `Scheduler` trait and an adapter over the header, linked as an engine's
  archive is ([C engine](../../../bindings/c-app-engine/README.md)). The lane
  still predicts the canonical scheduler in Rust, so a foreign scheduler is
  held to the [conformance vectors](../../protocol/scheduler-semantics.md#conformance)
  through `c-scheduler`'s `tests/conformance.rs`, which today links only the
  reference export; package it so an author links their own archive in one
  command.
- **A base machine's state through the range source.** Setup reads only the
  `state` inspect report, which must fit 2 MiB, and libdex's state is a 512 MiB
  range, so setup cannot start from a libdex machine. Add the labeled-range
  read the watchdog uses, and measure it against the loader's cycle cap
  ([register](../../review/register.md#sequencer-recovery-follow-ups)).
- **Close a batch on its real size.** The lane closes a batch on its op count
  times the largest op the application accepts. libdex's largest op is 6,971
  bytes and its usual op under 50, so its batches would close at 2 ops where
  about 110 fit ([issue 58](https://github.com/cartesi/sequencer/issues/58),
  [libdex's sizes](../../review/2026-11-12-libdex-answers.md#ops-and-their-sizes)).
  Count the encoded bytes instead: it is the lane's own choice and changes no
  image. The fee still charges every op as 126 bytes; a per-byte price changes
  what a signed op means, and waits.
- **Drills that are not tests yet.** The round trips from genesis and from a
  stored machine run on the devnet for both hosts
  ([drills](../../watchdog/incident-runbook.md#drills)). No test clears a
  watchdog latch around a rebuild, or realigns a reader other than the toy
  wallet. A path run once in a deployment's life rots.

## Redesign the egress API

Later, in light of this plan:

- keep the WS subscription;
- consider a paginated HTTP endpoint for history;
- remove the methods nothing uses, starting with what the
  [tracks board](../2026-07-coordination-tracks.md#track-3--feed--replay-protocol-redesign)
  lists to reshape or retire, `/history`'s baseline and accepted checkpoint;
- the feed's identity check (the era), `available_from` and the
  rebuilt range, and whether the sequencer refuses a client from an older era
  whose count lies past the rebuilt history end, which today only the client's
  own adopt check catches;
- `/nonce` after a recovery, which reads only the live rows;
- the L2 tx count's older name, `executed_input_count`, which the HTTP fields
  and the `X-Executed-Input-Count` header still carry (the storage columns say
  `l2_tx_count`);
- the WS headers.

## Later

- **A metrics endpoint.** The sequencer exports no metrics: an operator reads
  logs and `/healthz`. Design one on the internal listener, beside the other
  internal routes, starting from the two numbers `/healthz` reports
  ([what the sequencer does](../../recovery/README.md#what-the-sequencer-does)).
- **A batch proxy, to rotate the submitter's key.** The canonical machine
  compiles in the address that posts batches, so the key behind that address
  can never change. A proxy contract would stand at it: the proxy forwards a
  batch to the InputBox from one configured hot wallet, the sequencer's, and a
  cold multisig can replace that wallet. The machine then pins the proxy,
  which never changes. A rotation may still need a cockroach recovery; that is
  to work out.

## Machine sourcing

A separate task. Setup only takes a machine; finding a trusted one is the
operator's job.

- **From a validator node at a settled epoch**, so its root can be checked on
  chain. Review our sling node: does it keep the machines at the end of past
  epochs, or only the newest?
- **A playbook** for finding a machine at or below a given L2 tx count, and for
  checking it.
- **A backup procedure for machines.** Storing directory trees for a long time
  is a solved problem; pick a mature tool rather than building one.

## libdex

[libdex as its author describes it](../../review/2026-11-12-libdex-answers.md)
records the answers to the first round of questions. They add two items to
[build](#build), the state's range read and closing a batch on its real size.
They also open two canonical rules in the
[register](../../review/register.md#canonical-rules-open-for-the-c-port): what
the scheduler does with an over-long deposit, and where the nonce lives.

## Questions for Bart

What an op pays is the application's choice, with charging what the sender
has as the recommendation, and admission's refusal by policy answers a
redundant price push; neither is a question.

- When a deleted session wallet is registered again, where does its nonce
  start? If it can start below where it stopped, what keeps the wallet's
  earlier ops, which are public on L1, from running again?
- What does `validate_user_op` check besides the nonce? Each of those checks
  becomes part of execution, which always runs and always counts.
- Does a client ever send several ops from one wallet without waiting for
  each answer? Does anything need a wallet's ops to run with no gap between
  them?
- Does the native engine lay out the state range byte for byte as the guest
  does? On which hosts has that been run?
- When there are numbers: the machine cycles of an idle tick and of the worst
  one.

## Measure

The [recovery measurements](../../review/2026-11-10-recovery-measurements.md)
give setup's rebuild rate, the cost of reading a base machine, the wallet's
largest drain and the drills' times. What they leave out:

- **The L1 sync** through a deployment's RPC provider, the one part of
  setup's time the provider bounds.
- **libdex's numbers**: its execution rate in setup, its cost per drained
  direct, and its `state` size, since every guest number there is the
  wallet's ([questions](#questions-for-bart)).
- **Production-like hardware.** One Apple machine on macOS ran every measurement;
  re-measure the rebuild on the recovery host's Linux storage.
