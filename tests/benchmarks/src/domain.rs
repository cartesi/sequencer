// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use alloy_sol_types::Eip712Domain;
use serde::{Deserialize, Serialize};

use crate::{BenchResult, support::err};

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:3000";
pub const DOMAIN_NAME: &str = "CartesiAppSequencer";
pub const DOMAIN_VERSION: &str = "1";
pub const SELF_CONTAINED_DOMAIN_CHAIN_ID: u64 = 31_337;
pub const SELF_CONTAINED_DOMAIN_VERIFYING_CONTRACT: &str =
    "0x1111111111111111111111111111111111111111";

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BenchmarkDomain {
    pub chain_id: u64,
    pub verifying_contract: Address,
}

impl BenchmarkDomain {
    pub fn eip712_domain(self) -> Eip712Domain {
        Eip712Domain {
            name: Some(DOMAIN_NAME.to_string().into()),
            version: Some(DOMAIN_VERSION.to_string().into()),
            chain_id: Some(U256::from(self.chain_id)),
            verifying_contract: Some(self.verifying_contract),
            salt: None,
        }
    }
}

pub fn parse_address(raw: &str) -> Result<Address, String> {
    if !raw.starts_with("0x") {
        return Err("verifying contract must be 0x-prefixed".to_string());
    }

    let bytes =
        alloy_primitives::hex::decode(raw).map_err(|err| format!("invalid address hex: {err}"))?;
    if bytes.len() != 20 {
        return Err("verifying contract must be 20 bytes".to_string());
    }
    Ok(Address::from_slice(&bytes))
}

pub fn resolve_external_benchmark_domain(
    domain_chain_id: Option<u64>,
    domain_verifying_contract: Option<Address>,
) -> BenchResult<BenchmarkDomain> {
    let chain_id = domain_chain_id.ok_or_else(|| {
        err("external benchmarks require --domain-chain-id to match the target sequencer")
    })?;
    let verifying_contract = domain_verifying_contract.ok_or_else(|| {
        err("external benchmarks require --domain-verifying-contract to match the target sequencer")
    })?;

    Ok(BenchmarkDomain {
        chain_id,
        verifying_contract,
    })
}

pub fn self_contained_domain() -> BenchmarkDomain {
    BenchmarkDomain {
        chain_id: SELF_CONTAINED_DOMAIN_CHAIN_ID,
        // Used only by offline/unit benchmark paths that do not spawn a local chain.
        verifying_contract: parse_address(SELF_CONTAINED_DOMAIN_VERIFYING_CONTRACT)
            .expect("self-contained verifying contract is valid"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SELF_CONTAINED_DOMAIN_CHAIN_ID, SELF_CONTAINED_DOMAIN_VERIFYING_CONTRACT,
        resolve_external_benchmark_domain, self_contained_domain,
    };
    use crate::parse_address;

    #[test]
    fn external_domain_requires_explicit_inputs() {
        let error =
            resolve_external_benchmark_domain(None, None).expect_err("missing external domain");
        assert!(error.to_string().contains("--domain-chain-id"));
    }

    #[test]
    fn external_domain_uses_explicit_inputs() {
        let domain = resolve_external_benchmark_domain(
            Some(SELF_CONTAINED_DOMAIN_CHAIN_ID),
            Some(
                parse_address("0x1111111111111111111111111111111111111111")
                    .expect("valid verifying contract"),
            ),
        )
        .expect("external domain");
        assert_eq!(domain.chain_id, SELF_CONTAINED_DOMAIN_CHAIN_ID);
        assert_eq!(
            domain.verifying_contract.to_string(),
            "0x1111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn self_contained_domain_is_stable_for_offline_paths() {
        let domain = self_contained_domain();
        assert_eq!(domain.chain_id, SELF_CONTAINED_DOMAIN_CHAIN_ID);
        assert_eq!(
            domain.verifying_contract.to_string(),
            SELF_CONTAINED_DOMAIN_VERIFYING_CONTRACT
        );
    }
}
