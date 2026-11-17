# Running the sequencer

This guide owns how to invoke, configure, supervise, and upgrade the sequencer.
[`commands/config.rs`](../sequencer/src/commands/config.rs) owns every variable
name, default, and validation; each subcommand's `--help` shows them. The
[watchdog deployment guide](watchdog/operator-deployment.md) deploys its
independent checker on a live chain.

## Subcommands

The sequencer runs in two phases. **`setup`** pins the deployment identity
(including the reviewed fee-oracle source), does the initial L1 sync, and
registers the genesis snapshot — run it once. Plain `setup` is L1-read-only: it
takes the batch-submitter *address*, never the signing key. **`run`** boots the
sequencer from the set-up DB, reading identity from it (so chain id and app
address are not `run` arguments); it holds the signing key because it submits.

A third subcommand, **`flush-mempool`**, settles the batch-submitter wallet
nonce on demand (keyed operator tool). It is flush-only: it requires a
completed setup and no canonical divergence, and it never performs
Sync/Cascade or launches runtime workers.

For rebuilding from a trusted checkpoint, follow the
[cockroach recovery procedure](recovery/cockroach.md#run-a-rebuild).
`setup --recovery` also needs the submitter key because it flushes transactions.

The release binary, `wallet-sequencer`, composes the sequencer with the toy
wallet configured for Sepolia. From a checkout, `cargo run -p wallet-sequencer
--release --` stands in for it. Its batch submitter must be
[`SEPOLIA_SEQUENCER_ADDRESS`](../examples/app-core/src/application/wallet.rs),
the only sender the Sepolia guest accepts batches from ([below](#configuration)).

```bash
# Phase A — set up the data dir (run once; idempotent).
CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT=https://<sepolia-rpc> \
CARTESI_SEQUENCER_BLOCKCHAIN_ID=11155111 \
CARTESI_SEQUENCER_APP_ADDRESS=<app-address> \
CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS=<SEPOLIA_SEQUENCER_ADDRESS> \
wallet-sequencer setup

# Phase B — run the sequencer.
CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT=https://<sepolia-rpc> \
CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE=<submitter-key-file> \
wallet-sequencer run
```

`setup` exits 20 (retryable) until the application's deployment block is at or
below the L1 safe head, so no reorg can move the block it pins. On Ethereum that
is a one-time wait of an epoch or two after deployment; on Anvil it is
`--slots-in-an-epoch` mined blocks (one in the repo's devnet stack, which sets
it to 1; Anvil's default is 32).

## Configuration

`setup` requires `CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`,
`CARTESI_SEQUENCER_BLOCKCHAIN_ID`, `CARTESI_SEQUENCER_APP_ADDRESS`, and
`CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS`. On chains other than Ethereum
mainnet and Sepolia it also requires a fee-oracle source (listed below).
The submitter address must equal the `sequencer_address` compiled into the
canonical machine; nothing checks this at runtime
([scheduler semantics](protocol/scheduler-semantics.md#the-one-stream-the-one-classification)).

**Use a dedicated submitter address.** Plain `setup` refuses an unsettled wallet
nonce, so sharing a busy contract-deployer address can trip the detection gate
while its deployment transactions are not yet safe. The devnet uses Anvil
account 9 (`DEVNET_SEQUENCER_ADDRESS`), separate from the account-0 deployer.
The Sepolia reference reserves [`SEPOLIA_SEQUENCER_ADDRESS`](../examples/app-core/src/application/wallet.rs)
for this deployment's batch submissions and application fee revenue; do not share
its signing key with other app or node instances. Changing that compiled address
requires a rebuilt canonical image and a matching new application deployment,
not just a runtime signing-key change.

`run` and `flush-mempool` require `CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT`
and the signing key, `CARTESI_SEQUENCER_AUTH_PRIVATE_KEY` or
`CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE` (whose first line is the key); both
refuse to start until `setup` has completed.

Optional variables, by subcommand:

- Every subcommand: `CARTESI_SEQUENCER_DATA_DIR` (the SQLite file is
  `sequencer.db` inside; `setup` creates it, and `run` and `flush-mempool`
  refuse a missing one), `CARTESI_SEQUENCER_SECONDS_PER_BLOCK`, and
  `CARTESI_SEQUENCER_ALLOW_INSECURE_RPC` ([below](#the-l1-endpoint)).
- `setup` and `run`: `CARTESI_SEQUENCER_PREEMPTIVE_MARGIN_BLOCKS`,
  `CARTESI_SEQUENCER_L1_READ_STALE_AFTER_BLOCKS`.
- `setup`: the fee-oracle source it pins, either
  `CARTESI_SEQUENCER_FEE_ORACLE_FIXED_LOG_GAS_PRICE` or a Uniswap source (the
  overrides `CARTESI_SEQUENCER_FEE_TOKEN_ADDRESS`, `CARTESI_SEQUENCER_WETH_ADDRESS`
  and `CARTESI_SEQUENCER_UNISWAP_V3_POOL`, given together and required on other
  chains, and `CARTESI_SEQUENCER_FEE_ORACLE_TWAP_WINDOW_SECS`);
  `CARTESI_SEQUENCER_CHECKPOINT_BLOCK`; and, for a
  [rebuild](recovery/cockroach.md), `CARTESI_SEQUENCER_RECOVERY`,
  `CARTESI_SEQUENCER_CHECKPOINT_DUMP_DIR` and the signing key.
- `run`: `CARTESI_SEQUENCER_HTTP_ADDR` (public listener),
  `CARTESI_SEQUENCER_INTERNAL_HTTP_ADDR` (internal listener),
  `CARTESI_SEQUENCER_MAX_BATCH_OPEN_SECONDS`,
  `CARTESI_SEQUENCER_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS`,
  `CARTESI_SEQUENCER_BATCH_SUBMITTER_CONFIRMATION_DEPTH` and
  `CARTESI_SEQUENCER_FEE_ORACLE_POLL_INTERVAL_MS`.

The [EIP-712 domain](protocol/scheduler-semantics.md#user-op-signatures) is
fixed except `chainId` and `verifyingContract`, which come from
`CARTESI_SEQUENCER_BLOCKCHAIN_ID` and `CARTESI_SEQUENCER_APP_ADDRESS`.

## The L1 endpoint

By default the blockchain endpoint must be `https://` unless its host is
loopback (`localhost`, `127.0.0.0/8`, `::1`) — a guard against accidentally
sending L1 traffic to a public RPC in the clear. Set
`CARTESI_SEQUENCER_ALLOW_INSECURE_RPC=true` (or `--allow-insecure-rpc`) to
permit plaintext `http://` to a non-loopback host on a **trusted private
network** — e.g. a Docker Compose / Kubernetes service name
(`http://anvil:8545`), `host.docker.internal`, or a private-VPC IP.

The flag is **per-invocation, not pinned into the DB**: set it on every
subcommand that dials L1 (`setup`, `run`, and `flush-mempool`). Setting it only
on `setup` and then omitting it on `run` is refused at boot with
`remote RPC must use https` — that is by design (each keyed/read path
re-validates the endpoint), not a bug. In a container deployment, put it in the
shared environment for all sequencer commands. Example (Docker Compose):

```yaml
environment:
  CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT: "http://anvil:8545"
  CARTESI_SEQUENCER_ALLOW_INSECURE_RPC: "true"
```

During a provider outage each worker retries on its next tick, and the inclusion
lane and API keep operating locally. The RPC client resends a rate-limited or
temporarily unavailable request at most twice, a second apart or after the
server's requested wait when that is 5 s or less.

The client follows no redirects: an endpoint that answers 3xx fails with that
status, so configure the provider's final URL (`setup` exits 30 at its chain-id
check, since a retry gets the same redirect).

Logs and errors show only the endpoint's scheme, host and port, because its
path, query and userinfo can hold an API key or a password. An L1 error's text
stays on one line, with each cause cut at 256 bytes. Redaction has three gaps:

- The RPC server can echo the secret inside a JSON-RPC error message or a
  malformed result, which alloy builds above the redaction layer.
- An error body can quote only part of the URL, or a different URL that carries
  the secret, such as a redirect target on another path.
- Debug-level logging for the `alloy_transport_http` target records the full
  URL, so keep that target at `info` where logs leave the host.

## Exit codes

Process exit codes follow the orchestrator exit-code contract; the constants
live in [`commands/error.rs`](../sequencer/src/commands/error.rs).

| Code | Meaning |
|---|---|
| `0` | Clean shutdown. |
| `10` | Restart: expect a recovery boot. |
| `20` | Transient refusal: retry with backoff. |
| `30` | Terminal command refusal, which needs an operator: setup not complete, identity mismatch, canonical divergence, or a persistent storage or application invariant failure. |
| `40` | A previous instance left work past the checkpoint: wipe the data directory and run `setup --recovery`. |
| `1` | Unclassified operational failure. |
| `2` | Invalid command line, reported by the argument parser. |
| `101` | A panic before the command harness starts. |
| `134` (`SIGABRT`) | Diagnosed terminal runtime fault, such as a worker panic or an undecodable batch landed by the submitter key. |

A diagnosed terminal runtime fault calls `abort()` immediately, without worker
drain or database settlement; supervisors must treat `SIGABRT` as terminal
class. Ordinary shutdown, expected recovery, and transient errors still drain
gracefully. Startup panics caught by the command harness are projected to `30`;
panics after runtime scope creation abort.

## Supervision

The exit codes are the sequencer's whole restart contract: there is no database
gate and no acknowledgement command. Standard recovery is automatic on every
boot, and a persistent terminal fault re-detects fail-loud when the faulty state
is next read, so the supervisor's handling of 30 and SIGABRT (stop and page, do
not auto-restart) is what bounds a crash loop.

The final `sequencer exiting` line logs at ERROR for exits 30 and 1, and at
WARN for 10, 20 and 40, with `verdict` and `supervisor_action` fields; the
SIGABRT line says so. Alert on the exit status, not the log level.

- **Supervisor recipes.** systemd can act on the code directly:
  `RestartPreventExitStatus=30 ABRT` (SIGABRT/134 is terminal-class too).
  Kubernetes Deployments restart regardless of exit code, and there is no
  boot gate — so on k8s a terminal exit will restart-loop through
  re-detection windows, serving traffic in between. There, the crash-loop
  bound is your alerting, not the restart policy: page immediately on
  `lastState.terminated.exitCode == 30` (and on signal exits / 134).
- **A fresh deployment needs a setup retry budget.** `setup` waits for the
  [deployment block](#subcommands), about one to two epochs on Ethereum
  (6.4–12.8 min); give a setup Job or unit a longer retry budget (a Kubernetes
  Job's default backoffLimit 6 allows about 10.5 min).
- **After an unclean death (OOM, node reboot, SIGKILL) no action is
  needed**: the next start re-derives everything from facts. For
  postmortems, the `terminal_faults` table records the cause of every
  command that returned through its bracket with a terminal verdict
  (best-effort, append-only, traveling with the data directory —
  `SELECT * FROM terminal_faults ORDER BY fault_id DESC`; each `run` boot
  logs the latest row at WARN unless a later completed setup resolved it). Any death that did not return
  through the bracket — SIGKILL, OOM, a node reboot, a terminal runtime abort,
  a controller panic — leaves only the process logs.
- **Untrustworthy local state requires manual recovery.** The reader's
  content-identity divergence marker freezes the acceptance frontier and
  blocks admission ([I15](invariants.md#i15-divergence-marker-present--acceptance-frontier-frozen)).
  A watchdog state mismatch independently signals application-state
  disagreement ([incident runbook](watchdog/incident-runbook.md)). Diagnose the cause
  and follow the [incident playbook](recovery/cockroach.md#incident-playbook)
  for a fresh-directory rebuild when needed; standard recovery does not repair
  bugs.

## Upgrades

The schema is one baseline migration, rewritten in place when it changes
([migration policy](../AGENTS.md#migration-policy)). `sequencer.db` records only
that the baseline ran, not which version of it, so a binary cannot detect a
database written by a release with a different schema. Release notes say when
a release changes the schema.

Upgrade across such a release by rebuilding with
[cockroach recovery](recovery/cockroach.md):

1. While the old sequencer still runs, download its accepted checkpoint from
   `GET /finalized_snapshot` and validate it as a
   [recovery artifact](recovery/cockroach.md#obtaining-the-recovery-artifact).
2. Stop the old sequencer.
3. Run the new release's `setup --recovery` into a fresh data directory, then
   `run`. Never point the new binary at the old data directory.

The new release must load the old release's export: its `info.toml` and
`checkpoint.toml` format and the application's state format. When it cannot,
prepare the bundle with the application's canonical export instead. The rebuild
starts a new era, so subscribers rebootstrap, and it carries over only what
landed on L1 by its stopping block.
