# Recovering the sequencer: what remains

The procedure is built. [Cockroach recovery](../../recovery/cockroach.md) is
its home: one setup from any base, its inputs and outputs, what setup does,
the operator's and the client's playbooks, and what can go wrong. This page
keeps only the operational work that is not built yet.

- **Finding a trusted machine.** Setup takes a machine; finding one at or
  below a given L2 tx count, checking it, and backing machines up is the
  operator's job, with no tooling yet
  ([machine sourcing](todo.md#machine-sourcing)).
- **Drills that are not tests yet.** No test clears a watchdog latch around a
  rebuild, or realigns a client other than the devnet's toy wallet
  ([drills](../../watchdog/incident-runbook.md#drills)).
- **Bart's client.** Its backups and its realignment at a count it trusts are
  his to build ([client backups](client-backups.md),
  [questions for Bart](todo.md#questions-for-bart)).
- **Refusing a client past the rebuilt history end.** The feed checks a claim
  only against the era's start and its head, so the check is the client's;
  whether the sequencer refuses such a client itself is part of the
  [egress redesign](todo.md#redesign-the-egress-api).
- **Recovery time on a real deployment.** The rebuild's time follows the user
  ops past the base ([how long a recovery takes](../../recovery/cockroach.md#how-long-a-recovery-takes));
  the L1 sync through a deployment's provider and libdex's execution are not
  measured yet ([measure](todo.md#measure)).
