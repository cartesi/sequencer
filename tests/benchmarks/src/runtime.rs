// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::U256;
use sequencer_core::fee::fee_to_linear;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::workload::funded_account_plans;
use crate::{BenchResult, BenchmarkDomain, WorkloadConfig};

pub use rollups_harness::ManagedSequencer;
use rollups_harness::TestSigner;

pub const DEFAULT_SEQUENCER_BIN: &str = "target/release/wallet-sequencer-devnet";
pub const DEFAULT_MEMORY_SAMPLE_INTERVAL_MS: u64 = 500;
pub const DEFAULT_RESULTS_DIR: &str = "tests/benchmarks/results";
const BENCHMARK_FUNDING_SIGNER_PRIVATE_KEY: &str =
    "0x1000000000000000000000000000000000000000000000000000000000000001";
const BENCHMARK_FUNDING_SIGNER_ETH_WEI: u64 = 1_000_000_000_000_000_000;

pub fn managed_sequencer_config(
    log_prefix: impl Into<String>,
    sequencer_bin: impl Into<String>,
) -> rollups_harness::ManagedSequencerConfig {
    let mut config = rollups_harness::default_devnet_sequencer_config(log_prefix);
    config.sequencer_bin = PathBuf::from(sequencer_bin.into());
    config.logs_dir = PathBuf::from(DEFAULT_RESULTS_DIR);
    config
}

pub fn benchmark_domain(runtime: &ManagedSequencer) -> BenchmarkDomain {
    BenchmarkDomain {
        chain_id: runtime.domain_chain_id(),
        verifying_contract: runtime.verifying_contract(),
    }
}

pub async fn bootstrap_funded_workload(
    runtime: &ManagedSequencer,
    workload: &WorkloadConfig,
    per_worker_counts: &[u64],
    max_fee: u16,
) -> BenchResult<()> {
    let plans = funded_account_plans(workload, per_worker_counts, max_fee)?;
    if plans.is_empty() {
        return Ok(());
    }

    let funding_signer = TestSigner::from_private_key_hex(BENCHMARK_FUNDING_SIGNER_PRIVATE_KEY)?;
    let funding_address = funding_signer.address();
    if plans.iter().any(|plan| plan.address == funding_address) {
        return Err(std::io::Error::other(format!(
            "benchmark funding signer {funding_address} overlaps with a funded-transfer workload account"
        ))
        .into());
    }
    let total_required_balance = plans.iter().fold(U256::ZERO, |acc, plan| {
        acc.saturating_add(plan.required_balance)
    });
    if total_required_balance.is_zero() {
        return Ok(());
    }

    // The funding signer will do one L2 transfer per plan. Each transfer costs gas
    // at the frame fee (currently 1356), so we must deposit enough to cover both the
    // workload balances AND the funding transfers themselves.
    let transfer_count = plans
        .iter()
        .filter(|p| !p.required_balance.is_zero())
        .count();
    let funding_gas = U256::from(transfer_count as u64) * fee_to_linear(max_fee);
    let deposit_amount = total_required_balance.saturating_add(funding_gas);

    let mut ws = runtime.ws(0).await?;
    let l1_funder = runtime.wallet_l1(TestSigner::from_default(0)?).await?;
    l1_funder
        .send_eth(
            funding_address,
            U256::from(BENCHMARK_FUNDING_SIGNER_ETH_WEI),
        )
        .await?;

    let l1 = runtime.wallet_l1(funding_signer.clone()).await?;
    l1.mint_and_deposit_supported_token(deposit_amount).await?;
    runtime.mine_l1_blocks(1).await?;
    let _ = ws
        .expect_direct_input_from(runtime.erc20_portal_address())
        .await?;

    let mut l2 = runtime.wallet_l2(funding_signer)?;
    for plan in plans {
        if plan.required_balance.is_zero() {
            continue;
        }

        l2.transfer(plan.address, plan.required_balance).await?;
        let _ = ws.expect_user_op_from(funding_address).await?;
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryReport {
    pub method: String,
    pub sample_interval_ms: u64,
    pub rss_start_mb: Option<f64>,
    pub rss_peak_mb: Option<f64>,
    pub rss_end_mb: Option<f64>,
    pub rss_growth_mb: Option<f64>,
    pub rss_growth_per_1k_accepted_tx_mb: Option<f64>,
}

pub struct MemorySampler {
    stop_tx: Option<oneshot::Sender<u64>>,
    join: JoinHandle<MemoryReport>,
}

impl MemorySampler {
    pub fn start(pid: u32, sample_interval: Duration) -> Self {
        let (stop_tx, stop_rx) = oneshot::channel::<u64>();
        let join =
            tokio::spawn(
                async move { sample_memory_until_stop(pid, sample_interval, stop_rx).await },
            );
        Self {
            stop_tx: Some(stop_tx),
            join,
        }
    }

    pub async fn stop(mut self, accepted_count: u64) -> BenchResult<MemoryReport> {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(accepted_count);
        }
        let report = self.join.await.map_err(|err| {
            std::io::Error::other(format!("memory sampler task join failed: {err}"))
        })?;
        Ok(report)
    }
}

async fn sample_memory_until_stop(
    pid: u32,
    sample_interval: Duration,
    mut stop_rx: oneshot::Receiver<u64>,
) -> MemoryReport {
    let mut start_mb = sample_rss_mb(pid);
    let mut end_mb = start_mb;
    let mut peak_mb = start_mb;
    let mut accepted_count = 0_u64;

    loop {
        tokio::select! {
            maybe_accepted = &mut stop_rx => {
                if let Ok(value) = maybe_accepted {
                    accepted_count = value;
                }
                end_mb = sample_rss_mb(pid).or(end_mb);
                if let Some(end) = end_mb {
                    peak_mb = Some(peak_mb.unwrap_or(end).max(end));
                }
                break;
            }
            _ = tokio::time::sleep(sample_interval) => {
                if let Some(current) = sample_rss_mb(pid) {
                    if start_mb.is_none() {
                        start_mb = Some(current);
                    }
                    end_mb = Some(current);
                    peak_mb = Some(peak_mb.unwrap_or(current).max(current));
                }
            }
        }
    }

    let rss_growth_mb = match (start_mb, end_mb) {
        (Some(start), Some(end)) => Some(end - start),
        _ => None,
    };
    // Only compute per-1k metric when accepted_count >= 1000; below that threshold
    // the value is dominated by one-time startup allocations and is misleading.
    let rss_growth_per_1k_accepted_tx_mb = match (rss_growth_mb, accepted_count) {
        (Some(growth), value) if value >= 1000 => Some(growth / (value as f64 / 1000.0)),
        _ => None,
    };

    MemoryReport {
        method: "ps-rss-sampling".to_string(),
        sample_interval_ms: u64::try_from(sample_interval.as_millis()).unwrap_or(u64::MAX),
        rss_start_mb: start_mb,
        rss_peak_mb: peak_mb,
        rss_end_mb: end_mb,
        rss_growth_mb,
        rss_growth_per_1k_accepted_tx_mb,
    }
}

fn sample_rss_mb(pid: u32) -> Option<f64> {
    let output = std::process::Command::new("ps")
        .arg("-o")
        .arg("rss=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let rss_kib = text.trim().parse::<f64>().ok()?;
    Some(rss_kib / 1024.0)
}
