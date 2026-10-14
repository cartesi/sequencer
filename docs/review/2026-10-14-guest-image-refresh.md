# Guest image refresh, 2026-10-14

**Consumer:** the release notes of the next canonical guest image, and the next
change that moves the template hash, which compares against these values.
**Retire** this record once that change records its own hashes.

W3b of the [whole-repo review](2026-10-13-whole-repo-review.md) moved the
template hash once, deliberately, with no intended behavior change: a Rust
toolchain and dependency refresh followed by guest-crate refactors. What moves
the hash, and how a behavior-preserving move is checked, is in
[`docs/cartesi-machine.md`](../cartesi-machine.md#versions-are-all-or-nothing).

## Method

- Hashes: `just canonical-print-build-hashes` (devnet) and
  `just canonical-build-machine-image-sepolia` plus `cartesi-machine --load=…
  --initial-hash` (Sepolia), cartesi-machine 0.21.0, cross 0.2.5. Every value
  below is from a cold build (no `target/` for the guest or its host side),
  with `rust-src` in the cross toolchain (see `docs/cartesi-machine.md`). Two
  cold builds of the final commit agreed, and so
  did builds from three checkout paths.
- Behavior: [`tests/fixtures/consensus_vectors_v1.json`](../../tests/fixtures/consensus_vectors_v1.json),
  pinned at `6000e3a` before anything moved and byte-identical at the final
  commit. The workspace tests check it natively at every commit, and
  `just canonical-test-guest` (15 cases, 5 golden) checks it in the built image
  after each dependency step and after the refactors. A
  stdlib-only Python reimplementation (Keccak, EIP-712, ABI encoding, fee table,
  secp256k1 recovery) reproduced 47 of the pinned values when they were pinned.
- Dev shell: nixpkgs moved from `104240a` (2026-08-03) to `8edc0c7`
  (2026-10-08) first; both images were bit-identical before and after, and the
  devnet and watchdog e2e suites passed on the new shell.

## Results

Devnet initial hash and cycles to the first yield:

| Step | Commit | Initial hash | Cycles |
|---|---|---|---|
| Baseline, Rust 1.95.0 | `6000e3a` | `0x489e84ce7aaf451f36ebea813b3c1d9b12137ee4b78c91d6f73a1fdac372a180` | 51284983 |
| Rust 1.99.0 | `6009104` | `0x6c323c329e712445d911be8972c92c78808dd4943cbbef9765e4c3370ef00ffb` | 51252817 |
| alloy-core 1.7.3 | `02d8fa9` | `0x94b6a4779094aaea1f5c7adce8b3b8a2fda94bead6ed2802c6312c069be7e9d6` | 51252675 |
| 11 guest-compiled crates | `fb37bc9` | `0xc1fc8f03cfdb69adb1b9c382fddc27ecad383fcf725e3b1a2d986ef1689f5b1f` | 51248352 |
| Guest refactors (B6–J5) | `8d03166` | `0xa006cd4d34685b2804737bcef48e3b398999c30081106a5745216b08fdad223b` | 51222421 |
| 6 updates in the guest's build graph | `711f312` | `0xa4c01357665937d80f006c9c3726c49d0a80fe7aea2d3b8bf2199ea7333d388d` | 51222416 |

The commits before `6009104` (libtest-mimic, tokio-tungstenite, async-trait)
and the remaining compatible updates after `711f312` left every artifact
byte-identical. Among the guest refactors, every commit that touched guest
source moved the dapp binary, including those whose changed code the guest never
calls (B6, C3b, J1); the test-only J5 commit and the manifest commit did not.
`711f312` updates only crates that run on the host inside proc macros and
bindgen, yet moves the binary. cpufeatures, which only keccak uses and only on
aarch64 hosts, went with the host-only updates: the x86_64 cross build never
compiles it.

| Image | Baseline (`6000e3a`) | Final (`756a8ac`) |
|---|---|---|
| devnet dapp sha256 | `9415789ca67687000081ef4038cd6ac7b1508c0f9c2d42db81ee9281c492bee8` | `96a97d65cf45134c47778c03fa0b5239226553a7766fd6f89145961f4975f95b` |
| Sepolia dapp sha256 | `ca9fd50eda3773c628ef7d5d47eeba532d1b97c22f7e4f7b0a4f01d986482bd7` | `9d8df1e003c3cb11deada2b024957a5eff3a4e425abf480f367ec99f0ed8c79d` |
| Sepolia initial hash | `0xc6fd1515ba68c7256b16e17ced3bd48f9d5ede5c8079c14729f2ef5275f11457` | `0x3d7adeebdcdc72e7fc5f767eb4568fe46d0ef763f0c26044e9b302962264664a` |

`f0d1ccf` is the last commit that reproduces the baseline image.

## Limits

The vectors are samples: equivalence beyond them rests on `canonical-test` and
the devnet e2e suite, which runs the Rust and C hosts and the duality test.
Guest report text is not pinned: it is not consensus and may change with the
toolchain. Cycle counts are not consensus either. A warm `target/` built partly
before `rust-src` arrived mixes both kinds of embedded paths and gives a hash
no cold build reproduces; measure hashes cold.
