// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

/// @notice Reference wallet exits preserve the full uint256 token amount.
contract WalletWithdrawalOutputBuilder {
    address public immutable token;

    constructor(address token_) {
        token = token_;
    }

    function buildWithdrawalOutput(address, bytes calldata account)
        external view returns (bytes memory)
    {
        require(account.length == 64, "invalid wallet account length");
        (address owner, uint256 balance) = abi.decode(account, (address, uint256));
        return abi.encodeWithSignature(
            "Voucher(address,uint256,bytes)", token, uint256(0),
            abi.encodeWithSignature("transfer(address,uint256)", owner, balance)
        );
    }
}
