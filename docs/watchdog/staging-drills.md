# Watchdog Staging Drills

Operator drills for webhook delivery and divergence detection. Local harness
steps live in [`README.md`](README.md); this document covers staging and manual
verification.

## Prerequisites

- Built canonical machine image: `just canonical-build-machine-image`
- `cartesi-machine`, `lua`, and `curl` on PATH
- `lua-cjson` (system package, or `just watchdog-lua-deps` copies/builds `.deps/lua/cjson.so` via `gcc` — no `make`)
- `lua-curl` optional — drills and compare harness fall back to `curl` CLI when absent
- Staging or local sequencer reachable at `WATCHDOG_SEQUENCER_URL`
- L1 RPC + InputBox + app addresses matching that deployment
- Webhook receiver URL (Slack incoming webhook, PagerDuty, or `https://httpbin.org/post` for smoke tests)

## Drill 1 — Webhook delivery (no sequencer)

Verifies the alarm transport reaches your receiver.

```bash
just watchdog-lua-deps
export WATCHDOG_WEBHOOK_URL="https://your-receiver.example/hook"
WATCHDOG_LUA_DEPS=.deps/lua lua watchdog/tests/drill_webhook.lua
# or: just test-watchdog-webhook-drill
```

Expected: HTTP 2xx for both `state_mismatch` and `safe_block_regressed` sample payloads.
Check the receiver shows JSON with `"kind"` and `"run_id"` fields.

## Drill 2 — Divergence webhook (synthetic mismatch, no CM)

Verifies the receiver gets a realistic `state_mismatch` payload without running compare mode:

```bash
export WATCHDOG_WEBHOOK_URL="https://your-receiver.example/hook"
WATCHDOG_LUA_DEPS=.deps/lua lua watchdog/tests/drill_divergence.lua
```

Expected: HTTP 2xx, receiver shows `kind=state_mismatch` and a non-zero `mismatch_offset`.

Unit coverage: `just test-watchdog` (`runner alarms on raw state mismatch`).

## Drill 3 — Happy compare (local Anvil harness)

Full stack: Anvil + devnet rollups + sequencer + CM inspect + `GET /get_state`.

```bash
just test-watchdog-compare-harness
# equivalent:
# just setup && just watchdog-lua-deps && just ensure-machine-image
# cargo build -p sequencer --bin sequencer-devnet -p rollups-e2e
# RUN_WATCHDOG_E2E=1 cargo run -p rollups-e2e -- watchdog_genesis_compare_test --exact
```

Or run the Lua compare pass manually after starting a devnet sequencer yourself:

```bash
export WATCHDOG_MODE=compare
export WATCHDOG_SEQUENCER_URL=http://127.0.0.1:<port>
export WATCHDOG_L1_RPC_URL=http://127.0.0.1:8545
export WATCHDOG_INPUTBOX_ADDRESS=<from Anvil deployments>
export WATCHDOG_APP_ADDRESS=<deployed app>
export WATCHDOG_CHECKPOINT_DIR=/tmp/watchdog-checkpoints
export WATCHDOG_CM_SNAPSHOT_DIR=examples/canonical-app/out/canonical-machine-image
export WATCHDOG_CM_SNAPSHOT_SAFE_BLOCK=0
export WATCHDOG_LUA_DEPS=.deps/lua
lua watchdog/tests/run_compare_once.lua
```

Expected: exit 0, stdout `watchdog compare ok: safe_block=... input_count=...`, and genesis wallet state `{"balances":{},"nonces":{}}` on both sides.

## Drill 4 — Production compare daemon

Run the watchdog in compare mode against staging (daemon or cron):

```bash
export WATCHDOG_MODE=compare
export WATCHDOG_ONCE=1          # or 0 for daemon
export WATCHDOG_WEBHOOK_URL=...
# ... all WATCHDOG_* vars from config.lua ...
lua watchdog/main.lua
```

On mismatch: non-zero exit, webhook fired, logs show `state_mismatch` and byte offset.

## Triage checklist

| Symptom | Likely cause |
|---------|----------------|
| `inspect endpoint not implemented` | Stale CM image — rebuild |
| `state_mismatch` at genesis | Checkpoint not aligned with sequencer history |
| Webhook 4xx | Wrong URL or auth on receiver |
| Compare skipped in Lua e2e | Set `WATCHDOG_E2E_SEQUENCER_URL` to a live sequencer |
| Compare harness skipped | Set `RUN_WATCHDOG_E2E=1` (see `just test-watchdog-compare-harness`) |
