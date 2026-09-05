// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Shared EIP-1559 fee estimate and same-nonce replacement bump.
//!
//! Used by the batch poster (submission + retry), the fee oracle (charge), and
//! the mempool flusher (no-op replacement).
//!
//! ## Fee ceiling
//!
//! Same-nonce retries floor against the last recorded fees for **every**
//! pending nonce. Without a bound, a long L1 stall (or a 5s error-tick
//! ratchet) compounds ×1.1 until gas×maxFee ≈ wallet balance. Cap
//! `max_fee_per_gas` at `CEIL = max(K × E.max_fee, ABS_FLOOR)` where `E` is
//! **this tick's** fresh estimate (Optimism txmgr shape: multiplier on the
//! suggestion, not an absolute-gwei knob).
//!
//! - `K = 3`: CEIL only clips once base has risen ~(2K−1)× since the estimate;
//!   with a 72s re-estimate interval that is ~2.3× margin over max EIP-1559
//!   growth. Not an operator knob — a mis-set floor can cause a danger
//!   shutdown with no validatable lower bound.
//! - `ABS_FLOOR` (2 gwei): on base≈0 devnet/L2, `3×E` can be a few wei and
//!   sit forever under the node's min gas price.
//! - Clamp the **cap only** — the tip is steering; spend is still
//!   `paid = min(cap, base+tip) ≤ CEIL`.
//! - Escape valve: if the clamp would put the cap below `bump(prior.cap)`
//!   **and** `E.tip > prior.tip`, allow `cap = bump(prior.cap)` above CEIL
//!   so a starved-tip prior can heal when tips recover (geth couples the
//!   two fields).
//! - Hold: when the clamp bound this tick and the result cannot clear
//!   +10% over `prior.cap`, still broadcast (mempool probe) but write no
//!   floor — not an internal retry loop; the outer tick sleeps on the
//!   confirmation cadence.
//!
//! Operator funding bound: `balance ≥ N_pending × gas × K × E.max_fee`
//! (≈ `N × gas × 6 × base` with the default estimator) plus `gas × CEIL`
//! free for `estimateGas`.

use alloy::consensus::BlockHeader;
use alloy::providers::{DynProvider, Provider, utils};
use alloy::rpc::types::BlockNumberOrTag;

/// Multiplier on this tick's estimated `max_fee_per_gas` for the replacement
/// ceiling. Documented constant next to the estimator pin — not a knob.
pub const FEE_CEILING_MULTIPLIER: u128 = 3;

/// Absolute floor for the replacement ceiling (2 gwei). Keeps `3×E` above
/// typical node min-gas-price on base≈0 networks.
pub const FEE_CEILING_ABS_FLOOR_WEI: u128 = 2_000_000_000;

/// The three components of Alloy's default EIP-1559 estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip1559Fees {
    pub base_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
}

/// Result of [`fees_for_nonce`]: fees to send, and whether this tick is in
/// ceiling-hold (broadcast as probe, do not write the floor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeesForNonce {
    pub fees: Eip1559Fees,
    /// The ceiling clamped the cap and the result cannot clear
    /// [`bumped_replacement_fees`] of `prior.max_fee`. Caller must still
    /// broadcast, must not raise/replace the stored floor, and should
    /// surface a Held tick outcome.
    pub hold: bool,
}

/// Pad an `eth_estimateGas` result so a tight estimate cannot mine as an
/// out-of-gas revert (burns the wallet-nonce slot with no `InputAdded`).
pub fn pad_gas_estimate(gas: u64) -> u64 {
    gas.saturating_add(gas / 10)
}

/// `CEIL = max(K × estimate_max_fee, ABS_FLOOR)`.
pub fn fee_ceiling(estimate_max_fee: u128) -> u128 {
    FEE_CEILING_MULTIPLIER
        .saturating_mul(estimate_max_fee)
        .max(FEE_CEILING_ABS_FLOOR_WEI)
}

/// Bump one EIP-1559 component for a same-nonce replacement.
///
/// ×1.1, plus 1 wei so integer division cannot stall on a flat spot and so
/// geth's strict-greater precheck still passes when `x` is tiny. Saturating
/// `x+1` keeps a `u128::MAX` fee from shrinking after `saturating_mul`.
fn bump_replacement_component(value: u128) -> u128 {
    let bumped = value.saturating_mul(11) / 10 + 1;
    bumped.max(value.saturating_add(1))
}

/// Bump EIP-1559 fees for a same-nonce replacement under the ≥10% rule.
///
/// Both `max_fee` and the priority tip grow by the same ×1.1 (+1) factor.
/// Asymmetric growth (tip ×2, cap ×1.1) compounds across poster retries until
/// `tip > max_fee` — an invalid EIP-1559 tx every node rejects
/// (`ErrTipAboveFeeCap`), and because a failed send does not update the
/// in-flight floor the poster then resubmits the identical invalid pair
/// forever. Equal growth preserves `tip ≤ max_fee` whenever the input did;
/// the clamp is defense-in-depth for a one-shot bump of a near-zero-base
/// estimate (the flusher) and for already-invalid inputs.
///
/// The poster floors a re-estimate against the last recorded fees at that
/// wallet nonce (a successful broadcast **or** an underpriced raise that was
/// never accepted); the flusher prices no-ops at [`fee_ceiling`] so recovery
/// can clear anything the poster produced. Eviction is operational
/// acceleration, not a correctness precondition.
pub fn bumped_replacement_fees(base_max_fee: u128, base_priority_fee: u128) -> (u128, u128) {
    let tip = base_priority_fee.min(base_max_fee);
    let new_max_fee = bump_replacement_component(base_max_fee);
    let new_priority_fee = bump_replacement_component(tip).min(new_max_fee);
    (new_max_fee, new_priority_fee)
}

/// Absolute estimate, raised to a replacement floor when `prior` is set, then
/// capped by [`fee_ceiling`] on `max_fee_per_gas` only.
///
/// First send at a nonce uses `estimate` (clamped so `tip ≤ max_fee`, then
/// ceiling). A same-nonce resubmit takes the per-component max of the fresh
/// estimate and [`bumped_replacement_fees`] of the last recorded fees, so a
/// flat market cannot re-broadcast underpriced replacements. The ceiling and
/// tip≤cap clamps run **after** that floor and **before** returning — never
/// clamp only at the call site (that recreates tip>cap).
///
/// Underpriced self-correction must call
/// `fees_for_nonce(estimate, Some(attempted))` — basing the raise on `E`
/// keeps the stored floor from parking at `1.1×CEIL`.
pub fn fees_for_nonce(estimate: Eip1559Fees, prior: Option<Eip1559Fees>) -> FeesForNonce {
    let ceil = fee_ceiling(estimate.max_fee_per_gas);

    let (mut max_fee, mut tip) = match prior {
        None => (estimate.max_fee_per_gas, estimate.max_priority_fee_per_gas),
        Some(prior) => {
            let (bumped_max, bumped_prio) =
                bumped_replacement_fees(prior.max_fee_per_gas, prior.max_priority_fee_per_gas);
            (
                estimate.max_fee_per_gas.max(bumped_max),
                estimate.max_priority_fee_per_gas.max(bumped_prio),
            )
        }
    };

    let mut hold = false;
    if max_fee > ceil {
        let bump_prior_cap = prior.map(|p| bump_replacement_component(p.max_fee_per_gas));
        let escape = match (prior, bump_prior_cap) {
            (Some(p), Some(bump_cap)) => {
                // Clamp would put cap below the replacement floor, but tips
                // recovered: allow one bump(prior.cap) step above CEIL so
                // geth's coupled tip/cap rule can heal a starved-tip prior.
                ceil < bump_cap && estimate.max_priority_fee_per_gas > p.max_priority_fee_per_gas
            }
            _ => false,
        };
        if escape {
            max_fee = bump_prior_cap.expect("escape implies prior");
        } else {
            max_fee = ceil;
            if let Some(p) = prior {
                let (bump_cap, _) =
                    bumped_replacement_fees(p.max_fee_per_gas, p.max_priority_fee_per_gas);
                // Clamp bound this tick and we still cannot clear +10%.
                hold = max_fee < bump_cap;
            }
        }
    }

    tip = tip.min(max_fee);
    FeesForNonce {
        fees: Eip1559Fees {
            base_fee_per_gas: estimate.base_fee_per_gas,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: tip,
        },
        hold,
    }
}

/// Estimate fees with Alloy's default, MetaMask-style medium estimator.
///
/// We intentionally pin the policy constants here: 10 historical blocks, the
/// 20th-percentile reward, median positive reward, and `2 * base + priority`.
/// The submitter uses the cap; the oracle charges the expected `base + priority`.
pub async fn estimate_fees(provider: &DynProvider) -> Result<Eip1559Fees, String> {
    let history = provider
        .get_fee_history(
            utils::EIP1559_FEE_ESTIMATION_PAST_BLOCKS,
            BlockNumberOrTag::Latest,
            &[utils::EIP1559_FEE_ESTIMATION_REWARD_PERCENTILE],
        )
        .await
        .map_err(|err| err.to_string())?;

    let base_fee_per_gas = match history.latest_block_base_fee() {
        Some(base_fee) if base_fee != 0 => base_fee,
        _ => provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(|err| err.to_string())?
            .and_then(|block| block.header.base_fee_per_gas())
            .ok_or_else(|| "RPC does not support EIP-1559 base fees".to_string())?
            .into(),
    };
    let estimate =
        utils::eip1559_default_estimator(base_fee_per_gas, &history.reward.unwrap_or_default());
    Ok(Eip1559Fees {
        base_fee_per_gas,
        max_priority_fee_per_gas: estimate.max_priority_fee_per_gas,
        max_fee_per_gas: estimate.max_fee_per_gas,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_alloy_metamask_medium_estimator_policy() {
        assert_eq!(utils::EIP1559_FEE_ESTIMATION_PAST_BLOCKS, 10);
        assert_eq!(utils::EIP1559_FEE_ESTIMATION_REWARD_PERCENTILE, 20.0);

        // Positive rewards are [2, 4, 10], whose median is 4. The default
        // estimator makes `max_fee = 2 * base + priority`.
        let estimate =
            utils::eip1559_default_estimator(100, &[vec![0], vec![2], vec![10], vec![4]]);
        assert_eq!(estimate.max_priority_fee_per_gas, 4);
        assert_eq!(estimate.max_fee_per_gas, 204);
    }

    #[test]
    fn fee_ceiling_is_max_of_multiplier_and_abs_floor() {
        assert_eq!(fee_ceiling(0), FEE_CEILING_ABS_FLOOR_WEI);
        assert_eq!(fee_ceiling(1), FEE_CEILING_ABS_FLOOR_WEI);
        assert_eq!(
            fee_ceiling(10_000_000_000),
            FEE_CEILING_MULTIPLIER * 10_000_000_000
        );
    }

    #[test]
    fn pad_gas_estimate_adds_ten_percent() {
        assert_eq!(pad_gas_estimate(100_000), 110_000);
        assert_eq!(pad_gas_estimate(0), 0);
        assert_eq!(pad_gas_estimate(u64::MAX), u64::MAX);
    }

    #[test]
    fn replacement_fee_bump_exceeds_ten_percent_for_max_fee() {
        for base in [1_u128, 10, 100, 1_000, 1_000_000, 1_000_000_000_000] {
            let (new_max, _) = bumped_replacement_fees(base, 0);
            assert!(
                new_max.saturating_mul(10) >= base.saturating_mul(11),
                "max_fee bump violates ≥10% rule: base={base}, new={new_max}",
            );
        }
    }

    #[test]
    fn replacement_fee_bump_exceeds_ten_percent_for_priority_fee() {
        for base in [1_u128, 10, 100, 1_000, 1_000_000, 1_000_000_000_000] {
            let (_, new_prio) = bumped_replacement_fees(base.saturating_mul(4), base);
            assert!(
                new_prio.saturating_mul(10) >= base.saturating_mul(11),
                "priority bump violates ≥10% rule: base={base}, new={new_prio}",
            );
            assert!(new_prio > base);
        }
    }

    #[test]
    fn replacement_fee_bump_keeps_tip_at_or_below_fee_cap() {
        for (max_fee, tip) in [
            (0_u128, 0),
            (0, 100),
            (1, 1),
            (1, 10),
            (20_000_000_000, 1_000_000_000),
            (u128::MAX, u128::MAX),
        ] {
            let (new_max, new_prio) = bumped_replacement_fees(max_fee, tip);
            assert!(
                new_prio <= new_max,
                "bumped tip {new_prio} exceeds fee cap {new_max} (from max={max_fee} tip={tip})",
            );
        }
    }

    #[test]
    fn replacement_fee_floor_is_positive_even_when_base_is_zero() {
        let (new_max, new_prio) = bumped_replacement_fees(0, 0);
        assert!(new_max >= 1);
        assert!(new_prio >= 1);
    }

    #[test]
    fn replacement_fee_bump_saturates_at_u128_max() {
        let (new_max, new_prio) = bumped_replacement_fees(u128::MAX, u128::MAX);
        assert_eq!(new_max, u128::MAX);
        assert_eq!(new_prio, u128::MAX);
    }

    fn assert_eip1559_valid(fees: Eip1559Fees) {
        assert!(
            fees.max_priority_fee_per_gas <= fees.max_fee_per_gas,
            "invalid EIP-1559 pair: tip {} > max_fee {}",
            fees.max_priority_fee_per_gas,
            fees.max_fee_per_gas,
        );
    }

    #[test]
    fn fees_for_nonce_passes_estimate_through_on_first_send_when_under_ceiling() {
        let estimate = Eip1559Fees {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 2,
            max_fee_per_gas: 202,
        };
        // 202 < ABS_FLOOR ≤ CEIL, so the upper clamp does not bind.
        let out = fees_for_nonce(estimate, None);
        assert!(!out.hold);
        assert_eq!(out.fees, estimate);
    }

    #[test]
    fn fees_for_nonce_base_zero_ceiling_uses_abs_floor() {
        // Tiny estimate → CEIL = ABS_FLOOR (not 3 wei). A prior above that
        // clamps to the abs floor rather than a useless under-min-gas ceiling.
        let estimate = Eip1559Fees {
            base_fee_per_gas: 0,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: 1,
        };
        assert_eq!(
            fee_ceiling(estimate.max_fee_per_gas),
            FEE_CEILING_ABS_FLOOR_WEI
        );
        let prior = Eip1559Fees {
            base_fee_per_gas: 0,
            max_priority_fee_per_gas: 1,
            max_fee_per_gas: FEE_CEILING_ABS_FLOOR_WEI,
        };
        let out = fees_for_nonce(estimate, Some(prior));
        assert!(out.hold);
        assert_eq!(out.fees.max_fee_per_gas, FEE_CEILING_ABS_FLOOR_WEI);
        assert_eip1559_valid(out.fees);
    }

    #[test]
    fn fees_for_nonce_floors_flat_estimate_to_replacement_bump() {
        let prior = Eip1559Fees {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_gas: 1_000,
        };
        // Estimate high enough that 3×E clears bump(prior) and ABS_FLOOR.
        let estimate = Eip1559Fees {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_gas: 10_000_000_000,
        };
        let out = fees_for_nonce(estimate, Some(prior));
        let (bumped_max, bumped_prio) =
            bumped_replacement_fees(prior.max_fee_per_gas, prior.max_priority_fee_per_gas);
        assert!(!out.hold);
        assert_eq!(
            out.fees.max_fee_per_gas,
            estimate.max_fee_per_gas.max(bumped_max)
        );
        assert_eq!(
            out.fees.max_priority_fee_per_gas,
            estimate.max_priority_fee_per_gas.max(bumped_prio)
        );
        assert!(out.fees.max_fee_per_gas > prior.max_fee_per_gas);
    }

    #[test]
    fn fees_for_nonce_keeps_estimate_when_market_already_clears_bump() {
        let prior = Eip1559Fees {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_gas: 1_000,
        };
        let estimate = Eip1559Fees {
            base_fee_per_gas: 500,
            max_priority_fee_per_gas: 50,
            max_fee_per_gas: 10_000_000_000,
        };
        let out = fees_for_nonce(estimate, Some(prior));
        assert!(!out.hold);
        assert_eq!(out.fees.max_fee_per_gas, estimate.max_fee_per_gas);
        assert_eq!(
            out.fees.max_priority_fee_per_gas,
            estimate.max_priority_fee_per_gas
        );
    }

    #[test]
    fn fees_for_nonce_iterated_retry_respects_ceiling_and_tip_cap() {
        let estimate = Eip1559Fees {
            base_fee_per_gas: 20_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 41_000_000_000,
        };
        let ceil = fee_ceiling(estimate.max_fee_per_gas);
        let mut fees = estimate;
        for _ in 0..40 {
            let out = fees_for_nonce(estimate, Some(fees));
            assert_eip1559_valid(out.fees);
            assert!(
                out.fees.max_fee_per_gas <= ceil
                    || out.fees.max_fee_per_gas == bump_replacement_component(fees.max_fee_per_gas),
                "cap {} exceeded CEIL {ceil} without escape",
                out.fees.max_fee_per_gas
            );
            assert!(out.fees.max_priority_fee_per_gas <= out.fees.max_fee_per_gas);
            if out.hold {
                // Saturation: tip stays on the estimate/bump path, not ≈ CEIL.
                assert!(out.fees.max_fee_per_gas <= ceil);
                assert!(
                    out.fees.max_priority_fee_per_gas
                        <= estimate
                            .max_priority_fee_per_gas
                            .max(bump_replacement_component(fees.max_priority_fee_per_gas))
                            .min(out.fees.max_fee_per_gas)
                            + 1,
                    "hold tip walked up toward CEIL: tip={}",
                    out.fees.max_priority_fee_per_gas
                );
                break;
            }
            fees = out.fees;
            assert!(
                fees.max_fee_per_gas <= ceil,
                "recorded floor must not exceed CEIL across raises"
            );
        }
    }

    #[test]
    fn fees_for_nonce_hold_when_prior_above_ceiling_and_tips_flat() {
        let estimate = Eip1559Fees {
            base_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 10_000_000_000, // CEIL = 30 gwei
        };
        let ceil = fee_ceiling(estimate.max_fee_per_gas);
        let prior = Eip1559Fees {
            base_fee_per_gas: estimate.base_fee_per_gas,
            max_priority_fee_per_gas: estimate.max_priority_fee_per_gas,
            max_fee_per_gas: ceil, // already at ceiling
        };
        let out = fees_for_nonce(estimate, Some(prior));
        assert!(out.hold, "flat tips at CEIL must hold");
        assert_eq!(out.fees.max_fee_per_gas, ceil);
        assert_eip1559_valid(out.fees);
        // Tip must not walk to CEIL on hold.
        assert!(out.fees.max_priority_fee_per_gas < ceil);
        assert!(
            out.fees.max_priority_fee_per_gas
                <= bump_replacement_component(prior.max_priority_fee_per_gas)
        );
    }

    #[test]
    fn fees_for_nonce_escape_valve_heals_starved_tip_prior() {
        let estimate = Eip1559Fees {
            base_fee_per_gas: 10_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000, // recovered tip
            max_fee_per_gas: 30_000_000_000,         // CEIL = 90 gwei
        };
        let ceil = fee_ceiling(estimate.max_fee_per_gas);
        let prior = Eip1559Fees {
            base_fee_per_gas: 10_000_000_000,
            max_priority_fee_per_gas: 1, // starved (empty reward history)
            max_fee_per_gas: ceil,
        };
        let out = fees_for_nonce(estimate, Some(prior));
        let (bump_cap, _) =
            bumped_replacement_fees(prior.max_fee_per_gas, prior.max_priority_fee_per_gas);
        assert!(!out.hold, "recovered tip must escape hold");
        assert_eq!(out.fees.max_fee_per_gas, bump_cap);
        assert!(out.fees.max_fee_per_gas > ceil);
        assert!(out.fees.max_priority_fee_per_gas > prior.max_priority_fee_per_gas);
        assert_eip1559_valid(out.fees);
    }

    #[test]
    fn fees_for_nonce_clamps_tip_above_fee_cap_on_first_send() {
        let estimate = Eip1559Fees {
            base_fee_per_gas: 1,
            max_priority_fee_per_gas: FEE_CEILING_ABS_FLOOR_WEI + 500,
            max_fee_per_gas: FEE_CEILING_ABS_FLOOR_WEI,
        };
        let out = fees_for_nonce(estimate, None);
        assert_eq!(out.fees.max_fee_per_gas, FEE_CEILING_ABS_FLOOR_WEI);
        assert_eq!(out.fees.max_priority_fee_per_gas, FEE_CEILING_ABS_FLOOR_WEI);
    }

    #[test]
    fn fees_for_nonce_underpriced_raise_baselines_on_estimate() {
        // Raise must be fees_for_nonce(estimate, Some(attempted)), not
        // fees_for_nonce(attempted, Some(attempted)) — otherwise the floor
        // parks at 1.1×CEIL.
        let estimate = Eip1559Fees {
            base_fee_per_gas: 20_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 41_000_000_000,
        };
        let ceil = fee_ceiling(estimate.max_fee_per_gas);
        let attempted = Eip1559Fees {
            base_fee_per_gas: estimate.base_fee_per_gas,
            max_priority_fee_per_gas: estimate.max_priority_fee_per_gas,
            max_fee_per_gas: ceil,
        };
        let raised = fees_for_nonce(estimate, Some(attempted));
        assert!(raised.hold || raised.fees.max_fee_per_gas <= ceil);
        assert!(raised.fees.max_fee_per_gas <= ceil || raised.hold);
    }
}
