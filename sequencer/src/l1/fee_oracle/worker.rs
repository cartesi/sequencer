// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Periodically refresh the fee-token price charged for L1 gas.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::providers::DynProvider;
use alloy_primitives::U256;
use async_trait::async_trait;
use thiserror::Error;
use tracing::{debug, warn};

use crate::l1::eip1559::{Eip1559Fees, estimate_fees};
use crate::l1::fee_oracle::math::{MathError, compute_x_units_per_gas, encode_log_gas_price};
use crate::l1::fee_oracle::uniswap::{PriceSourceError, TokenPriceSource};
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{Storage, StorageOpenError};

#[async_trait]
pub trait GasPriceSource: Send + Sync {
    async fn estimate_gas_fees(&self) -> Result<Eip1559Fees, String>;
}

#[async_trait]
impl GasPriceSource for DynProvider {
    async fn estimate_gas_fees(&self) -> Result<Eip1559Fees, String> {
        estimate_fees(self).await
    }
}

/// Explicit local-dev source. It never calls Uniswap.
#[derive(Debug, Clone)]
pub enum FixedPriceSource {
    LogGasPrice(u16),
    LinearXUnitsPerGas(U256),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshResult {
    pub log_gas_price: u16,
    pub changed: bool,
}

#[derive(Debug, Error)]
pub enum FeeOracleError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error("fee-oracle storage: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("fee-oracle DB task join: {0}")]
    Join(String),
    #[error("transient fee-oracle source failure: {0}")]
    Transient(String),
    #[error("fatal fee-oracle arithmetic failure: {0}")]
    FatalMath(#[from] MathError),
}

enum SourceMode {
    Live {
        gas: Arc<dyn GasPriceSource>,
        token: Arc<dyn TokenPriceSource>,
    },
    Fixed(FixedPriceSource),
}

pub struct FeeOracle {
    db_path: String,
    poll_interval: Duration,
    source: SourceMode,
}

impl FeeOracle {
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(12);

    pub fn new(
        db_path: impl Into<String>,
        poll_interval: Duration,
        gas: Arc<dyn GasPriceSource>,
        token: Arc<dyn TokenPriceSource>,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            poll_interval,
            source: SourceMode::Live { gas, token },
        }
    }

    pub fn fixed(
        db_path: impl Into<String>,
        poll_interval: Duration,
        source: FixedPriceSource,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            poll_interval,
            source: SourceMode::Fixed(source),
        }
    }

    /// Mandatory startup refresh. Runtime must call this before opening the
    /// inclusion lane so the first frame samples an L1-derived fee.
    pub async fn refresh_once(&self) -> Result<RefreshResult, FeeOracleError> {
        let (log_gas_price, live_values) = match &self.source {
            SourceMode::Fixed(FixedPriceSource::LogGasPrice(value)) => (*value, None),
            SourceMode::Fixed(FixedPriceSource::LinearXUnitsPerGas(value)) => {
                (encode_log_gas_price(*value), None)
            }
            SourceMode::Live { gas, token } => {
                let fees = gas
                    .estimate_gas_fees()
                    .await
                    .map_err(FeeOracleError::Transient)?;
                let quote = token
                    .quote_x_per_weth()
                    .await
                    .map_err(|err| FeeOracleError::Transient(err.to_string()))?;
                let linear = compute_x_units_per_gas(
                    fees.base_fee_per_gas,
                    fees.max_priority_fee_per_gas,
                    quote,
                    10,
                )?;
                (encode_log_gas_price(linear), Some((fees, quote, linear)))
            }
        };

        let db_path = self.db_path.clone();
        let refresh =
            tokio::task::spawn_blocking(move || -> Result<RefreshResult, FeeOracleError> {
                let mut storage = Storage::open(&db_path)?;
                let changed = storage.log_gas_price()? != log_gas_price;
                if changed {
                    storage.set_log_gas_price(log_gas_price)?;
                }
                Ok(RefreshResult {
                    log_gas_price,
                    changed,
                })
            })
            .await
            .map_err(|err| FeeOracleError::Join(err.to_string()))??;

        if let Some((fees, quote_x_per_weth, linear_x_per_gas)) = live_values {
            if refresh.changed {
                tracing::info!(
                    base_fee = fees.base_fee_per_gas,
                    priority_fee = fees.max_priority_fee_per_gas,
                    quote_x_per_weth = %quote_x_per_weth,
                    linear_x_per_gas = %linear_x_per_gas,
                    log_gas_price = refresh.log_gas_price,
                    changed = refresh.changed,
                    "refreshed L1 fee oracle",
                );
            } else {
                debug!(
                    base_fee = fees.base_fee_per_gas,
                    priority_fee = fees.max_priority_fee_per_gas,
                    quote_x_per_weth = %quote_x_per_weth,
                    linear_x_per_gas = %linear_x_per_gas,
                    log_gas_price = refresh.log_gas_price,
                    changed = refresh.changed,
                    "L1 fee oracle price unchanged",
                );
            }
        }
        Ok(refresh)
    }

    pub fn start(
        self,
        shutdown: ShutdownSignal,
    ) -> Result<tokio::task::JoinHandle<Result<(), FeeOracleError>>, StorageOpenError> {
        let _ = Storage::open_read_only(self.db_path.as_str())?;
        Ok(tokio::spawn(
            async move { self.run_forever(shutdown).await },
        ))
    }

    async fn run_forever(self, shutdown: ShutdownSignal) -> Result<(), FeeOracleError> {
        let mut last_success = Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.wait_for_shutdown() => return Ok(()),
                result = self.refresh_once() => match result {
                    Ok(refresh) => {
                        last_success = Instant::now();
                        if refresh.changed {
                            debug!(log_gas_price = refresh.log_gas_price, "updated L1 fee oracle price");
                        }
                    }
                    Err(FeeOracleError::Transient(error)) => {
                        warn!(%error, retained_age_secs = last_success.elapsed().as_secs(), "retaining last L1 fee-oracle price after transient failure");
                    }
                    Err(error) => return Err(error),
                },
            }
            tokio::select! {
                biased;
                _ = shutdown.wait_for_shutdown() => return Ok(()),
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
        }
    }
}

impl From<PriceSourceError> for FeeOracleError {
    fn from(error: PriceSourceError) -> Self {
        Self::Transient(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::temp_db;

    #[tokio::test]
    async fn fixed_source_updates_only_when_log_price_changes() {
        let db = temp_db("fixed-fee-oracle");
        let oracle = FeeOracle::fixed(
            db.path.clone(),
            Duration::from_secs(1),
            FixedPriceSource::LogGasPrice(123),
        );
        assert_eq!(
            oracle.refresh_once().await.unwrap(),
            RefreshResult {
                log_gas_price: 123,
                changed: true
            }
        );
        assert_eq!(
            oracle.refresh_once().await.unwrap(),
            RefreshResult {
                log_gas_price: 123,
                changed: false
            }
        );
    }
}
