# Guest SDK

The crates an application author uses on the canonical side: to build the
Cartesi Machine guest (`examples/canonical-app`), to encode its rollup inputs
and outputs, and to test the stored guest image (`examples/canonical-test`).
The watchdog's test guest (`watchdog/test-guest`) uses them too. The client
side of the SDK is `sdk/rust-client`. These crates are workspace members and
are not published.

## Provenance

- **Rust crates:** imported from
  [GCdePaula/cartesi-tools-rs](https://github.com/GCdePaula/cartesi-tools-rs)
  at rev `ed14b98ecfe9796dc3ca7c9b96bfdbf0ef9baf22`
  (`guest/libcmt-sys`, `guest/trolley`, `host/testsi`, and `types`, renamed
  here to `rollups-types`; its examples were not imported). Since then they
  have been updated here for Cartesi Machine v0.21.0 and machine-guest-tools
  v0.18.0. Apache-2.0 licensed.
- **libcmt C sources** (`libcmt-sys/libcmt/`): the `sys-utils/libcmt`
  `Makefile`, `include/` and `src/`, plus the top-level `LICENSE` (Apache-2.0)
  and `AUTHORS`, from
  [cartesi/machine-guest-tools](https://github.com/cartesi/machine-guest-tools)
  tag `v0.18.0` (commit `5222250c69371f7cbe96e6082699a8ca22b5969c`), taken
  from `https://github.com/cartesi/machine-guest-tools/archive/refs/tags/v0.18.0.tar.gz`
  (sha256 `a16ee31a7abb0522ae70497721bf4856665297e1dd26d875a43fadbe48814bb6`).
  The files are unmodified. libcmt must match the guest-tools version pinned
  in `examples/canonical-app/Dockerfile`, which pairs with the emulator
  release in `toolchain-pins.env`.

## Crates

- **`libcmt-sys`**: raw bindgen bindings to libcmt. On `riscv64` targets
  `build.rs` builds the real library with `riscv64-unknown-linux-musl-gcc` (from
  the pinned `cross` image). On the host it builds libcmt's mock I/O backend, so
  the guest crates still compile and lint there.
- **`trolley`**: a safe guest-side rollup API over libcmt: the input loop,
  vouchers, notices, reports and GIO.
- **`testsi`** (with its proc-macro crate `testsi-macros`): a host-side test
  harness that loads a stored machine image and feeds it advance-state inputs.
  It uses the Cartesi Machine v0.21 Rust bindings from
  [cartesi/dave](https://github.com/cartesi/dave), pinned by rev in
  `testsi/Cargo.toml`. These link the prebuilt emulator named by
  `LIBCARTESI_PATH` / `INCLUDECARTESI_PATH`: the dev flake exports both, and so
  does CI when it installs the emulator `.deb`.
- **`rollups-types`**: the Cartesi Rollups ABI types (`EvmAdvance` input,
  vouchers, notices, portal deposit encodings), shared by the guest and host
  crates; `examples/app-core` decodes portal deposits with it.

## Updating libcmt

1. Download the new machine-guest-tools tag tarball and record its sha256.
2. Replace `libcmt-sys/libcmt/{Makefile,include,src,LICENSE,AUTHORS}` with the
   new tag's files.
3. Update the version and hashes above.
4. Bump the guest-tools pin in `examples/canonical-app/Dockerfile` to match.
