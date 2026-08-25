// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Read-only Uniswap V3 WETH/fee-token TWAP source.

use alloy::contract::Error as ContractError;
use alloy::providers::{DynProvider, Provider};
use alloy::sol;
use alloy_primitives::{Address, U256, Uint};
use alloy_sol_types::Revert;
use async_trait::async_trait;
use thiserror::Error;

sol! {
    #[sol(rpc)]
    interface IUniswapV3Pool {
        function observe(uint32[] calldata secondsAgos)
            external view returns (int56[] memory tickCumulatives, uint160[] memory secondsPerLiquidityCumulativeX128s);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }
}

pub const MAINNET_WETH: Address =
    alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
pub const MAINNET_USDC: Address =
    alloy::primitives::address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
pub const MAINNET_USDC_WETH_005_POOL: Address =
    alloy::primitives::address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
pub const SEPOLIA_WETH: Address =
    alloy::primitives::address!("ffF9976782d46CC05630D1f6eBAb18b2324d6B14");
pub const SEPOLIA_USDC: Address =
    alloy::primitives::address!("1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
/// Provisional community-deployed Sepolia WETH/USDC 0.05% pool; review before production use.
pub const SEPOLIA_USDC_WETH_005_POOL: Address =
    alloy::primitives::address!("6Ce0896eAE6D4BD668fDe41BB784548fb8F59b50");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniswapConfig {
    pub chain_id: u64,
    pub weth: Address,
    pub fee_token: Address,
    pub pool: Address,
    pub twap_window_secs: u32,
}

impl UniswapConfig {
    pub const DEFAULT_TWAP_WINDOW_SECS: u32 = 1_800;
}

#[async_trait]
pub trait TokenPriceSource: Send + Sync {
    /// Fee-token smallest units quoted for one WETH (10^18 wei).
    async fn quote_x_per_weth(&self) -> Result<U256, PriceSourceError>;
}

/// Read-only Uniswap V3 source. Validation must complete before use.
pub struct UniswapV3PriceSource {
    provider: DynProvider,
    config: UniswapConfig,
    weth_is_token0: bool,
}

#[derive(Debug, Error)]
pub enum PriceSourceError {
    #[error("provider: {0}")]
    Provider(String),
    #[error("RPC chain id {rpc} does not match configured chain id {expected}")]
    ChainIdMismatch { rpc: u64, expected: u64 },
    #[error("configured pool has no code: {0}")]
    MissingPoolCode(Address),
    #[error("pool token pair is not configured WETH/fee token")]
    WrongTokenPair,
    #[error("pool has insufficient observations for the configured TWAP window")]
    InsufficientObservations,
    #[error("invalid TWAP window of zero seconds")]
    ZeroTwapWindow,
    #[error("TWAP arithmetic overflow")]
    ArithmeticOverflow,
}

impl UniswapV3PriceSource {
    pub async fn connect(
        provider: DynProvider,
        config: UniswapConfig,
    ) -> Result<Self, PriceSourceError> {
        if config.twap_window_secs == 0 {
            return Err(PriceSourceError::ZeroTwapWindow);
        }
        let rpc_chain_id = provider
            .get_chain_id()
            .await
            .map_err(|err| PriceSourceError::Provider(err.to_string()))?;
        if rpc_chain_id != config.chain_id {
            return Err(PriceSourceError::ChainIdMismatch {
                rpc: rpc_chain_id,
                expected: config.chain_id,
            });
        }
        let code = provider
            .get_code_at(config.pool)
            .await
            .map_err(|err| PriceSourceError::Provider(err.to_string()))?;
        if code.is_empty() {
            return Err(PriceSourceError::MissingPoolCode(config.pool));
        }

        let pool = IUniswapV3Pool::new(config.pool, &provider);
        let token0 = pool.token0().call().await.map_err(provider_error)?;
        let token1 = pool.token1().call().await.map_err(provider_error)?;
        let weth_is_token0 = if token0 == config.weth && token1 == config.fee_token {
            true
        } else if token1 == config.weth && token0 == config.fee_token {
            false
        } else {
            return Err(PriceSourceError::WrongTokenPair);
        };
        // Uniswap V3 pools canonically order token0/token1 by address. Enforce
        // that setup validated a real pool with the same ordering runtime later
        // derives without another RPC round-trip.
        if weth_is_token0 != (config.weth < config.fee_token) {
            return Err(PriceSourceError::WrongTokenPair);
        }
        // Probe the exact `observe` call used for quotes. Uniswap's `OLD`
        // revert means this TWAP window cannot currently be served. Transport
        // and `OLD` are availability failures; the remaining validation errors
        // are deterministic configuration failures.
        pool.observe(vec![config.twap_window_secs, 0])
            .call()
            .await
            .map_err(classify_observe_error)?;

        Ok(Self {
            provider,
            config,
            weth_is_token0,
        })
    }

    /// Construct the runtime source from setup-pinned, setup-validated
    /// identity without touching L1. Uniswap V3's canonical address ordering
    /// supplies the only fact `connect` discovered that is needed to quote;
    /// the input reader independently verifies the pinned chain on contact.
    pub(crate) fn from_setup_validated(provider: DynProvider, config: UniswapConfig) -> Self {
        Self {
            provider,
            config,
            weth_is_token0: config.weth < config.fee_token,
        }
    }

    async fn mean_tick(&self) -> Result<i32, PriceSourceError> {
        let pool = IUniswapV3Pool::new(self.config.pool, &self.provider);
        let observed = pool
            .observe(vec![self.config.twap_window_secs, 0])
            .call()
            .await
            .map_err(classify_observe_error)?;
        let start: i64 = observed.tickCumulatives[0]
            .try_into()
            .expect("Uniswap int56 tick cumulative fits i64");
        let end: i64 = observed.tickCumulatives[1]
            .try_into()
            .expect("Uniswap int56 tick cumulative fits i64");
        let delta = end
            .checked_sub(start)
            .expect("difference of two int56 values fits i64");
        let window = i64::from(self.config.twap_window_secs);
        // Solidity integer division truncates toward zero; Uniswap rounds a
        // negative remainder down to preserve its canonical oracle semantics.
        let mut tick = delta / window;
        if delta < 0 && delta % window != 0 {
            tick -= 1;
        }
        Ok(i32::try_from(tick).expect("mean tick is within the Uniswap int24 range"))
    }
}

#[async_trait]
impl TokenPriceSource for UniswapV3PriceSource {
    async fn quote_x_per_weth(&self) -> Result<U256, PriceSourceError> {
        let tick = self.mean_tick().await?;
        quote_x_per_weth_from_tick(tick, self.weth_is_token0)
    }
}

fn provider_error(error: impl std::fmt::Display) -> PriceSourceError {
    PriceSourceError::Provider(error.to_string())
}

/// Exact Uniswap V3 Oracle reason for an `observe` window that exceeds history.
/// Compare decoded `Error(string)` payloads only — never substring-match
/// `Display` text (`"THRESHOLD"` contains `"OLD"`).
fn is_old_observation_reason(reason: &str) -> bool {
    reason == "OLD"
}

/// Classify an `observe` failure from revert data: decode `Error(string)` and
/// require an exact `"OLD"` reason. Transport failures and unrelated reverts
/// (including messages that merely *contain* the letters OLD) stay provider
/// errors.
fn classify_observe_error(error: ContractError) -> PriceSourceError {
    classify_decoded_observe_revert(
        error.as_decoded_error::<Revert>().as_ref(),
        error.to_string(),
    )
}

fn classify_decoded_observe_revert(
    revert: Option<&Revert>,
    fallback_message: String,
) -> PriceSourceError {
    match revert {
        Some(revert) if is_old_observation_reason(&revert.reason) => {
            PriceSourceError::InsufficientObservations
        }
        _ => PriceSourceError::Provider(fallback_message),
    }
}

/// Setup-time mapping: provider/RPC failures and an unavailable observation
/// window may self-heal. Both still fail setup's hard first-read requirement;
/// only their restart classification differs from deterministic misconfig.
pub(super) fn bootstrap_price_source_error(error: PriceSourceError) -> (bool, String) {
    match error {
        PriceSourceError::Provider(message) => (true, message),
        error @ PriceSourceError::InsufficientObservations => (true, error.to_string()),
        error => (false, error.to_string()),
    }
}

/// Convert a Uniswap tick to fee-token smallest units per WETH.
pub fn quote_x_per_weth_from_tick(
    tick: i32,
    weth_is_token0: bool,
) -> Result<U256, PriceSourceError> {
    let sqrt_price_x96 = sqrt_ratio_at_tick(tick)?;
    let square: Uint<512, 8> = sqrt_price_x96.widening_mul(sqrt_price_x96);
    let q192: Uint<512, 8> = Uint::from(1u8) << 192;
    let one_weth = Uint::<512, 8>::from(1_000_000_000_000_000_000u128);
    let (numerator, denominator) = if weth_is_token0 {
        (square * one_weth, q192)
    } else {
        (q192 * one_weth, square)
    };
    ceil_div_512(numerator, denominator)
}

fn sqrt_ratio_at_tick(tick: i32) -> Result<U256, PriceSourceError> {
    const MAX_TICK: i32 = 887_272;
    if tick.unsigned_abs() > MAX_TICK as u32 {
        return Err(PriceSourceError::ArithmeticOverflow);
    }
    let constants = [
        "fffcb933bd6fad37aa2d162d1a594001",
        "fff97272373d413259a46990580e213a",
        "fff2e50f5f656932ef12357cf3c7fdcc",
        "ffe5caca7e10e4e61c3624eaa0941cd0",
        "ffcb9843d60f6159c9db58835c926644",
        "ff973b41fa98c081472e6896dfb254c0",
        "ff2ea16466c96a3843ec78b326b52861",
        "fe5dee046a99a2a811c461f1969c3053",
        "fcbe86c7900a88aedcffc83b479aa3a4",
        "f987a7253ac413176f2b074cf7815e54",
        "f3392b0822b70005940c7a398e4b70f3",
        "e7159475a2c29b7443b29c7fa6e889d9",
        "d097f3bdfd2022b8845ad8f792aa5825",
        "a9f746462d870fdf8a65dc1f90e061e5",
        "70d869a156d2a1b890bb3df62baf32f7",
        "31be135f97d08fd981231505542fcfa6",
        "9aa508b5b7a84e1c677de54f3e99bc9",
        "5d6af8dedb81196699c329225ee604",
        "2216e584f5fa1ea926041bedfe98",
        "48a170391f7dc42444e8fa2",
    ];
    let abs = tick.unsigned_abs();
    let mut ratio = if abs & 1 != 0 {
        parse_u256(constants[0])?
    } else {
        U256::from_limbs([0, 0, 1, 0])
    };
    for (bit, constant) in constants.iter().enumerate().skip(1) {
        if abs & (1 << bit) != 0 {
            ratio = mul_shift_128(ratio, parse_u256(constant)?)?;
        }
    }
    if tick > 0 {
        ratio = U256::MAX / ratio;
    }
    // Q128.128 to Q64.96, rounding up as canonical TickMath does.
    let shifted = ratio >> 32;
    Ok(if ratio & U256::from((1u64 << 32) - 1) == U256::ZERO {
        shifted
    } else {
        shifted
            .checked_add(U256::from(1))
            .ok_or(PriceSourceError::ArithmeticOverflow)?
    })
}

fn parse_u256(value: &str) -> Result<U256, PriceSourceError> {
    U256::from_str_radix(value, 16).map_err(|_| PriceSourceError::ArithmeticOverflow)
}

fn mul_shift_128(left: U256, right: U256) -> Result<U256, PriceSourceError> {
    let shifted: Uint<512, 8> = left.widening_mul(right) >> 128;
    let limbs = shifted.into_limbs();
    if limbs[4..].iter().any(|limb| *limb != 0) {
        return Err(PriceSourceError::ArithmeticOverflow);
    }
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

fn ceil_div_512(
    numerator: Uint<512, 8>,
    denominator: Uint<512, 8>,
) -> Result<U256, PriceSourceError> {
    if denominator == Uint::ZERO {
        return Err(PriceSourceError::ArithmeticOverflow);
    }
    let quotient = numerator / denominator;
    let rounded = if numerator % denominator == Uint::ZERO {
        quotient
    } else {
        quotient + Uint::from(1u8)
    };
    let limbs = rounded.into_limbs();
    if limbs[4..].iter().any(|limb| *limb != 0) {
        return Err(PriceSourceError::ArithmeticOverflow);
    }
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_zero_quotes_one_weth_in_each_direction() {
        assert_eq!(
            quote_x_per_weth_from_tick(0, true).unwrap(),
            U256::from(1_000_000_000_000_000_000u128)
        );
        assert_eq!(
            quote_x_per_weth_from_tick(0, false).unwrap(),
            U256::from(1_000_000_000_000_000_000u128)
        );
    }

    #[test]
    fn negative_tick_uses_uniswap_rounding_direction() {
        // The exact ratio is below one, so WETH as token1 (inverse quote) is
        // above one WETH unit; this also exercises the negative TickMath path.
        assert!(
            quote_x_per_weth_from_tick(-1, false).unwrap()
                > U256::from(1_000_000_000_000_000_000u128)
        );
    }

    #[test]
    fn pinned_usdc_pool_constants_are_distinct_pairs() {
        assert_ne!(MAINNET_WETH, MAINNET_USDC);
        assert_ne!(SEPOLIA_WETH, SEPOLIA_USDC);
        assert_ne!(MAINNET_USDC_WETH_005_POOL, SEPOLIA_USDC_WETH_005_POOL);
    }

    #[test]
    fn out_of_range_ticks_overflow() {
        assert!(matches!(
            quote_x_per_weth_from_tick(887_273, true),
            Err(PriceSourceError::ArithmeticOverflow)
        ));
        assert!(matches!(
            quote_x_per_weth_from_tick(-887_273, false),
            Err(PriceSourceError::ArithmeticOverflow)
        ));
    }

    #[test]
    fn old_observation_reason_is_exact_match() {
        assert!(is_old_observation_reason("OLD"));
        // Substring false positives that Display matching would hit.
        assert!(!is_old_observation_reason("THRESHOLD"));
        assert!(!is_old_observation_reason("BALANCE_THRESHOLD_EXCEEDED"));
        assert!(!is_old_observation_reason("UPSTREAM_THRESHOLD"));
        assert!(!is_old_observation_reason("execution reverted: OLD"));
    }

    #[test]
    fn observe_old_revert_is_insufficient_observations() {
        let revert = Revert::from("OLD");
        assert!(matches!(
            classify_decoded_observe_revert(Some(&revert), "unused".into()),
            PriceSourceError::InsufficientObservations
        ));
    }

    #[test]
    fn observe_threshold_revert_is_provider_error() {
        // `"THRESHOLD"` contains `"OLD"` as a substring — must not misclassify.
        let revert = Revert::from("THRESHOLD");
        assert!(matches!(
            classify_decoded_observe_revert(Some(&revert), "gateway THRESHOLD".into()),
            PriceSourceError::Provider(_)
        ));
        assert!(matches!(
            classify_decoded_observe_revert(
                None,
                "error code -32000: BALANCE_THRESHOLD_EXCEEDED".into()
            ),
            PriceSourceError::Provider(_)
        ));
    }

    #[test]
    fn observe_transport_failure_is_provider_error() {
        assert!(matches!(
            classify_decoded_observe_revert(
                None,
                "error sending request for url (http://127.0.0.1:8545/)".into()
            ),
            PriceSourceError::Provider(_)
        ));
    }

    #[test]
    fn bootstrap_maps_source_availability_as_transient() {
        let (transient, _) =
            bootstrap_price_source_error(PriceSourceError::Provider("timeout".into()));
        assert!(transient);
        let (transient, _) =
            bootstrap_price_source_error(PriceSourceError::InsufficientObservations);
        assert!(transient);
        let (transient, _) = bootstrap_price_source_error(PriceSourceError::WrongTokenPair);
        assert!(!transient);
    }

    #[test]
    fn runtime_source_derives_canonical_token_order_without_rpc() {
        let provider = crate::l1::provider::create_provider("http://127.0.0.1:1", false)
            .expect("provider construction is local");
        let source = UniswapV3PriceSource::from_setup_validated(
            provider,
            UniswapConfig {
                chain_id: 1,
                weth: MAINNET_WETH,
                fee_token: MAINNET_USDC,
                pool: MAINNET_USDC_WETH_005_POOL,
                twap_window_secs: UniswapConfig::DEFAULT_TWAP_WINDOW_SECS,
            },
        );
        assert_eq!(source.weth_is_token0, MAINNET_WETH < MAINNET_USDC);
    }

    #[test]
    fn boundary_ticks_are_representable() {
        assert!(quote_x_per_weth_from_tick(887_272, true).is_ok());
        assert!(quote_x_per_weth_from_tick(-887_272, false).is_ok());
    }

    #[test]
    fn positive_tick_directionality_matches_token_order() {
        let one_weth = U256::from(1_000_000_000_000_000_000u128);
        assert!(quote_x_per_weth_from_tick(1, true).unwrap() > one_weth);
        assert!(quote_x_per_weth_from_tick(1, false).unwrap() < one_weth);
    }

    #[test]
    fn tick_math_matches_uniswap_canonical_sqrt_ratios() {
        // Canonical TickMath.getSqrtRatioAtTick vectors from Uniswap v3-core.
        assert_eq!(
            sqrt_ratio_at_tick(0).unwrap(),
            U256::from(79_228_162_514_264_337_593_543_950_336u128)
        );
        assert_eq!(
            sqrt_ratio_at_tick(-887_272).unwrap(),
            U256::from(4_295_128_739u64)
        );
        assert_eq!(
            sqrt_ratio_at_tick(887_272).unwrap(),
            U256::from_str_radix("1461446703485210103287273052203988822378723970342", 10).unwrap()
        );
    }
}
