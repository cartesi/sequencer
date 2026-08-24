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

/// The L1 bundle handed whole to the entry points that reach L1 with the
/// submitter key: the batch-submitter provider builder and the
/// preemptive-recovery flush. Built once in `run` from the pinned
/// [`DeploymentIdentity`](crate::storage::DeploymentIdentity) plus the
/// command config; the identity is carried verbatim, never re-copied field
/// by field, so every consumer reads the pinned values through one route.
///
/// It is not a funnel: the per-component configs (`InputReaderConfig`,
/// `BatchPosterConfig`, `UniswapConfig`) are built directly from the same
/// two sources — `identity` carries deployment values, and `RunConfig`
/// carries the per-client tuning knobs (`long_block_range_error_codes`,
/// poll intervals, confirmation depth) that belong to each client, not to
/// the L1 endpoint. Homed here (not with the command configs): L1-domain
/// identity consumed by mechanisms below the command layer.
#[derive(Debug, Clone)]
pub struct L1Config {
    /// The pinned deployment identity, verbatim from the DB. Carried whole so
    /// keyed-write paths (e.g. the preemptive-recovery flush) can re-confirm
    /// the RPC's chain id right before signing via
    /// [`crate::l1::provider::create_verified_signer_provider`].
    pub identity: crate::storage::DeploymentIdentity,
    pub eth_rpc_url: String,
    pub batch_submitter_private_key: SubmitterKey,
    /// Opt into plaintext (`http://`) RPC against a non-loopback host — a
    /// trusted private network (Docker/K8s service, private-VPC IP). Off by
    /// default: the provider layer refuses remote plaintext otherwise. See
    /// [`crate::l1::provider`].
    pub allow_insecure_rpc: bool,
}

/// Hex-encoded batch-submitter signing key. `Debug` redacts and no `Display`
/// exists, so the secret cannot reach logs through formatting; the raw hex
/// is reachable only via [`Self::expose_secret`], keeping every consumer of
/// the secret greppable. The key's *public* identity needs no accessor — it
/// is the pinned `identity.batch_submitter_address` beside it in
/// [`L1Config`], verified against this key at the command gate.
#[derive(Clone)]
pub struct SubmitterKey(String);

impl SubmitterKey {
    pub fn new(hex: String) -> Self {
        Self(hex)
    }

    /// The raw hex. Every caller handles secret material — keep the call
    /// sites few and auditable.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SubmitterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SubmitterKey([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::SubmitterKey;

    #[test]
    fn submitter_key_debug_never_prints_the_secret() {
        let key = SubmitterKey::new("0xdeadbeef_sentinel".to_string());
        let debug = format!("{key:?}");
        assert!(!debug.contains("sentinel"), "got: {debug}");
        assert_eq!(debug, "SubmitterKey([redacted])");
        // The derived Debug of a carrier struct inherits the redaction.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Carrier {
            key: SubmitterKey,
        }
        let carried = format!(
            "{:?}",
            Carrier {
                key: SubmitterKey::new("0xdeadbeef_sentinel".to_string())
            }
        );
        assert!(!carried.contains("sentinel"), "got: {carried}");
    }
}
