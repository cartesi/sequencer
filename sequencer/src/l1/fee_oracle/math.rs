// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Host-side conversion from an L1 gas price and a Uniswap TWAP tick to the
//! fee-token gas-price exponent. This is approximate f64 arithmetic: only the
//! published frame fee is consensus, and `fee_to_linear` converts it exactly.

use sequencer_core::fee::MAX_EXPONENT;

/// Fee-token smallest units per L1 gas. `directed_tick` orients the TWAP so
/// `1.0001^directed_tick` is fee-token smallest units per wei. Pricing slack
/// is applied by `batch_policy.log_slack`, so this stays the quoted gas cost.
pub fn x_units_per_gas(gas_price_wei: f64, directed_tick: i32) -> f64 {
    gas_price_wei * 1.0001_f64.powi(directed_tick)
}

/// Round a linear price up to a base-129/128 exponent, clamped to
/// `[floor, MAX_EXPONENT]` (see `Storage::log_gas_price_floor`). Near an
/// exponent boundary the f64 ceil can land one step (0.78%) low; `log_slack`
/// absorbs it.
pub fn log_gas_price_ceil(x_units_per_gas: f64, floor: i16) -> i16 {
    assert!(
        x_units_per_gas >= 0.0,
        "gas price {x_units_per_gas} is negative or NaN"
    );
    let exponent = (x_units_per_gas.ln() / (129.0_f64 / 128.0).ln()).ceil();
    // ln(0) is -inf, which the clamp maps to `floor`.
    exponent.clamp(f64::from(floor), f64::from(MAX_EXPONENT)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: f64 = 129.0 / 128.0;

    #[test]
    fn x_units_per_gas_matches_reference_quotes() {
        // Per 1e18 wei: ~2063 USDC (6 decimals), ~2.718 of an 18-decimal token, the largest quote.
        for (directed_tick, expected) in [
            (-200_000, 2_063_215_670.0),
            (10_000, 2.718_145_926_825_225e18),
            (887_272, 3.402_567_868_363_881e56),
        ] {
            let quote = x_units_per_gas(1e18, directed_tick);
            assert!(
                (quote - expected).abs() <= expected * 1e-8,
                "tick {directed_tick}: {quote} vs {expected}"
            );
        }
        assert!(x_units_per_gas(1.0, -887_272) > 0.0);
    }

    #[test]
    fn log_gas_price_is_the_next_exponent_up_within_tolerance() {
        for e in [1i32, 100, 1356, 10_000] {
            let encoded = i32::from(log_gas_price_ceil(BASE.powi(e), i16::MIN));
            assert!(
                (encoded - e).abs() <= 1,
                "exponent {e} encoded as {encoded}"
            );
        }
        for v in [1.5, 36.0, 1e6, 123_456_789.0] {
            let e = i32::from(log_gas_price_ceil(v, i16::MIN));
            assert!(BASE.powi(e) >= v * (1.0 - 1e-9));
            assert!(BASE.powi(e - 1) < v * (1.0 + 1e-9));
        }
    }

    #[test]
    fn sub_two_unit_price_rounds_in_log_space_not_to_whole_units() {
        // The removed whole-unit ceil charged 2 units (exponent 90) for 1.25.
        assert_eq!(log_gas_price_ceil(1.25, i16::MIN), 29);
    }

    #[test]
    fn prices_below_one_unit_go_negative_down_to_the_floor() {
        assert_eq!(log_gas_price_ceil(0.5, -1356), -89);
        assert_eq!(log_gas_price_ceil(1.0, -1356), 0);
        assert_eq!(log_gas_price_ceil(0.0, -1356), -1356);
        assert_eq!(log_gas_price_ceil(1e-300, -1356), -1356);
    }

    #[test]
    fn prices_above_the_representable_range_saturate() {
        let max = i16::try_from(MAX_EXPONENT).expect("MAX_EXPONENT fits i16");
        assert_eq!(log_gas_price_ceil(f64::MAX, i16::MIN), max);
        assert_eq!(
            log_gas_price_ceil(x_units_per_gas(2.0 * u128::MAX as f64, 887_272), i16::MIN),
            max
        );
    }

    #[test]
    #[should_panic(expected = "negative or NaN")]
    fn nan_price_fails_loud() {
        log_gas_price_ceil(f64::NAN, i16::MIN);
    }
}
