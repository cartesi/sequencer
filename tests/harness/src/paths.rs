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
