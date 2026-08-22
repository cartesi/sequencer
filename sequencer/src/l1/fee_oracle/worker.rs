// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Periodically refresh the fee-token price charged for L1 gas.

use std::time::Duration;

use alloy::providers::DynProvider;
use async_trait::async_trait;
use thiserror::Error;
use tracing::{debug, warn};

use crate::clock::unix_now_ms;
use crate::l1::eip1559::{Eip1559Fees, estimate_fees};
use crate::l1::fee_oracle::math::{MathError, compute_x_units_per_gas, encode_log_gas_price};
use crate::l1::fee_oracle::uniswap::{
    PriceSourceError, TokenPriceSource, UniswapConfig, UniswapV3PriceSource,
    bootstrap_price_source_error,
};
use crate::runtime::process_lock::{ProcessLock, spawn_blocking_with_lock};
use crate::runtime::shutdown::RuntimeScope;
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
    #[error("fee-oracle misconfiguration: {0}")]
    Misconfig(String),
    #[error("fatal fee-oracle arithmetic failure: {0}")]
    FatalMath(#[from] MathError),
}

impl FeeOracleError {
    /// Whether this error poisons the run rather than restarting. Named
    /// arms, no wildcard: a new variant must classify itself here (D1/H1).
    pub(crate) fn is_terminal_invariant(&self) -> bool {
        match self {
            Self::Storage(source) => crate::storage::is_persistent_storage_error(source),
            Self::OpenStorage(source) => crate::storage::is_persistent_storage_open_error(source),
            // The oracle's `Join` wraps its internal blocking storage task.
            // During a live worker exit that task can only have panicked;
            // ordinary shutdown cancels the enclosing refresh future instead.
            Self::FatalMath(_) | Self::Misconfig(_) | Self::Join(_) => true,
            // A transient quote/transport failure self-heals.
            Self::Transient(_) => false,
        }
    }
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

enum TokenHandle {
    Connected(Box<dyn TokenPriceSource>),
    /// Re-runs `UniswapV3PriceSource::connect` on every quote. Used when boot
    /// skipped a live connect because L1 was transiently unreachable.
    Reconnecting {
        provider: DynProvider,
        config: UniswapConfig,
    },
}

pub struct FeeOracle {
    db_path: String,
    poll_interval: Duration,
    /// Shared with L1 read-staleness: `l1_read_stale_after_secs * 1000`.
    max_price_age_ms: u64,
    gas: Box<dyn GasFeeSource>,
    token: TokenHandle,
    /// Retains data-directory exclusivity inside detached-capable blocking DB
    /// work if the async setup/runtime task awaiting it is cancelled.
    /// Required at construction (H14).
    process_lock: ProcessLock,
}

impl FeeOracle {
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(12);

    pub(super) fn new(
        db_path: impl Into<String>,
        poll_interval: Duration,
        max_price_age_ms: u64,
        provider: DynProvider,
        token: Box<dyn TokenPriceSource>,
        process_lock: ProcessLock,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            poll_interval,
            max_price_age_ms,
            gas: Box::new(provider),
            token: TokenHandle::Connected(token),
            process_lock,
        }
    }

    /// Uniswap worker that reconnects on each quote. Prefer [`Self::new`] when
    /// boot already validated the pool; use this when boot tolerated a
    /// transient connect failure and is running on a persisted price.
    pub(super) fn reconnecting_uniswap(
        db_path: impl Into<String>,
        poll_interval: Duration,
        max_price_age_ms: u64,
        provider: DynProvider,
        config: UniswapConfig,
        process_lock: ProcessLock,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            poll_interval,
            max_price_age_ms,
            gas: Box::new(provider.clone()),
            token: TokenHandle::Reconnecting { provider, config },
            process_lock,
        }
    }

    #[cfg(test)]
    fn new_with_sources(
        db_path: impl Into<String>,
        poll_interval: Duration,
        max_price_age_ms: u64,
        gas: Box<dyn GasFeeSource>,
        token: Box<dyn TokenPriceSource>,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            poll_interval,
            max_price_age_ms,
            gas,
            token: TokenHandle::Connected(token),
            process_lock: ProcessLock::test(),
        }
    }

    async fn quote_x_per_weth(&self) -> Result<alloy_primitives::U256, FeeOracleError> {
        match &self.token {
            TokenHandle::Connected(token) => token.quote_x_per_weth().await.map_err(Into::into),
            TokenHandle::Reconnecting { provider, config } => {
                let token = UniswapV3PriceSource::connect(provider.clone(), *config).await?;
                token.quote_x_per_weth().await.map_err(Into::into)
            }
        }
    }

    /// Refresh from L1 and stamp `log_gas_price_updated_at_ms` even when the
    /// encoded exponent is unchanged — the timestamp is the freshness signal.
    pub(super) async fn refresh_once(&self) -> Result<RefreshResult, FeeOracleError> {
        let fees = self
            .gas
            .estimate_gas_fees()
            .await
            .map_err(FeeOracleError::Transient)?;
        let quote = self.quote_x_per_weth().await?;
        let linear =
            compute_x_units_per_gas(fees.base_fee_per_gas, fees.max_priority_fee_per_gas, quote)?;
        let log_gas_price = encode_log_gas_price(linear)?;

        let db_path = self.db_path.clone();
        let process_lock = self.process_lock.clone();
        let refresh = spawn_blocking_with_lock(
            process_lock,
            move || -> Result<RefreshResult, FeeOracleError> {
                let mut storage = Storage::open_writer(&db_path)?;
                let changed = storage.log_gas_price()? != log_gas_price;
                // Always stamp: successful quote renews the staleness clock.
                storage.set_log_gas_price(log_gas_price)?;
                Ok(RefreshResult {
                    log_gas_price,
                    changed,
                })
            },
        )
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

    /// Refuse when the persisted price is missing or older than `max_age_ms`.
    pub(super) fn ensure_persisted_price_fresh(
        db_path: &str,
        max_age_ms: u64,
    ) -> Result<(), FeeOracleError> {
        let storage = Storage::open_read_only(db_path)?;
        let now = unix_now_ms();
        if storage.log_gas_price_is_stale(now, max_age_ms)? {
            let age = storage.log_gas_price_age_ms(now)?;
            return Err(FeeOracleError::Transient(format!(
                "persisted fee oracle price stale: age_ms={age}, max_age_ms={max_age_ms}"
            )));
        }
        Ok(())
    }

    pub(crate) fn start(
        self,
        shutdown: RuntimeScope,
    ) -> tokio::task::JoinHandle<Result<(), FeeOracleError>> {
        tokio::spawn(async move { self.run_forever(shutdown).await })
    }

    async fn run_forever(self, shutdown: RuntimeScope) -> Result<(), FeeOracleError> {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.wait_for_shutdown() => return Ok(()),
                result = self.refresh_once() => match result {
                    Ok(refresh) => {
                        if refresh.changed {
                            debug!(log_gas_price = refresh.log_gas_price, "updated L1 fee oracle price");
                        }
                    }
                    Err(FeeOracleError::Transient(error)) => {
                        let db_path = self.db_path.clone();
                        let max_age_ms = self.max_price_age_ms;
                        let process_lock = self.process_lock.clone();
                        let (age_ms, stale) = spawn_blocking_with_lock(process_lock, move || {
                            let storage = Storage::open_read_only(&db_path)?;
                            let now = unix_now_ms();
                            let age_ms = storage.log_gas_price_age_ms(now)?;
                            let stale = storage.log_gas_price_is_stale(now, max_age_ms)?;
                            Ok::<_, FeeOracleError>((age_ms, stale))
                        })
                        .await
                        .map_err(|err| FeeOracleError::Join(err.to_string()))??;
                        if stale {
                            return Err(FeeOracleError::Transient(format!(
                                "fee oracle price older than {max_age_ms}ms \
                                 (age {age_ms}ms) after transient failure: {error}"
                            )));
                        }
                        warn!(
                            %error,
                            retained_age_ms = age_ms,
                            max_age_ms,
                            "retaining last L1 fee-oracle price after transient failure"
                        );
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
        let (transient, message) = bootstrap_price_source_error(error);
        if transient {
            Self::Transient(message)
        } else {
            Self::Misconfig(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::temp_db;
    use alloy_primitives::U256;
    use std::sync::Mutex;

    const TEST_MAX_AGE_MS: u64 = 60 * 60 * 1000;

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

    struct AlwaysFailToken;

    #[async_trait]
    impl TokenPriceSource for AlwaysFailToken {
        async fn quote_x_per_weth(&self) -> Result<U256, PriceSourceError> {
            Err(PriceSourceError::Provider("pool unavailable".into()))
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

    fn oracle_with(
        path: &str,
        max_age_ms: u64,
        gas: Box<dyn GasFeeSource>,
        token: Box<dyn TokenPriceSource>,
    ) -> FeeOracle {
        FeeOracle::new_with_sources(
            path.to_owned(),
            Duration::from_secs(1),
            max_age_ms,
            gas,
            token,
        )
    }

    #[tokio::test]
    async fn live_source_updates_only_when_encoded_price_changes() {
        let db = temp_db("live-fee-oracle");
        initialize_db(&db.path);
        let expected_log = expected_log_price();
        let oracle = oracle_with(
            &db.path,
            TEST_MAX_AGE_MS,
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
        let first_stamp = Storage::open_read_only(&db.path)
            .unwrap()
            .log_gas_price_updated_at_ms()
            .unwrap();
        assert!(first_stamp > 0);

        // Unchanged exponent still renews the freshness stamp.
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert_eq!(
            oracle.refresh_once().await.unwrap(),
            RefreshResult {
                log_gas_price: expected_log,
                changed: false
            }
        );
        let storage = Storage::open_read_only(&db.path).unwrap();
        assert_eq!(storage.log_gas_price().unwrap(), expected_log);
        assert!(storage.log_gas_price_updated_at_ms().unwrap() >= first_stamp);
    }

    #[tokio::test]
    async fn transient_gas_failure_retains_previous_price() {
        let db = temp_db("retain-gas-fee-oracle");
        initialize_db(&db.path);
        let expected_log = expected_log_price();
        let oracle = oracle_with(
            &db.path,
            TEST_MAX_AGE_MS,
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
        let oracle = oracle_with(
            &db.path,
            TEST_MAX_AGE_MS,
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
        let storage = Storage::open(&db.path).unwrap();
        assert_eq!(storage.log_gas_price().unwrap(), 0);
        drop(storage);

        let oracle = oracle_with(
            &db.path,
            TEST_MAX_AGE_MS,
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
            TEST_MAX_AGE_MS,
            Box::new(StaticGas(sample_fees())),
            Box::new(StaticToken(sample_quote())),
        );
        let shutdown = RuntimeScope::default();
        let handle = oracle.start(shutdown.clone());

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
            TEST_MAX_AGE_MS,
            Box::new(FailsAfterFirstGas {
                calls: Mutex::new(0),
                ok: sample_fees(),
            }),
            Box::new(StaticToken(sample_quote())),
        );
        let shutdown = RuntimeScope::default();
        let mut handle = oracle.start(shutdown.clone());

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

    #[tokio::test]
    async fn run_forever_exits_when_persisted_price_is_stale() {
        let db = temp_db("fee-oracle-stale-exit");
        initialize_db(&db.path);
        {
            let mut storage = Storage::open_writer(&db.path).unwrap();
            storage.set_log_gas_price(42).unwrap();
            // Force an ancient stamp so the next transient fails the bound.
            storage.set_log_gas_price_updated_at_ms_for_test(1).unwrap();
        }

        let oracle = FeeOracle::new_with_sources(
            db.path.clone(),
            Duration::from_millis(20),
            100, // 100ms max age
            Box::new(StaticGas(sample_fees())),
            Box::new(AlwaysFailToken),
        );
        let shutdown = RuntimeScope::default();
        let handle = oracle.start(shutdown);

        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("fee oracle exits within timeout")
            .expect("join");
        let err = result.expect_err("stale price must stop the worker");
        assert!(matches!(err, FeeOracleError::Transient(ref m) if m.contains("older than")));
    }

    #[test]
    fn ensure_persisted_price_fresh_rejects_never_written() {
        let db = temp_db("fee-oracle-never-written");
        initialize_db(&db.path);
        let err = FeeOracle::ensure_persisted_price_fresh(&db.path, TEST_MAX_AGE_MS)
            .expect_err("default stamp 0 is stale");
        assert!(matches!(err, FeeOracleError::Transient(_)));
    }

    #[test]
    fn ensure_persisted_price_fresh_accepts_recent_write() {
        let db = temp_db("fee-oracle-fresh-write");
        initialize_db(&db.path);
        Storage::open_writer(&db.path)
            .unwrap()
            .set_log_gas_price(7)
            .unwrap();
        FeeOracle::ensure_persisted_price_fresh(&db.path, TEST_MAX_AGE_MS).unwrap();
    }
}
