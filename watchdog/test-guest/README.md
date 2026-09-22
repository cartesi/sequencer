# Watchdog test guest

A small Cartesi Machine guest that drives every outcome the watchdog's rollup
host must handle ([rollup host semantics](../../docs/cartesi-machine.md#rollup-host-semantics)).
Tests run it in the real emulator and use the `cartesi-machine` CLI as the
oracle.

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
| `exception` | `TX_EXCEPTION` with payload `test-guest exception` |
| `halt` | exits with status 7; the machine halts with payload 7 |
| starts with `report` | one report carrying the payload, then accepted |
| anything else | accepted |

| Inspect query | Outcome |
|---|---|
| `state` | one report with the whole 64 KiB NVRAM |
| anything else | one report `unsupported` |

The image also carries two musl stand-ins for guest-tools binaries, which are
linked against glibc and do not run on Alpine: `/usr/bin/nvram` (`nvram.sh`),
which the CLI's init lines call to find each NVRAM's `/dev/uioN`, and
`/usr/sbin/xhalt` (`src/bin/xhalt.rs`), which cartesi-init calls to halt with
the entrypoint's exit status. The guest resolves its own device from the label
the same way. trolley has no exception call, so the guest raises the exception
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

## Oracle checks

Advance inputs are `EvmAdvance` calldata (selector `0x415bf363`); inspect
queries are raw bytes. Run from the repository root inside `direnv exec . bash`:

```bash
IMG=$PWD/watchdog/test-guest/out/test-machine-image
W=$(mktemp -d)
adv() { # adv <payload>: writes $W/<payload>.bin
  cast calldata 'EvmAdvance(uint256,address,address,uint256,uint256,uint256,uint256,bytes)' \
    31337 0x0000000000000000000000000000000000000a11 0x0000000000000000000000000000000000005e4d \
    100 1700000000 0 0 "$(cast from-utf8 "$1")" | xxd -r -p > "$W/$1.bin"
}
for p in hello 'world!' reject exception halt; do adv "$p"; done
printf state > "$W/query-state"
check() { # check <run> <payload>...; QUERY=<file> inspects afterwards, MODE=stored clones
  local dir=$W/$1 i=0 inspect=() load=(--load="$IMG" --remote-spawn --remote-shutdown)
  shift; mkdir -p "$dir"
  for p in "$@"; do cp "$W/$p.bin" "$dir/input-$i.bin"; i=$((i + 1)); done
  if [ -n "${QUERY:-}" ]; then
    inspect=(--cmio-inspect-state=query:"$W/$QUERY",report:"$dir/query-report-%o.bin")
  fi
  if [ "${MODE:-}" = stored ]; then
    cp -R "$IMG" "$dir/m"
    load=(--load="$dir/m",sharing:all --revert-mode=stored)
  fi
  cartesi-machine "${load[@]}" \
    --cmio-advance-state=input:"$dir/input-%i.bin",input_index_begin:0,input_index_end:$i,check_outputs_merkle_root:false,output:"$dir/output-%o-input-%i.bin",rejected_output:"$dir/rejected-output-%o-input-%i.bin",output_proof:,report:"$dir/input-%i-report-%o.bin",outputs_merkle_root:,outputs_merkle_root_proof:,print_input_state_hashes \
    "${inspect[@]}" --final-hash
}
QUERY=query-state check a hello 'world!'
QUERY=query-state check b hello reject 'world!'
QUERY=query-state MODE=stored check b-stored hello reject 'world!'
check c hello exception 'world!'
check d hello halt 'world!'
```

The guest ignores input metadata. Keeping it fixed makes an input's bytes a
function of its payload alone, which matters for comparing root hashes across
runs: the CMIO RX buffer keeps the last input, so it is part of the state.

| Run | CLI exit | Result |
|---|---|---|
| `a` | 0 | `query-report-0.bin` is the 64 KiB layout: count 2, total 11, L 6, `world!` |
| `b`, `b-stored` | 0 | `rx-rejected`; input 2 starts from the pre-reject root; report and final hash equal `a`'s |
| `c` | 1 | `tx-exception`, `cmio exception with payload: "test-guest exception"`; input 2 never runs |
| `d` | 7 | `Halted with payload: 7`; input 2 never runs |

Loading a stored result with `cartesi.new():load(dir)` and reading the `state`
range with `read_memory` returns the same bytes as the `state` report.

`--no-revert` cannot run past a reject: the emulator refuses the next advance
because the machine is not at an `RX_ACCEPTED` yield. Storing right after the
reject shows the 0xFF bytes in the NVRAM.

The emulator gates only advances on the yield reason, so the CLI still delivers
an inspect query to `c`'s exception yield. The guest treats that resume as a
protocol violation and halts with payload 101 inside the discarded inspect
snapshot: the CLI exits 101, writes no report, and prints `c`'s final hash.
After a halt (`d`) the CLI skips the query.
