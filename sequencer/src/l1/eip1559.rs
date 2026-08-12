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

/// Bump EIP-1559 fees for a same-nonce replacement under the ≥10% rule.
///
/// `max_fee` gets ×1.1 (+1 for integer-rounding flat spots); priority doubles
/// (intentionally generous past the 10% threshold). The poster floors a
/// re-estimate against the last successful send at that wallet nonce; the
/// flusher bumps a fresh estimate so no-ops can compete with pending batch
/// txs. Eviction is operational acceleration, not a correctness precondition.
pub fn bumped_replacement_fees(base_max_fee: u128, base_priority_fee: u128) -> (u128, u128) {
    let new_max_fee = base_max_fee.saturating_mul(11) / 10 + 1;
    let new_priority_fee = base_priority_fee.saturating_mul(2).max(1);
    (new_max_fee, new_priority_fee)
}

/// Absolute estimate, raised to a replacement floor when `prior` is set.
///
/// First send at a nonce uses `estimate` unchanged. A same-nonce resubmit
/// takes the per-component max of the fresh estimate and
/// [`bumped_replacement_fees`] of the last successful broadcast, so a flat
/// market cannot re-broadcast underpriced replacements.
pub fn fees_for_nonce(estimate: Eip1559Fees, prior: Option<Eip1559Fees>) -> Eip1559Fees {
    let Some(prior) = prior else {
        return estimate;
    };
    let (bumped_max, bumped_prio) =
        bumped_replacement_fees(prior.max_fee_per_gas, prior.max_priority_fee_per_gas);
    Eip1559Fees {
        base_fee_per_gas: estimate.base_fee_per_gas,
        max_fee_per_gas: estimate.max_fee_per_gas.max(bumped_max),
        max_priority_fee_per_gas: estimate.max_priority_fee_per_gas.max(bumped_prio),
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
    fn replacement_fee_bump_doubles_priority_fee() {
        for base in [1_u128, 10, 1_000, 1_000_000_000] {
            let (_, new_prio) = bumped_replacement_fees(0, base);
            assert_eq!(new_prio, base.saturating_mul(2));
            assert!(
                new_prio.saturating_mul(10) >= base.saturating_mul(11),
                "priority bump violates ≥10% rule: base={base}, new={new_prio}",
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
        assert_eq!(new_max, u128::MAX / 10 + 1);
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
}
