# Release bundle versioning

All release artifacts for tag `vX.Y.Z` share one **bundle version** (`vX.Y.Z`) and one
**toolchain pin set** in [`versions.env`](versions.env).

| Artifact | Version key |
|----------|-------------|
| `sequencer-vX-linux-{amd64,arm64}.tar.gz` | `vX` (git tag) |
| `canonical-machine-image-{devnet,sepolia}-vX.tar.gz` | `vX` + guest built with pins below |
| `sequencer-watchdog-vX-linux-{amd64,arm64}.tar.gz` | `vX` OCI image (`docker save`) |
| `release-manifest-vX.json` | Lists all artifacts + pins |

## Single pin source

Edit **`release/versions.env` only** — CI and release workflows load it via
[`.github/actions/load-release-versions`](../.github/actions/load-release-versions/action.yml).
After editing, run `bash scripts/verify-release-versions.sh` (also enforced in CI) and bump
`rust-toolchain.toml` / `watchdog/third_party/lua-curl/UPSTREAM` when those pins change.

`CARTESI_MACHINE_VERSION` must match:

- The `cartesi-machine` inside the watchdog image
- The emulator used to build `canonical-machine-image-*` tarballs

Mismatch causes CM load/advance failures or false `state_mismatch` alarms.

## Watchdog image

```bash
docker load < sequencer-watchdog-vX-linux-amd64.tar.gz
docker run --rm \
  -e WATCHDOG_MODE=compare \
  -e WATCHDOG_ONCE=1 \
  -e WATCHDOG_SEQUENCER_URL=... \
  -e WATCHDOG_L1_RPC_URL=... \
  -v /var/lib/watchdog/checkpoints:/checkpoints \
  sequencer-watchdog:vX
```

Mount `canonical-machine-image-sepolia-vX.tar.gz` extract for bootstrap
(`WATCHDOG_CM_SNAPSHOT_DIR`) on first run.

Inspect alignment:

```bash
docker inspect --format '{{ index .Config.Labels "org.cartesi.sequencer.release-tag" }}' IMAGE
cat /opt/watchdog/RELEASE.json   # inside container
```
