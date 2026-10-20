// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

/// @dev Test fixture: consensus finalization is supplied by the scenario.
contract CheckpointValidator {
    bytes32 public root;
    uint256 public inputCount;

    function finalize(bytes32 root_, uint256 inputCount_) external {
        root = root_;
        inputCount = inputCount_;
    }

    function supportsInterface(bytes4) external pure returns (bool) { return true; }
    function getLastFinalizedMachineMerkleRoot(address) external view returns (bytes32) { return root; }
    function wasInputFinalized(address, uint256 index, uint256) external view returns (bool) { return index < inputCount; }
    function isOutputsMerkleRootValid(address, bytes32) external pure returns (bool) { return false; }
}
