// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Boot-time construction of the Uniswap fee oracle — the one home for the
//! connect / classify / tolerate policy that `setup` and `run` previously
//! each hand-rolled by reaching past this module's boundary.
//!
//! The two commands share the connect-and-classify core but deliberately
//! keep different tolerance postures:
//!
//! - **`setup`** ([`connect_uniswap`] + [`persist_first_price`]): L1 is a
//!   hard requirement, and the pool must validate *before* anything is
//!   pinned — so connect runs early and any failure (transient or misconfig)
//!   aborts the command. The first price persists after identity pinning so
//!   `run` never samples an empty price.
//! - **`run`** ([`bootstrap_for_run`]): warm boot. The price is already
//!   pinned by setup, so a transient connect or first-refresh failure falls
//!   back to the persisted price — bounded by `max_price_age_ms` — with a
//!   reconnecting source. Misconfig stays terminal.

use std::time::Duration;

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
    /// The pool is unreachable right now; retrying may succeed.
    Transient(String),
}

/// Failure of `run`'s warm bootstrap. Transient connect failures never
/// surface here — [`bootstrap_for_run`] absorbs them by design.
#[derive(Debug)]
pub(crate) enum RunFeeOracleBootstrapError {
    Misconfig(String),
    /// The oracle failed after construction (a non-transient refresh error,
    /// or the persisted-price freshness bound was exceeded).
    Oracle(FeeOracleError),
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
                UniswapConnectError::Transient(message)
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
    max_price_age_ms: u64,
    process_lock: ProcessLock,
) -> Result<(), FeeOracleError> {
    let oracle = FeeOracle::new(
        db_path,
        FeeOracle::DEFAULT_POLL_INTERVAL,
        max_price_age_ms,
        provider,
        Box::new(token),
        process_lock,
    );
    oracle.refresh_once().await?;
    Ok(())
}

/// `run`'s warm bootstrap: prefer a live connect and refresh; tolerate a
/// transient failure by verifying the persisted price is fresher than
/// `max_price_age_ms` and (for a failed connect) running on a reconnecting
/// source. Misconfig stays terminal.
pub(crate) async fn bootstrap_for_run(
    db_path: String,
    rpc_url: &str,
    allow_insecure_rpc: bool,
    uniswap: UniswapConfig,
    poll_interval: Duration,
    max_price_age_ms: u64,
    process_lock: ProcessLock,
) -> Result<FeeOracle, RunFeeOracleBootstrapError> {
    let oracle = match connect_uniswap(rpc_url, allow_insecure_rpc, uniswap).await {
        Ok((provider, token)) => FeeOracle::new(
            db_path.clone(),
            poll_interval,
            max_price_age_ms,
            provider,
            Box::new(token),
            process_lock,
        ),
        Err(UniswapConnectError::Misconfig(message)) => {
            return Err(RunFeeOracleBootstrapError::Misconfig(message));
        }
        Err(UniswapConnectError::Transient(message)) => {
            warn_boot_fallback(&message);
            FeeOracle::ensure_persisted_price_fresh(&db_path, max_price_age_ms)
                .map_err(RunFeeOracleBootstrapError::Oracle)?;
            // The connect itself failed, so the worker starts on a source
            // that re-runs the connect on every quote.
            let provider = crate::l1::provider::create_provider(rpc_url, allow_insecure_rpc)
                .map_err(RunFeeOracleBootstrapError::Misconfig)?;
            return Ok(FeeOracle::reconnecting_uniswap(
                db_path,
                poll_interval,
                max_price_age_ms,
                provider,
                uniswap,
                process_lock,
            ));
        }
    };
    match oracle.refresh_once().await {
        Ok(_) => Ok(oracle),
        Err(FeeOracleError::Transient(message)) => {
            warn_boot_fallback(&message);
            FeeOracle::ensure_persisted_price_fresh(&db_path, max_price_age_ms)
                .map_err(RunFeeOracleBootstrapError::Oracle)?;
            Ok(oracle)
        }
        Err(error) => Err(RunFeeOracleBootstrapError::Oracle(error)),
    }
}

fn warn_boot_fallback(message: &str) {
    tracing::warn!(
        error = %message,
        "fee oracle unreachable at boot — continuing from persisted price"
    );
}
