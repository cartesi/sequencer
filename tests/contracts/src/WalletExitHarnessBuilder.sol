// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;
import {WalletWithdrawalOutputBuilder} from "wallet/WalletWithdrawalOutputBuilder.sol";

contract WalletExitHarnessBuilder is WalletWithdrawalOutputBuilder {
    constructor(address token) WalletWithdrawalOutputBuilder(token) {}
}
