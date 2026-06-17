# Watchdog (Lua)

Off-chain sidecar that compares the sequencer's finalized SSZ snapshot to state from the canonical Cartesi Machine.

**Documentation:** [`docs/watchdog/operator-deployment.md`](../docs/watchdog/operator-deployment.md) (Sepolia / mainnet) · [`docs/watchdog/getting-started.md`](../docs/watchdog/getting-started.md) (local devnet) · [`docs/watchdog/README.md`](../docs/watchdog/README.md) (architecture)

```bash
just doctor                        # lua + cartesi + lcurl + machine_cartesi load probe
just watchdog-lua-deps             # .deps/lua/lcurl.so (needs libcurl + liblua5.4-dev)
just test-watchdog                 # unit tests (mocked HTTP; no lcurl required)
just devnet-for-watchdog           # local Anvil + sequencer-devnet (prints WATCHDOG_* env)
```

Watchdog-local recipes also live in [`justfile`](justfile) (`just -f watchdog/justfile <recipe>`).

Host packages and build errors: [`docs/watchdog/README.md`](../docs/watchdog/README.md#host-dependencies-watchdog-lua-deps).
