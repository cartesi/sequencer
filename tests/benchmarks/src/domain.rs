// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::Address;
use alloy_sol_types::Eip712Domain;
use serde::{Deserialize, Serialize};

use crate::{BenchResult, support::io_err};

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:3000";
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BenchmarkDomain {
    pub chain_id: u64,
    pub verifying_contract: Address,
}

impl BenchmarkDomain {
    pub fn eip712_domain(self) -> Eip712Domain {
        sequencer_core::build_input_domain(self.chain_id, self.verifying_contract)
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
        io_err("external benchmarks require --domain-chain-id to match the target sequencer")
    })?;
    let verifying_contract = domain_verifying_contract.ok_or_else(|| {
        io_err(
            "external benchmarks require --domain-verifying-contract to match the target sequencer",
        )
    })?;

    Ok(BenchmarkDomain {
        chain_id,
        verifying_contract,
    })
}

#[cfg(test)]
mod tests {
    use super::resolve_external_benchmark_domain;
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
            Some(31_337),
            Some(
                parse_address("0x1111111111111111111111111111111111111111")
                    .expect("valid verifying contract"),
            ),
        )
        .expect("external domain");
        assert_eq!(domain.chain_id, 31_337);
        assert_eq!(
            domain.verifying_contract.to_string(),
            "0x1111111111111111111111111111111111111111"
        );
    }
}
