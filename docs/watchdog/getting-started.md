# Watchdog Local Development

This guide runs the watchdog against a local devnet: Anvil with the rollups
contracts, and `wallet-sequencer-devnet`. The [watchdog README](README.md) owns
how the watchdog works and its [configuration](README.md#configuration);
[deployment](operator-deployment.md) covers live chains.

## Prerequisites

The Nix/direnv development shell provides the tools. Without it, install:

- Lua 5.4 with its headers, a C compiler, and libcurl development files that
  `pkg-config` finds, for the native modules;
- `cartesi-machine` at `CARTESI_MACHINE_VERSION` from
  [`toolchain-pins.env`](../../toolchain-pins.env), with its `cartesi` Lua
  module;
- `flock` (util-linux), which the `sequencer-watchdog` wrapper takes its lock
  with;
- Foundry and the pinned Rust toolchain
  ([shell and commands](../../AGENTS.md#shell-and-commands)) for the devnet;
- `cross`, Docker buildx able to build `linux/riscv64`, and `xgenext2fs`, to
  build machine images.

Then, from the repository root:

```bash
just setup                                  # Anvil state, test contracts, kernel, native modules
just canonical-build-machine-image          # devnet canonical image
just canonical-build-machine-image-sepolia  # Sepolia canonical image
just doctor                                 # Lua, the emulator, the native modules, the devnet image
```

`just watchdog-lua-deps`, which `just setup` and the watchdog test recipes run,
compiles lua-curl (`lcurl.so`) and LuaFileSystem (`lfs.so`) from the vendored
sources into `.deps/lua`.

`just test-watchdog-e2e` needs the Sepolia canonical image and the test guest
image (`watchdog/test-guest/out/test-machine-image`), which it builds on first
use; the [test guest README](../../watchdog/test-guest/README.md) explains the
guest and how to rebuild it. `just test-watchdog-compare-harness` and
`just test-rollups-e2e` build missing canonical images themselves but reuse
existing ones, so rebuild an image after changing the wallet or its guest. The
README's [Tests](README.md#tests) table says what each recipe exercises.

## Path A: the automated harness

```bash
just test-watchdog-compare-harness
```

The recipe runs `just setup`, builds the devnet image if it is missing, builds
`wallet-sequencer-devnet` and the e2e runner, and runs
`watchdog_genesis_compare_test` against a fresh Anvil and sequencer. The
scenario checks genesis parity and the watchdog's start through its production
wrapper:

1. The sequencer's `/finalized_state` at genesis equals the devnet wallet's
   genesis SSZ state.
2. The devnet image answers the inspect query `state`, run through the
   `cartesi-machine` CLI, with the same bytes.
3. `init` accepts the image against the application's on-chain template hash.
4. Two ticks are idle at the genesis block, and `status` reports it.

It replays no inputs. `just test-rollups-e2e` runs the comparisons after the
sequencer accepts batches, through both the Rust and the C-engine host, and
`watchdog_divergence_drill_test`, which walks the
[incident runbook](incident-runbook.md) with the real commands.

## Path B: interactive

In one terminal:

```bash
just devnet-for-watchdog
```

It starts Anvil and `wallet-sequencer-devnet` on ephemeral ports and prints
`export CARTESI_WATCHDOG_*=…` lines: the sequencer and L1 URLs, chain id
`31337`, the application address, a state directory under the
system temporary directory, the devnet image as the bootstrap at block 0,
`CARTESI_WATCHDOG_STATE_SOURCE=inspect`, and the native modules. Leave it
running; Ctrl+C stops both processes. If either exits on its own, the command
prints its log path and the log's tail, then exits.

In a second terminal, from the repository root, paste the exports, then:

```bash
export CARTESI_WATCHDOG_LUA_ROOT="$PWD"
curl -s "$CARTESI_WATCHDOG_SEQUENCER_URL/finalized_state/digest"
./watchdog/sequencer-watchdog init
./watchdog/sequencer-watchdog tick
./watchdog/sequencer-watchdog status
```

On a fresh devnet the digest route answers for block 0. `init` logs
`watchdog: init: initialized; head at block 0 (0 inputs)`, `tick` logs
`watchdog: tick: idle at block 0` and exits 0, and `status` prints the head and
the last tick as JSON. Ticks stay idle until the sequencer accepts a batch
past genesis; the next tick then replays the new inputs and logs
`watchdog: tick: sequencer agrees at block <B> (from block 0)`.
`prepare_non_genesis_watchdog_state` in
[`tests/e2e/src/test_cases.rs`](../../tests/e2e/src/test_cases.rs) drives the
deposits and transfers that get there.

Restarting `just devnet-for-watchdog` starts a new chain on new ports. Paste
the new exports: `tick` reads both endpoints from the environment. The printed
state directory is the same path every time, and a head from the old chain
does not describe the new one: remove `$CARTESI_WATCHDOG_STATE_DIR` and run
`init` again.

## Troubleshooting

| Message | Fix |
|---|---|
| `watchdog-lua-deps: libcurl dev package not found (libcurl4-openssl-dev or similar)` | Install libcurl development files that `pkg-config` finds |
| `watchdog-lua-deps: Lua headers not found; install the Lua 5.4 headers (liblua5.4-dev) or set LUA_INC` | Install the Lua 5.4 headers (`liblua5.4-dev` on Debian), or set `LUA_INC` to the directory holding `lua.h` |
| `watchdog-lua-deps: built lcurl.so but <lua> cannot load it (Lua version mismatch?)` | The headers and the interpreter belong to different Lua versions |
| `doctor: cartesi-machine not on PATH` | Install the pinned emulator, or enter the development shell |
| `missing the sepolia canonical image; run: just canonical-build-machine-image-sepolia` | `just test-watchdog-e2e` needs it |
| `CM inspect bytes mismatch (len <N> vs expected <M>)` | The devnet image is older than the wallet code; rebuild it with `just canonical-build-machine-image` |
| `watchdog state is already locked: <dir>/run.lock` | Another `init`, `tick`, or `clear` is running |
| `flock is required (util-linux)` | Use the devshell, or install util-linux |
| `sequencer /finalized_state/inclusion_block: GET <url> failed: …` | The devnet stack stopped, or the exports come from an earlier run |
| `divergence inclusion_block_regressed latched at block 0` | The state directory comes from an earlier devnet run; remove it and run `init` again |
