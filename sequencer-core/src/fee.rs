// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Log-space fees, base 129/128: exponent `n` nominally means `(129/128)^n`
//! smallest token units (0 is 1 unit; no sentinel), for frame `fee_price`, user-op
//! `max_fee` and the DB `recommended_fee`. [`fee_to_linear`] owns the exact integer
//! conversion; ports must match it bit for bit, since fees decide validation and state.

use alloy_primitives::U256;

// Import the build-time generated constants: TABLE and MAX_EXPONENT.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/fee_table.rs"));
}
pub use generated::MAX_EXPONENT;
use generated::TABLE;

/// Fractional bits used in the fixed-point precomputed table.
const FRAC_BITS: usize = 64;

/// 1.0 in fixed-point representation.
const FIXED_ONE: U256 = U256::from_limbs([0, 1, 0, 0]);

/// Type alias for the 512-bit intermediate used in fixed-point multiplication.
type U512 = alloy_primitives::Uint<512, 8>;

/// Convert a log-space fee exponent to a linear [`U256`] value.
///
/// Let `S = 2^64`. `build.rs` generates `T[0] = 129 * 2^57` and
/// `T[i] = floor(T[i-1] * T[i-1] / S)` for `i = 1, …, 14`.
///
/// Start `R = S`. Visit bits `i = 0, …, 14` in ascending order; for each set bit
/// of `n`, replace `R` with `floor(R * T[i] / S)`. Return `floor(R / S)`.
/// Every multiplication widens its 256-bit operands to a 512-bit product
/// before shifting right by 64. Intermediate flooring and accumulation order
/// are part of the contract: using the same table in a different order, or
/// computing the exact rational power and flooring only once, is not a
/// compatible replacement.
///
/// # Panics
///
/// Panics for `n > MAX_EXPONENT`. The bound protects the full fixed-point
/// accumulator, including its 64 fractional bits, rather than just the final
/// integer result.
pub fn fee_to_linear(log_fee: u16) -> U256 {
    fee_to_linear_fixed(log_fee) >> FRAC_BITS
}

/// Compute the rounded fixed-point accumulator specified by [`fee_to_linear`].
fn fee_to_linear_fixed(log_fee: u16) -> U256 {
    assert!(
        log_fee <= MAX_EXPONENT,
        "fee exponent {log_fee} exceeds MAX_EXPONENT ({MAX_EXPONENT})"
    );
    let mut result = FIXED_ONE; // 1.0 in fixed-point
    for (i, entry) in TABLE.iter().enumerate() {
        if log_fee & (1 << i) != 0 {
            result = fixed_mul(result, *entry);
        }
    }
    result
}

/// Fixed-point multiplication: `(a * b) >> FRAC_BITS`.
///
/// Uses a 512-bit intermediate to avoid overflow.
#[inline]
fn fixed_mul(a: U256, b: U256) -> U256 {
    let product: U512 = a.widening_mul(b);
    let shifted = product >> FRAC_BITS;
    // Truncate to U256: the high limbs are dropped without a check. Every
    // caller's inputs are table entries or partial products for an exponent
    // already bounded by `MAX_EXPONENT` in `fee_to_linear_fixed`, and partial
    // products never exceed the final value, so the shifted product fits.
    U256::from_limbs_slice(&shifted.into_limbs()[..4])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponent_zero_is_one_unit() {
        assert_eq!(fee_to_linear(0), U256::from(1u64));
    }

    #[test]
    fn small_exponents() {
        // (129/128)^128 ≈ 2.708
        let v = fee_to_linear(128);
        assert!(v >= U256::from(2u64) && v <= U256::from(3u64));
    }

    #[test]
    fn known_values() {
        // (129/128)^1060 ≈ 3824
        let v = fee_to_linear(1060);
        assert!(
            v >= U256::from(3800u64) && v <= U256::from(3850u64),
            "fee_to_linear(1060) = {v}, expected ~3824"
        );

        // (129/128)^1000 ≈ 2397
        let v = fee_to_linear(1000);
        assert!(
            v >= U256::from(2380u64) && v <= U256::from(2420u64),
            "fee_to_linear(1000) = {v}, expected ~2397"
        );
    }

    #[test]
    fn fee_to_linear_at_max_exponent() {
        // Should not panic and should return a large value.
        let v = fee_to_linear(MAX_EXPONENT);
        assert!(v > U256::ZERO);
    }

    #[test]
    #[should_panic(expected = "exceeds MAX_EXPONENT")]
    fn fee_to_linear_above_max_exponent_panics() {
        fee_to_linear(MAX_EXPONENT + 1);
    }

    /// `build.rs` regenerates the table on every build, and the host and the
    /// guest each build their own; this pins it against the bytes generated
    /// before any ruint or toolchain move.
    #[test]
    fn generated_table_and_conversions_match_the_pinned_values() {
        let pinned = crate::consensus_vectors::vectors().fee;
        assert_eq!(MAX_EXPONENT, pinned.max_exponent);
        let limbs: Vec<[u64; 4]> = TABLE.iter().map(|entry| *entry.as_limbs()).collect();
        let pinned_limbs: Vec<[u64; 4]> = pinned
            .table
            .iter()
            .map(|entry| entry.map(|limb| limb.to::<u64>()))
            .collect();
        assert_eq!(limbs, pinned_limbs);

        for case in &pinned.fee_to_linear {
            assert_eq!(
                fee_to_linear_fixed(case.exponent),
                case.fixed,
                "exponent {}",
                case.exponent
            );
            assert_eq!(
                fee_to_linear(case.exponent),
                case.linear,
                "exponent {}",
                case.exponent
            );
        }
    }
}
