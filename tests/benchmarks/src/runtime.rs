// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::U256;
use sequencer_core::fee::fee_to_linear;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};
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

/// How long a self-contained run waits, after the load stops, for its sealed
/// batches to be accepted on L1.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(120);
const DRAIN_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageReport {
    /// `None` in results written before the logical size was measured.
    #[serde(default)]
    pub logical_db_bytes: Option<u64>,
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub growth_bytes_per_1k_accepted: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub struct StorageSample {
    logical_bytes: u64,
    db_bytes: u64,
    wal_bytes: u64,
}

/// Footprint (the SQLite file and its WAL, 0 when absent) and logical size.
pub fn sample_storage(runtime: &ManagedSequencer) -> BenchResult<StorageSample> {
    let db_path = runtime.db_path();
    let mut wal_path = db_path.clone().into_os_string();
    wal_path.push("-wal");
    let wal_bytes = match std::fs::metadata(&wal_path) {
        Ok(metadata) => metadata.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
        Err(err) => return Err(err.into()),
    };
    Ok(StorageSample {
        logical_bytes: runtime.logical_db_bytes()?,
        db_bytes: std::fs::metadata(&db_path)?.len(),
        wal_bytes,
    })
}

pub fn storage_report(
    start: StorageSample,
    end: StorageSample,
    accepted_count: u64,
) -> StorageReport {
    // Logical bytes: the WAL file sits at its autocheckpoint size and the main
    // file grows only when a checkpoint copies pages back, so file sizes are
    // footprint, not growth. Signed, because deletes can free pages.
    let growth_bytes = end.logical_bytes as f64 - start.logical_bytes as f64;
    // Same floor as the RSS metric: below it, one-time growth dominates.
    let growth_bytes_per_1k_accepted =
        (accepted_count >= 1000).then(|| growth_bytes / (accepted_count as f64 / 1000.0));
    StorageReport {
        logical_db_bytes: Some(end.logical_bytes),
        db_bytes: end.db_bytes,
        wal_bytes: end.wal_bytes,
        growth_bytes_per_1k_accepted,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainReport {
    pub batches_pending: u64,
    /// `None` when the backlog did not drain within [`DRAIN_DEADLINE`].
    pub drain_secs: Option<f64>,
}

/// Time from now, with the load stopped, until `safe_accepted_batches` holds
/// the highest valid sealed batch nonce read now. The open batch is excluded,
/// so this measures the submission backlog, not the batch-open window. Mines
/// one L1 block per second at Anvil's own clock.
pub async fn measure_drain(runtime: &ManagedSequencer) -> BenchResult<DrainReport> {
    let started = Instant::now();
    let frontier = runtime.batch_nonce_frontier()?;
    let Some(target) = frontier.sealed else {
        return Ok(DrainReport {
            batches_pending: 0,
            drain_secs: Some(0.0),
        });
    };
    let accepted_through = frontier.safe_accepted.map_or(0, |nonce| nonce + 1);
    let batches_pending = (target + 1).saturating_sub(accepted_through);
    if batches_pending == 0 {
        return Ok(DrainReport {
            batches_pending,
            drain_secs: Some(0.0),
        });
    }

    while started.elapsed() < DRAIN_DEADLINE {
        runtime.mine_l1_block_at_wall_clock().await?;
        tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
        let accepted = runtime.batch_nonce_frontier()?.safe_accepted;
        if accepted.is_some_and(|nonce| nonce >= target) {
            return Ok(DrainReport {
                batches_pending,
                drain_secs: Some(started.elapsed().as_secs_f64()),
            });
        }
    }
    Ok(DrainReport {
        batches_pending,
        drain_secs: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{StorageReport, StorageSample, storage_report};

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn storage_growth_comes_from_the_logical_size_not_the_footprint() {
        let sample = |logical_bytes| StorageSample {
            logical_bytes,
            db_bytes: 4 * MIB,
            wal_bytes: 4 * MIB,
        };
        // Unchanged footprint: a file-size basis would report 0.
        let report = storage_report(sample(MIB), sample(3 * MIB), 2000);
        assert_eq!(report.growth_bytes_per_1k_accepted, Some(MIB as f64));
        assert_eq!(report.logical_db_bytes, Some(3 * MIB));

        let report = storage_report(sample(MIB), sample(3 * MIB), 999);
        assert_eq!(report.growth_bytes_per_1k_accepted, None);
    }

    #[test]
    fn results_written_before_the_logical_size_still_load() {
        let old: StorageReport = serde_json::from_str(
            r#"{"db_bytes":1,"wal_bytes":2,"growth_bytes_per_1k_accepted":null}"#,
        )
        .expect("an old storage report loads");
        assert_eq!(old.logical_db_bytes, None);
    }
}
