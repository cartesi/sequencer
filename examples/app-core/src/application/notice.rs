// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    function DepositNotice(address token, address sender, uint256 amount) external;

    #[derive(Debug, PartialEq, Eq)]
    function TransferNotice(address sender, address recipient, uint256 amount) external;
}

pub type DepositNotice = DepositNoticeCall;
pub type TransferNotice = TransferNoticeCall;
