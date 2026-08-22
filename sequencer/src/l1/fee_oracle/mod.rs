// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! L1-native fee-token gas-price oracle.

mod bootstrap;
pub mod math;
pub mod uniswap;
pub mod worker;

pub(crate) use bootstrap::{
    RunFeeOracleBootstrapError, UniswapConnectError, bootstrap_for_run, connect_uniswap,
    persist_first_price,
};
pub use uniswap::{TokenPriceSource, UniswapConfig, UniswapV3PriceSource};
pub use worker::FeeOracle;
