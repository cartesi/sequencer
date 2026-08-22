// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! L1 client surface: reads InputBox events into storage (`reader`), submits
//! batches back out (`submitter`), and shares L1 utilities (`provider`,
//! `partition`).

pub mod eip1559;
pub mod fee_oracle;
pub mod partition;
pub mod provider;
pub mod reader;
pub mod submitter;
pub mod watermark;

use alloy_primitives::Address;

/// Shared L1 / InputBox configuration used by the input reader, the batch
/// submitter, and the recovery flush.
///
/// Built once per command from the pinned deployment identity plus the
/// command config, so RPC URL, InputBox address, and app address are defined
/// in a single place and not duplicated across component configs. Homed here
/// (not with the command configs): this is L1-domain identity consumed by
/// mechanisms below the command layer.
#[derive(Debug, Clone)]
pub struct L1Config {
    pub eth_rpc_url: String,
    pub input_box_address: Address,
    pub app_address: Address,
    pub batch_submitter_private_key: String,
    pub batch_submitter_address: Address,
    /// The pinned deployment chain id. Carried here so keyed-write paths (e.g.
    /// the preemptive-recovery flush) can re-confirm the RPC's chain id right
    /// before signing via [`crate::l1::provider::create_verified_signer_provider`].
    pub chain_id: u64,
    /// Opt into plaintext (`http://`) RPC against a non-loopback host — a
    /// trusted private network (Docker/K8s service, private-VPC IP). Off by
    /// default: the provider layer refuses remote plaintext otherwise. See
    /// [`crate::l1::provider`].
    pub allow_insecure_rpc: bool,
}
