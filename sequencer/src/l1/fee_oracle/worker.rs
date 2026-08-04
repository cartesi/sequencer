// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Periodically refresh the fee-token price charged for L1 gas.

use std::time::{Duration, Instant};

use alloy::providers::DynProvider;
use async_trait::async_trait;
use thiserror::Error;
use tracing::{debug, warn};

use crate::l1::eip1559::{Eip1559Fees, estimate_fees};
use crate::l1::fee_oracle::math::{MathError, compute_x_units_per_gas, encode_log_gas_price};
use crate::l1::fee_oracle::uniswap::{PriceSourceError, TokenPriceSource};
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{Storage, StorageOpenError};

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

#[async_trait]
trait GasFeeSource: Send + Sync {
    async fn estimate_gas_fees(&self) -> Result<Eip1559Fees, String>;
}

#[async_trait]
impl GasFeeSource for DynProvider {
    async fn estimate_gas_fees(&self) -> Result<Eip1559Fees, String> {
        estimate_fees(self).await
    }
}

pub struct FeeOracle {
    db_path: String,
    poll_interval: Duration,
    gas: Box<dyn GasFeeSource>,
    token: Box<dyn TokenPriceSource>,
}

impl FeeOracle {
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(12);

    pub fn new(
        db_path: impl Into<String>,
        poll_interval: Duration,
        provider: DynProvider,
        token: Box<dyn TokenPriceSource>,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            poll_interval,
            gas: Box::new(provider),
            token,
        }
    }

    #[cfg(test)]
    fn new_with_sources(
        db_path: impl Into<String>,
        poll_interval: Duration,
        gas: Box<dyn GasFeeSource>,
        token: Box<dyn TokenPriceSource>,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            poll_interval,
            gas,
            token,
        }
    }

    /// Mandatory startup refresh. Runtime must call this before opening the
    /// inclusion lane so the first frame samples an L1-derived fee.
    pub async fn refresh_once(&self) -> Result<RefreshResult, FeeOracleError> {
        let fees = self
            .gas
            .estimate_gas_fees()
            .await
            .map_err(FeeOracleError::Transient)?;
        let quote = self
            .token
            .quote_x_per_weth()
            .await
            .map_err(|err| FeeOracleError::Transient(err.to_string()))?;
        let linear =
            compute_x_units_per_gas(fees.base_fee_per_gas, fees.max_priority_fee_per_gas, quote)?;
        let log_gas_price = encode_log_gas_price(linear)?;

        let db_path = self.db_path.clone();
        let refresh =
            tokio::task::spawn_blocking(move || -> Result<RefreshResult, FeeOracleError> {
                let mut storage = Storage::open_writer(&db_path)?;
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

        if refresh.changed {
            tracing::info!(
                base_fee = fees.base_fee_per_gas,
                priority_fee = fees.max_priority_fee_per_gas,
                quote_x_per_weth = %quote,
                linear_x_per_gas = %linear,
                log_gas_price = refresh.log_gas_price,
                changed = refresh.changed,
                "refreshed L1 fee oracle",
            );
        } else {
            debug!(
                base_fee = fees.base_fee_per_gas,
                priority_fee = fees.max_priority_fee_per_gas,
                quote_x_per_weth = %quote,
                linear_x_per_gas = %linear,
                log_gas_price = refresh.log_gas_price,
                changed = refresh.changed,
                "L1 fee oracle price unchanged",
            );
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
    use alloy_primitives::U256;
    use std::sync::Mutex;

    struct StaticGas(Eip1559Fees);

    #[async_trait]
    impl GasFeeSource for StaticGas {
        async fn estimate_gas_fees(&self) -> Result<Eip1559Fees, String> {
            Ok(self.0)
        }
    }

    struct StaticToken(U256);

    #[async_trait]
    impl TokenPriceSource for StaticToken {
        async fn quote_x_per_weth(&self) -> Result<U256, PriceSourceError> {
            Ok(self.0)
        }
    }

    struct FailsAfterFirstGas {
        calls: Mutex<usize>,
        ok: Eip1559Fees,
    }

    #[async_trait]
    impl GasFeeSource for FailsAfterFirstGas {
        async fn estimate_gas_fees(&self) -> Result<Eip1559Fees, String> {
            let mut calls = self.calls.lock().expect("lock");
            *calls += 1;
            if *calls == 1 {
                Ok(self.ok)
            } else {
                Err("rpc unavailable".into())
            }
        }
    }

    struct FailsAfterFirstToken {
        calls: Mutex<usize>,
        ok: U256,
    }

    #[async_trait]
    impl TokenPriceSource for FailsAfterFirstToken {
        async fn quote_x_per_weth(&self) -> Result<U256, PriceSourceError> {
            let mut calls = self.calls.lock().expect("lock");
            *calls += 1;
            if *calls == 1 {
                Ok(self.ok)
            } else {
                Err(PriceSourceError::Provider("pool unavailable".into()))
            }
        }
    }

    struct OverflowToken;

    #[async_trait]
    impl TokenPriceSource for OverflowToken {
        async fn quote_x_per_weth(&self) -> Result<U256, PriceSourceError> {
            Ok(U256::MAX)
        }
    }

    fn sample_fees() -> Eip1559Fees {
        Eip1559Fees {
            base_fee_per_gas: 19_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 39_000_000_000,
        }
    }

    fn sample_quote() -> U256 {
        U256::from(1_800_000_000u64)
    }

    fn expected_log_price() -> u16 {
        encode_log_gas_price(
            compute_x_units_per_gas(19_000_000_000, 1_000_000_000, sample_quote()).unwrap(),
        )
        .unwrap()
    }

    fn initialize_db(path: &str) {
        let _ = Storage::open(path).expect("initialize storage schema");
    }

    #[tokio::test]
    async fn live_source_updates_only_when_encoded_price_changes() {
        let db = temp_db("live-fee-oracle");
        initialize_db(&db.path);
        let expected_log = expected_log_price();
        let oracle = FeeOracle::new_with_sources(
            db.path.clone(),
            Duration::from_secs(1),
            Box::new(StaticGas(sample_fees())),
            Box::new(StaticToken(sample_quote())),
        );
        assert_eq!(
            oracle.refresh_once().await.unwrap(),
            RefreshResult {
                log_gas_price: expected_log,
                changed: true
            }
        );
        assert_eq!(
            oracle.refresh_once().await.unwrap(),
            RefreshResult {
                log_gas_price: expected_log,
                changed: false
            }
        );
        let storage = Storage::open_read_only(&db.path).unwrap();
        assert_eq!(storage.log_gas_price().unwrap(), expected_log);
    }

    #[tokio::test]
    async fn transient_gas_failure_retains_previous_price() {
        let db = temp_db("retain-gas-fee-oracle");
        initialize_db(&db.path);
        let expected_log = expected_log_price();
        let oracle = FeeOracle::new_with_sources(
            db.path.clone(),
            Duration::from_secs(1),
            Box::new(FailsAfterFirstGas {
                calls: Mutex::new(0),
                ok: sample_fees(),
            }),
            Box::new(StaticToken(sample_quote())),
        );
        assert_eq!(
            oracle.refresh_once().await.unwrap().log_gas_price,
            expected_log
        );
        let err = oracle.refresh_once().await.expect_err("second call fails");
        assert!(matches!(err, FeeOracleError::Transient(_)));
        let storage = Storage::open_read_only(&db.path).unwrap();
        assert_eq!(storage.log_gas_price().unwrap(), expected_log);
    }

    #[tokio::test]
    async fn transient_token_failure_retains_previous_price() {
        let db = temp_db("retain-token-fee-oracle");
        initialize_db(&db.path);
        let expected_log = expected_log_price();
        let oracle = FeeOracle::new_with_sources(
            db.path.clone(),
            Duration::from_secs(1),
            Box::new(StaticGas(sample_fees())),
            Box::new(FailsAfterFirstToken {
                calls: Mutex::new(0),
                ok: sample_quote(),
            }),
        );
        assert_eq!(
            oracle.refresh_once().await.unwrap().log_gas_price,
            expected_log
        );
        let err = oracle.refresh_once().await.expect_err("second call fails");
        assert!(matches!(err, FeeOracleError::Transient(_)));
        let storage = Storage::open_read_only(&db.path).unwrap();
        assert_eq!(storage.log_gas_price().unwrap(), expected_log);
    }

    #[tokio::test]
    async fn arithmetic_overflow_is_fatal() {
        let db = temp_db("fatal-fee-oracle");
        // Ensure the schema exists so a later read would be meaningful; the
        // overflow path must fail before any write.
        let storage = Storage::open(&db.path).unwrap();
        assert_eq!(storage.log_gas_price().unwrap(), 0);
        drop(storage);

        let oracle = FeeOracle::new_with_sources(
            db.path.clone(),
            Duration::from_secs(1),
            Box::new(StaticGas(Eip1559Fees {
                base_fee_per_gas: u128::MAX,
                max_priority_fee_per_gas: u128::MAX,
                max_fee_per_gas: u128::MAX,
            })),
            Box::new(OverflowToken),
        );
        let err = oracle.refresh_once().await.expect_err("overflow is fatal");
        assert!(matches!(
            err,
            FeeOracleError::FatalMath(MathError::Overflow)
        ));
        let storage = Storage::open_read_only(&db.path).unwrap();
        assert_eq!(storage.log_gas_price().unwrap(), 0);
    }

    #[tokio::test]
    async fn run_forever_shuts_down_cleanly() {
        let db = temp_db("fee-oracle-shutdown");
        initialize_db(&db.path);
        let oracle = FeeOracle::new_with_sources(
            db.path.clone(),
            Duration::from_millis(50),
            Box::new(StaticGas(sample_fees())),
            Box::new(StaticToken(sample_quote())),
        );
        let shutdown = ShutdownSignal::default();
        let handle = oracle.start(shutdown.clone()).expect("start");

        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.request_shutdown();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("fee oracle exits within timeout")
            .expect("join")
            .expect("fee oracle result");
    }

    #[tokio::test]
    async fn run_forever_retains_price_across_transient_failures() {
        let db = temp_db("fee-oracle-retain-loop");
        initialize_db(&db.path);
        let expected_log = expected_log_price();

        let oracle = FeeOracle::new_with_sources(
            db.path.clone(),
            Duration::from_millis(40),
            Box::new(FailsAfterFirstGas {
                calls: Mutex::new(0),
                ok: sample_fees(),
            }),
            Box::new(StaticToken(sample_quote())),
        );
        let shutdown = ShutdownSignal::default();
        let mut handle = oracle.start(shutdown.clone()).expect("start");

        // First refresh succeeds; subsequent polls are transient. The worker
        // must keep running and leave the last good price untouched.
        tokio::select! {
            biased;
            result = &mut handle => panic!("fee oracle exited early: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        let storage = Storage::open_read_only(&db.path).unwrap();
        assert_eq!(storage.log_gas_price().unwrap(), expected_log);
        drop(storage);

        shutdown.request_shutdown();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("fee oracle exits within timeout")
            .expect("join")
            .expect("fee oracle result");
    }
}
