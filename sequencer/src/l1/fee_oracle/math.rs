// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Integer-only conversion from L1 gas prices to fee-token gas prices.

use alloy_primitives::U256;
use sequencer_core::fee::{MAX_EXPONENT, fee_from_linear, fee_to_linear};

/// Encode a linear price without ever rounding it down.
pub fn encode_log_gas_price(linear: U256) -> u16 {
    let exponent = fee_from_linear(linear);
    if fee_to_linear(exponent) >= linear || exponent == MAX_EXPONENT {
        exponent
    } else {
        exponent + 1
    }
}

/// Compute fee-token smallest units charged per L1 gas.
///
/// `quote_x_per_weth` is the number of fee-token smallest units for 1 WETH.
/// The multiplication is checked and rounded up, deliberately applying slack
/// before the log encoding.
pub fn compute_x_units_per_gas(
    base_fee: u128,
    priority_fee: u128,
    quote_x_per_weth: U256,
    slack_multiplier: u64,
) -> Result<U256, MathError> {
    let gas_price = U256::from(base_fee)
        .checked_add(U256::from(priority_fee))
        .ok_or(MathError::Overflow)?;
    let numerator = gas_price
        .checked_mul(quote_x_per_weth)
        .and_then(|value| value.checked_mul(U256::from(slack_multiplier)))
        .ok_or(MathError::Overflow)?;
    ceil_div(numerator, U256::from(1_000_000_000_000_000_000u128))
}

pub fn ceil_div(numerator: U256, denominator: U256) -> Result<U256, MathError> {
    if denominator.is_zero() {
        return Err(MathError::DivisionByZero);
    }
    let quotient = numerator / denominator;
    Ok(if numerator % denominator == U256::ZERO {
        quotient
    } else {
        quotient
            .checked_add(U256::from(1))
            .ok_or(MathError::Overflow)?
    })
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MathError {
    #[error("integer arithmetic overflow")]
    Overflow,
    #[error("division by zero")]
    DivisionByZero,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceil_division_rounds_up() {
        assert_eq!(
            ceil_div(U256::from(10), U256::from(3)).unwrap(),
            U256::from(4)
        );
        assert_eq!(
            ceil_div(U256::from(9), U256::from(3)).unwrap(),
            U256::from(3)
        );
    }

    #[test]
    fn gas_price_charges_base_plus_priority_with_ten_x_slack() {
        // ETH ~= $1800, USDC has 6 decimals, and a 20 gwei paid gas price:
        // 20e9 wei * 1.8e9 USDC-smallest/WETH * 10 / 1e18 = 360 USDC units.
        assert_eq!(
            compute_x_units_per_gas(19_000_000_000, 1_000_000_000, U256::from(1_800_000_000), 10)
                .unwrap(),
            U256::from(360)
        );
    }

    #[test]
    fn detects_multiplication_overflow() {
        assert_eq!(
            compute_x_units_per_gas(u128::MAX, u128::MAX, U256::MAX, 10),
            Err(MathError::Overflow)
        );
    }

    #[test]
    fn conservative_log_encoding_never_undercharges() {
        let target = U256::from(123_456_789u64);
        let encoded = encode_log_gas_price(target);
        assert!(fee_to_linear(encoded) >= target);
        assert!(encoded == 0 || fee_to_linear(encoded - 1) < target);
    }

    #[test]
    fn ten_x_is_applied_before_log_encoding() {
        let before =
            compute_x_units_per_gas(1, 0, U256::from(100_000_000_000_000_000u128), 10).unwrap();
        assert_eq!(before, U256::from(1));
        assert_eq!(encode_log_gas_price(before), 0);
    }

    #[test]
    fn ceil_div_rejects_zero_denominator() {
        assert_eq!(
            ceil_div(U256::from(1), U256::ZERO),
            Err(MathError::DivisionByZero)
        );
    }

    #[test]
    fn encode_zero_and_saturate_at_max_exponent() {
        assert_eq!(encode_log_gas_price(U256::ZERO), 0);
        assert_eq!(encode_log_gas_price(U256::MAX), MAX_EXPONENT);
        assert!(fee_to_linear(encode_log_gas_price(U256::MAX)) <= fee_to_linear(MAX_EXPONENT));
    }

    #[test]
    fn compute_rounds_up_fractional_wei_quote() {
        // 1 wei * 1 quote-unit * 10 = 10 — strictly less than 1e18, so ceil → 1.
        assert_eq!(
            compute_x_units_per_gas(1, 0, U256::from(1), 10).unwrap(),
            U256::from(1)
        );
        // Exact multiple of 1e18 stays exact (no round-up).
        assert_eq!(
            compute_x_units_per_gas(1, 0, U256::from(1_000_000_000_000_000_000u128), 10).unwrap(),
            U256::from(10)
        );
        // One extra quote unit pushes numerator over the next 1e18 boundary.
        assert_eq!(
            compute_x_units_per_gas(1, 0, U256::from(1_000_000_000_000_000_001u128), 10).unwrap(),
            U256::from(11)
        );
    }
}
