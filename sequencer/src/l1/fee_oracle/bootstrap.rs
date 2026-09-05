// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Setup-time construction of the Uniswap fee oracle. Setup owns the only
//! synchronous connect-and-quote requirement: it validates the source before
//! pinning and persists a real first price before completing. `run` starts
//! entirely from that persisted price and quotes from its supervised worker.

use alloy::providers::DynProvider;

use super::uniswap::{UniswapConfig, UniswapV3PriceSource, bootstrap_price_source_error};
use super::worker::{FeeOracle, FeeOracleError};
use crate::runtime::process_lock::ProcessLock;

/// Failure to reach the configured pool at connect time.
#[derive(Debug)]
pub(crate) enum UniswapConnectError {
    /// Wrong pool/pair/chain or a bad RPC URL: deterministic; retrying the
    /// same configuration re-fails identically.
    Misconfig(String),
    /// The source is unavailable right now; retrying may succeed.
    Transient { message: String },
}

/// Connect to the configured pool and classify any failure. `setup` calls
/// this while it still owns all address-bearing configuration and before
/// anything is pinned, so a misconfigured pool never pins an identity.
pub(crate) async fn connect_uniswap(
    rpc_url: &str,
    allow_insecure_rpc: bool,
    uniswap: UniswapConfig,
) -> Result<(DynProvider, UniswapV3PriceSource), UniswapConnectError> {
    let provider = crate::l1::provider::create_provider(rpc_url, allow_insecure_rpc)
        .map_err(UniswapConnectError::Misconfig)?;
    match UniswapV3PriceSource::connect(provider.clone(), uniswap).await {
        Ok(token) => Ok((provider, token)),
        Err(error) => {
            let (transient, message) = bootstrap_price_source_error(error);
            Err(if transient {
                UniswapConnectError::Transient { message }
            } else {
                UniswapConnectError::Misconfig(message)
            })
        }
    }
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
