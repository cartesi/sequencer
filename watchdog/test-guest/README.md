# Watchdog test guest

A small Cartesi Machine guest that drives every outcome the watchdog's rollup
host must handle ([rollup host semantics](../../docs/cartesi-machine.md#rollup-host-semantics)).
`watchdog/tests/e2e.lua` (`just test-watchdog-e2e`) runs it through the
watchdog's executor and through the `cartesi-machine` CLI and requires equal
root hashes; [Cartesi Machine facts](../../docs/cartesi-machine.md#the-cli-as-a-test-oracle)
says what the CLI cannot tell an oracle.

## Behavior

State lives in a 64 KiB NVRAM labeled `state`, little-endian:

| Bytes | Content |
|---|---|
| 0..8 | accepted inputs (u64) |
| 8..16 | total accepted payload bytes (u64) |
| 16..20 | length L of the last accepted payload (u32, capped at 4096) |
| 20..20+L | the last accepted payload's first L bytes |
| rest | zero |

The guest zeroes the NVRAM before its first accept yield, so the template's
NVRAM is all zero.

| Advance payload | Outcome |
|---|---|
| `reject` | writes 0xFF over bytes 0..64, then `RX_REJECTED` |
| `exception` | `TX_EXCEPTION` with payload `test-guest exception`; a host that resumes it anyway makes it halt with payload 101 |
| `halt` | exits with status 7; the machine halts with payload 7 |
| starts with `report` | one report carrying the payload, then accepted |
| anything else | accepted |

| Inspect query | Outcome |
|---|---|
| `state` | one report with the whole 64 KiB NVRAM |
| anything else | one report `unsupported` |

The image carries musl stand-ins for two glibc guest-tools binaries
([why](../../docs/cartesi-machine.md#the-guest-side)): `/usr/bin/nvram`
(`nvram.sh`) and `/usr/sbin/xhalt` (`src/bin/xhalt.rs`). The guest resolves its
own NVRAM device from the label as `nvram.sh` does, and raises its exception
through `libcmt-sys` directly.

## Build

The kernel is the canonical image's; fetch it once with
`just canonical download-deps`. Then, from the repository root:

```bash
direnv exec . just --justfile watchdog/test-guest/justfile build-image
```

This cleans `out/`, cross-builds both binaries, builds the rootfs, and stores
the template at `watchdog/test-guest/out/test-machine-image` (64 MiB RAM,
`--nvram=label:state,length:64Ki,user:dapp`, `--assert-rolling-template`). The
build is deterministic: rebuilding prints the same `--final-hash`.
