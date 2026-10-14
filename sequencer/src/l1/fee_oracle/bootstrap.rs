// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Setup-time construction of the Uniswap fee oracle. Setup owns the only
//! synchronous connect-and-quote requirement: it validates the source before
//! pinning and persists a real first price before completing. `run` starts
//! entirely from that persisted price and quotes from its supervised worker.

use alloy::providers::DynProvider;

use super::uniswap::{UniswapConfig, UniswapV3PriceSource};
use super::worker::{FeeOracle, FeeOracleError};
use crate::runtime::process_lock::ProcessLock;

/// Connect to the configured pool. `setup` calls this while it still owns all
/// address-bearing configuration and before anything is pinned, so a
/// misconfigured pool never pins an identity. Failures classify as at runtime
/// (`From<PriceSourceError> for FeeOracleError`); a bad RPC URL is Misconfig.
pub(crate) async fn connect_uniswap(
    rpc_url: &str,
    allow_insecure_rpc: bool,
    uniswap: UniswapConfig,
) -> Result<(DynProvider, UniswapV3PriceSource), FeeOracleError> {
    let provider = crate::l1::provider::create_read_provider(rpc_url, allow_insecure_rpc)
        .map_err(FeeOracleError::Misconfig)?;
    let token = UniswapV3PriceSource::connect(provider.clone(), uniswap).await?;
    Ok((provider, token))
}

/// `setup`'s persist step: construct the oracle on an already-validated
/// `(provider, token)` pair, retain the data-directory lock, and quote once
/// so the pinned deployment always has a real first price. Setup requires
/// L1, so nothing is tolerated here.
pub(crate) async fn persist_first_price(
    db_path: String,
    provider: DynProvider,
    token: UniswapV3PriceSource,
    process_lock: ProcessLock,
) -> Result<(), FeeOracleError> {
    let oracle = FeeOracle::new(
        db_path,
        FeeOracle::DEFAULT_POLL_INTERVAL,
        provider,
        Box::new(token),
        process_lock,
    );
    oracle.refresh_once().await?;
    Ok(())
}
