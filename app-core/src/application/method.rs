// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use ssz_derive::{Decode, Encode};

#[derive(PartialEq, Debug, Encode, Decode, Clone)]
#[ssz(enum_behaviour = "union")]
pub enum Method {
    Withdrawal(Withdrawal),
    Transfer(Transfer),
    Deposit(Deposit),
}

#[derive(PartialEq, Debug, Encode, Decode, Clone)]
pub struct Withdrawal {
    pub amount: U256,
}

#[derive(PartialEq, Debug, Encode, Decode, Clone)]
pub struct Transfer {
    pub amount: U256,
    pub to: Address,
}

#[derive(PartialEq, Debug, Encode, Decode, Clone)]
pub struct Deposit {
    pub amount: U256,
    pub to: Address,
}
