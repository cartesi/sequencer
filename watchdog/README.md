# Watchdog (Lua)

Off-chain sidecar that compares the sequencer's finalized SSZ snapshot to state from the canonical Cartesi Machine.

**Documentation:** [`docs/watchdog/operator-deployment.md`](../docs/watchdog/operator-deployment.md) (Sepolia / mainnet) · [`docs/watchdog/getting-started.md`](../docs/watchdog/getting-started.md) (local devnet) · [`docs/watchdog/README.md`](../docs/watchdog/README.md) (architecture)

**Container image** (multi-arch `amd64` + `arm64`, published per release tag `vX`):

```bash
docker pull ghcr.io/cartesi/sequencer-watchdog:vX
# mirror: docker.io/cartesi/sequencer-watchdog:vX
```

```bash
just doctor                        # lua + cartesi + lcurl + machine_cartesi load probe
just watchdog-lua-deps             # .deps/lua/lcurl.so (needs libcurl + liblua5.4-dev)
just test-watchdog                 # unit tests (mocked HTTP; no lcurl required)
just devnet-for-watchdog           # local Anvil + sequencer-devnet (prints CARTESI_WATCHDOG_* env)
```

Watchdog-local recipes also live in [`justfile`](justfile) (`just -f watchdog/justfile <recipe>`).

Host packages and build errors: [`docs/watchdog/README.md`](../docs/watchdog/README.md#host-dependencies-watchdog-lua-deps).
