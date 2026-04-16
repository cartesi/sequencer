// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::{Address, U256};
use alloy_sol_types::Eip712Domain;

pub mod api;
pub mod application;
pub mod batch;
pub mod broadcast;
pub mod fee;
pub mod l2_tx;
pub mod user_op;

/// Maximum number of L1 blocks a batch can wait before the scheduler considers it stale.
/// Shared between the scheduler (canonical-app) and the sequencer (batch submitter, startup detection).
pub const MAX_WAIT_BLOCKS: u64 = 1200;

/// EIP-712 domain name shared between sequencer and scheduler.
pub const DOMAIN_NAME: &str = "CartesiAppSequencer";

/// EIP-712 domain version shared between sequencer and scheduler.
pub const DOMAIN_VERSION: &str = "1";

/// Build the canonical EIP-712 domain for user-op signing and verification.
///
/// Both the sequencer (signature verification at ingress) and the scheduler
/// (signature recovery during batch execution) MUST use this constructor.
/// A mismatch in any field changes the domain separator and causes every
/// signature to recover a different address.
pub fn build_input_domain(chain_id: u64, verifying_contract: Address) -> Eip712Domain {
    Eip712Domain {
        name: Some(DOMAIN_NAME.into()),
        version: Some(DOMAIN_VERSION.into()),
        chain_id: Some(U256::from(chain_id)),
        verifying_contract: Some(verifying_contract),
        salt: None,
    }
}
