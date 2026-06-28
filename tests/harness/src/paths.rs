// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::path::{Path, PathBuf};

pub const DEFAULT_ANVIL_STATE_DIR: &str =
    "tests/benchmarks/.deps/rollups-contracts-2.2.0-anvil-v1.4.3";
pub const DEFAULT_MOCK_ERC20_ARTIFACT_PATH: &str =
    "tests/contracts/out/MockERC20.sol/MockERC20.json";
pub const DEFAULT_DEVNET_MACHINE_IMAGE_PATH: &str =
    "examples/canonical-app/out/canonical-machine-image";

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tests/harness crate lives under workspace root/tests")
        .to_path_buf()
}

pub fn resolve_from_workspace(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root().join(path)
    }
}

pub fn resolved_anvil_state_dir() -> PathBuf {
    workspace_root().join(DEFAULT_ANVIL_STATE_DIR)
}

pub fn mock_erc20_artifact_path() -> PathBuf {
    workspace_root().join(DEFAULT_MOCK_ERC20_ARTIFACT_PATH)
}

pub fn devnet_machine_image_path() -> PathBuf {
    workspace_root().join(DEFAULT_DEVNET_MACHINE_IMAGE_PATH)
}

const DEVNET_SEQUENCER_BIN: &str = "wallet-sequencer-devnet";

/// Resolve the `wallet-sequencer-devnet` binary built for the current Cargo invocation.
///
/// Prefers `CARGO_TARGET_DIR` (set by `cargo run` / `cargo test` in sandboxes and
/// custom target dirs) over the workspace `target/debug/` tree, which may be stale
/// when builds only run through Cargo with a redirected target directory.
pub fn resolve_devnet_sequencer_bin() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_WALLET_SEQUENCER_DEVNET") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        let path = PathBuf::from(target)
            .join("debug")
            .join(DEVNET_SEQUENCER_BIN);
        if path.exists() {
            return path;
        }
    }
    workspace_root()
        .join("target/debug")
        .join(DEVNET_SEQUENCER_BIN)
}
