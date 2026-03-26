// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Log-space fee encoding with base 129/128 = 1 + 2⁻⁷.
//!
//! All fees in the protocol (frame `fee_price`, user-op `max_fee`, DB `recommended_fee`)
//! are represented as **log-space exponents** with base 129/128.
//!
//! An exponent `n` represents a linear value of `(129/128)^n` smallest-token-units.
//! Exponent 0 = 1 unit (minimum, effectively free). There is no special sentinel.
//!
//! This encoding:
//! - Fits any token denomination in a u16 (range up to ~10⁷⁷)
//! - Eliminates integer overflow in the DB (fee derivation becomes pure addition)
//! - Compresses fees to 2 bytes on the wire
//! - Gives ~0.78% precision per step
//! - Uses **no floating-point arithmetic** — all conversions are pure integer ops
//!
//! The key trick: multiplying by 129/128 in integer math is `x + (x >> 7)`.
//! Exponentiation uses a precomputed table of 15 entries with binary
//! exponentiation (at most 15 fixed-point multiplications).
//!
//! The precomputed table and [`MAX_EXPONENT`] are generated at build time by
//! `build.rs` using exact integer arithmetic (iterated fixed-point squaring).
//! Any reimplementation (e.g. in C++) must use the same table values to
//! guarantee bit-identical fee calculations. See `build.rs` for the algorithm.

use alloy_primitives::U256;

// Import the build-time generated constants: TABLE and MAX_EXPONENT.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/fee_table.rs"));
}
use generated::{MAX_EXPONENT, TABLE};

/// Fee base numerator.
pub const FEE_BASE_NUM: u64 = 129;

/// Fee base denominator.
pub const FEE_BASE_DENOM: u64 = 128;

/// Fractional bits used in the fixed-point precomputed table.
const FRAC_BITS: usize = 64;

/// 1.0 in fixed-point representation.
const FIXED_ONE: U256 = U256::from_limbs([0, 1, 0, 0]);

/// Type alias for the 512-bit intermediate used in fixed-point multiplication.
type U512 = alloy_primitives::Uint<512, 8>;

/// Convert a log-space fee exponent to a linear [`U256`] value.
///
/// `fee_to_linear(n)` = `floor((129/128)^n)`.
///
/// Uses a precomputed table with binary exponentiation: at most 15 fixed-point
/// multiplications, no floats.
///
/// # Panics
///
/// Panics if the result would overflow `U256` (exponent > [`MAX_EXPONENT`]).
pub fn fee_to_linear(log_fee: u16) -> U256 {
    fee_to_linear_fixed(log_fee) >> FRAC_BITS
}

/// Compute `(129/128)^n` in fixed-point representation (64 fractional bits).
///
/// Used internally for higher-precision comparisons in binary search.
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

/// Convert a linear fee value to the nearest log-space exponent.
///
/// `fee_from_linear(v)` = `round(log_{129/128}(v))`.
///
/// Returns 0 for `value <= 1` (since `(129/128)^0 = 1`).
///
/// Uses binary search over `fee_to_linear` — 15 iterations, each doing at
/// most 15 multiplies = 225 multiplies total. Fine for the rare inverse path.
///
/// Accepts [`U256`] for symmetry with [`fee_to_linear`], which returns [`U256`].
pub fn fee_from_linear(value: U256) -> u16 {
    if value <= U256::from(1u64) {
        return 0;
    }
    // Values at or above the maximum representable fee saturate to MAX_EXPONENT.
    // This also prevents overflow in the left-shift below (which requires the
    // upper 64 bits of `value` to be zero).
    if value >= fee_to_linear(MAX_EXPONENT) {
        return MAX_EXPONENT;
    }
    // Compare in fixed-point for full precision.
    let target = value << FRAC_BITS;
    // Binary search: find smallest n where fee_to_linear_fixed(n) >= target.
    let mut lo: u32 = 0;
    let mut hi: u32 = MAX_EXPONENT as u32 + 1;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if fee_to_linear_fixed(mid as u16) < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    // Check whether lo or lo-1 is closer.
    if lo == 0 {
        return 0;
    }
    let above = fee_to_linear_fixed(lo as u16);
    let below = fee_to_linear_fixed((lo - 1) as u16);
    if target - below <= above - target {
        (lo - 1) as u16
    } else {
        lo as u16
    }
}

/// Compute `round(log_{129/128}(num / denom))` as a signed integer.
///
/// Used by `set_alpha` to convert a rational value (like α = num/denom) to a
/// log-space exponent. Returns negative values when `num < denom`.
///
/// All arithmetic is integer-only (no floats).
///
/// # Panics
///
/// Panics if `num == 0` or `denom == 0`.
pub fn log_fee_ratio(num: u64, denom: u64) -> i32 {
    assert!(num > 0 && denom > 0, "log_fee_ratio requires positive values");
    if num >= denom {
        // log(num/denom) is non-negative. Binary search for n where
        // fee_to_linear_fixed(n) * denom >= num * FIXED_ONE.
        let target = U256::from(num) << FRAC_BITS;
        let scale = U256::from(denom);
        let mut lo: u32 = 0;
        let mut hi: u32 = MAX_EXPONENT as u32 + 1;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if fee_to_linear_fixed(mid as u16) * scale < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // Pick whichever of lo-1 or lo is closer.
        if lo == 0 {
            return 0;
        }
        let above = fee_to_linear_fixed(lo as u16) * scale;
        let below = fee_to_linear_fixed((lo - 1) as u16) * scale;
        if target - below <= above - target {
            (lo - 1) as i32
        } else {
            lo as i32
        }
    } else {
        // num < denom: log(num/denom) = -log(denom/num).
        -log_fee_ratio(denom, num)
    }
}

/// Fixed-point multiplication: `(a * b) >> FRAC_BITS`.
///
/// Uses a 512-bit intermediate to avoid overflow.
#[inline]
fn fixed_mul(a: U256, b: U256) -> U256 {
    let product: U512 = a.widening_mul(b);
    let shifted = product >> FRAC_BITS;
    // Truncate to U256 — panics via debug_assert if the result overflows.
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
    fn round_trip() {
        for original in [1u64, 10, 100, 1_000, 1_000_000, 1_000_000_000] {
            let original_u256 = U256::from(original);
            let exp = fee_from_linear(original_u256);
            let recovered = fee_to_linear(exp);
            // Within ~1% of original (0.78% per step, rounding can add half a step).
            let lo = original_u256 * U256::from(99u64) / U256::from(100u64);
            let hi = original_u256 * U256::from(101u64) / U256::from(100u64);
            assert!(
                recovered >= lo && recovered <= hi,
                "round-trip failed for {original}: exp={exp}, recovered={recovered}"
            );
        }
    }

    #[test]
    fn log_fee_ratio_positive() {
        // log_{129/128}(26) ≈ 419
        let v = log_fee_ratio(26, 1);
        assert!((417..=421).contains(&v), "log(26) = {v}, expected ~419");

        // log_{129/128}(1.168) = log_{129/128}(1168/1000) ≈ 20
        let v = log_fee_ratio(1168, 1000);
        assert!((19..=21).contains(&v), "log(1.168) = {v}, expected ~20");
    }

    #[test]
    fn log_fee_ratio_negative() {
        // log_{129/128}(0.168) = log_{129/128}(168/1000) ≈ -229
        let v = log_fee_ratio(168, 1000);
        assert!((-231..=-227).contains(&v), "log(0.168) = {v}, expected ~-229");
    }

    #[test]
    fn log_fee_ratio_one() {
        assert_eq!(log_fee_ratio(1, 1), 0);
        assert_eq!(log_fee_ratio(1000, 1000), 0);
    }

    #[test]
    fn fee_from_linear_zero_and_one() {
        assert_eq!(fee_from_linear(U256::ZERO), 0);
        assert_eq!(fee_from_linear(U256::from(1u64)), 0);
    }
}
