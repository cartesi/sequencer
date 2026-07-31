// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0

//! Shared EIP-1559 fee estimate used by the poster and fee oracle.

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
}
