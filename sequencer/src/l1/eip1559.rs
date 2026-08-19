// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Shared EIP-1559 fee estimate and same-nonce replacement bump.
//!
//! Used by the batch poster (submission + retry), the fee oracle (charge), and
//! the mempool flusher (no-op replacement).

use alloy::consensus::BlockHeader;
use alloy::providers::{DynProvider, Provider, utils};
use alloy::rpc::types::BlockNumberOrTag;

/// The three components of Alloy's default EIP-1559 estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eip1559Fees {
    pub base_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
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
/// The poster floors a re-estimate against the last successful send at that
/// wallet nonce; the flusher bumps a fresh estimate so no-ops can compete
/// with pending batch txs. Eviction is operational acceleration, not a
/// correctness precondition.
pub fn bumped_replacement_fees(base_max_fee: u128, base_priority_fee: u128) -> (u128, u128) {
    let tip = base_priority_fee.min(base_max_fee);
    let new_max_fee = bump_replacement_component(base_max_fee);
    let new_priority_fee = bump_replacement_component(tip).min(new_max_fee);
    (new_max_fee, new_priority_fee)
}

/// Absolute estimate, raised to a replacement floor when `prior` is set.
///
/// First send at a nonce uses `estimate` (clamped so `tip ≤ max_fee`). A
/// same-nonce resubmit takes the per-component max of the fresh estimate and
/// [`bumped_replacement_fees`] of the last successful broadcast, so a flat
/// market cannot re-broadcast underpriced replacements. The tip is then
/// clamped to the fee cap: geth will not accept `maxPriorityFeePerGas >
/// maxFeePerGas`, and a rejected send must not become a sticky invalid pair.
pub fn fees_for_nonce(estimate: Eip1559Fees, prior: Option<Eip1559Fees>) -> Eip1559Fees {
    let fees = match prior {
        None => estimate,
        Some(prior) => {
            let (bumped_max, bumped_prio) =
                bumped_replacement_fees(prior.max_fee_per_gas, prior.max_priority_fee_per_gas);
            Eip1559Fees {
                base_fee_per_gas: estimate.base_fee_per_gas,
                max_fee_per_gas: estimate.max_fee_per_gas.max(bumped_max),
                max_priority_fee_per_gas: estimate.max_priority_fee_per_gas.max(bumped_prio),
            }
        }
    };
    Eip1559Fees {
        max_priority_fee_per_gas: fees.max_priority_fee_per_gas.min(fees.max_fee_per_gas),
        ..fees
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
            // Cap high enough that the tip clamp does not bind.
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

    #[test]
    fn fees_for_nonce_passes_estimate_through_on_first_send() {
        let estimate = Eip1559Fees {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 2,
            max_fee_per_gas: 202,
        };
        assert_eq!(fees_for_nonce(estimate, None), estimate);
    }

    #[test]
    fn fees_for_nonce_floors_flat_estimate_to_replacement_bump() {
        let prior = Eip1559Fees {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_gas: 1_000,
        };
        // Flat market: estimate equals prior. Replacement must clear ≥10%.
        let estimate = prior;
        let fees = fees_for_nonce(estimate, Some(prior));
        let (bumped_max, bumped_prio) =
            bumped_replacement_fees(prior.max_fee_per_gas, prior.max_priority_fee_per_gas);
        assert_eq!(fees.max_fee_per_gas, bumped_max);
        assert_eq!(fees.max_priority_fee_per_gas, bumped_prio);
        assert!(fees.max_fee_per_gas > prior.max_fee_per_gas);
        assert!(fees.max_priority_fee_per_gas > prior.max_priority_fee_per_gas);
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
            max_fee_per_gas: 10_000,
        };
        let fees = fees_for_nonce(estimate, Some(prior));
        assert_eq!(fees, estimate);
    }

    #[test]
    fn fees_for_nonce_clears_both_fields_when_estimate_is_mixed() {
        // Market moved up on max_fee but not on priority (or the reverse):
        // each component must still clear the ≥10% floor vs the in-flight tx.
        let prior = Eip1559Fees {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 100,
            max_fee_per_gas: 1_000,
        };
        let (bumped_max, bumped_prio) =
            bumped_replacement_fees(prior.max_fee_per_gas, prior.max_priority_fee_per_gas);

        // High max_fee estimate, priority still below the replacement floor.
        let estimate_high_max = Eip1559Fees {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: prior.max_priority_fee_per_gas + 1, // < 10% bump
            max_fee_per_gas: bumped_max + 5_000,
        };
        let fees = fees_for_nonce(estimate_high_max, Some(prior));
        assert_eq!(fees.max_fee_per_gas, estimate_high_max.max_fee_per_gas);
        assert_eq!(fees.max_priority_fee_per_gas, bumped_prio);

        // High priority estimate, max_fee still below the replacement floor.
        let estimate_high_prio = Eip1559Fees {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: bumped_prio + 50,
            max_fee_per_gas: prior.max_fee_per_gas + 1, // < 10% bump
        };
        let fees = fees_for_nonce(estimate_high_prio, Some(prior));
        assert_eq!(fees.max_fee_per_gas, bumped_max);
        assert_eq!(
            fees.max_priority_fee_per_gas,
            estimate_high_prio.max_priority_fee_per_gas
        );
    }

    fn assert_eip1559_valid(fees: Eip1559Fees) {
        assert!(
            fees.max_priority_fee_per_gas <= fees.max_fee_per_gas,
            "invalid EIP-1559 pair: tip {} > max_fee {}",
            fees.max_priority_fee_per_gas,
            fees.max_fee_per_gas,
        );
    }

    fn assert_clears_replacement_floor(prior: Eip1559Fees, next: Eip1559Fees) {
        assert!(next.max_fee_per_gas > prior.max_fee_per_gas);
        assert!(next.max_priority_fee_per_gas > prior.max_priority_fee_per_gas);
        assert!(
            next.max_fee_per_gas.saturating_mul(10) >= prior.max_fee_per_gas.saturating_mul(11),
            "max_fee lost the ≥10% floor: prior={} next={}",
            prior.max_fee_per_gas,
            next.max_fee_per_gas,
        );
        assert!(
            next.max_priority_fee_per_gas.saturating_mul(10)
                >= prior.max_priority_fee_per_gas.saturating_mul(11),
            "priority lost the ≥10% floor: prior={} next={}",
            prior.max_priority_fee_per_gas,
            next.max_priority_fee_per_gas,
        );
    }

    #[test]
    fn fees_for_nonce_stays_valid_across_repeated_flat_retries() {
        // The poster records the *sent* pair, so a stuck nonce compounds the
        // bump against its own output. Asymmetric ×2 tip / ×1.1 cap crossed
        // `tip > max_fee` in ~7 rounds at 20 gwei base / 1 gwei tip — and on
        // the first retry when cap ≈ tip. Equal growth must stay valid.
        for start in [
            Eip1559Fees {
                base_fee_per_gas: 20_000_000_000,
                max_priority_fee_per_gas: 1_000_000_000,
                max_fee_per_gas: 41_000_000_000,
            },
            Eip1559Fees {
                base_fee_per_gas: 1,
                max_priority_fee_per_gas: 1,
                max_fee_per_gas: 1,
            },
            Eip1559Fees {
                base_fee_per_gas: 0,
                max_priority_fee_per_gas: 1_000,
                max_fee_per_gas: 1_000,
            },
        ] {
            let mut fees = start;
            assert_eip1559_valid(fees);
            for _ in 0..20 {
                let next = fees_for_nonce(fees, Some(fees));
                assert_eip1559_valid(next);
                assert_clears_replacement_floor(fees, next);
                fees = next;
            }
        }
    }

    #[test]
    fn fees_for_nonce_clamps_tip_above_fee_cap_on_first_send() {
        let estimate = Eip1559Fees {
            base_fee_per_gas: 1,
            max_priority_fee_per_gas: 500,
            max_fee_per_gas: 100,
        };
        let fees = fees_for_nonce(estimate, None);
        assert_eq!(fees.max_fee_per_gas, 100);
        assert_eq!(fees.max_priority_fee_per_gas, 100);
    }
}
