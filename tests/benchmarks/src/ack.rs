// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

use sequencer_rust_client::SequencerClient;

use crate::{
    BenchResult,
    domain::BenchmarkDomain,
    rejection::classify_rejection,
    runtime,
    stats::{Stats, rejection_rate, summarize},
    support::{DEFAULT_PROGRESS_EVERY, now},
    workload::{WorkloadConfig, WorkloadState},
};

#[derive(Debug, Clone)]
pub struct AckRunConfig {
    pub endpoint: String,
    pub domain: BenchmarkDomain,
    pub count: u64,
    pub concurrency: usize,
    pub seed_offset: u64,
    pub max_fee: u16,
    pub request_timeout_ms: u64,
    pub fail_on_rejection: bool,
    pub workload: WorkloadConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckRunReport {
    pub count: u64,
    pub endpoint: String,
    pub concurrency: usize,
    pub accepted: u64,
    pub rejected: u64,
    pub rejection_rate: f64,
    pub rejection_breakdown: BTreeMap<String, u64>,
    pub first_rejection: Option<String>,
    pub total_wall: Duration,
    pub ack_latency_accepted: Stats,
    pub ack_latency_rejected: Option<Stats>,
    pub memory: Option<runtime::MemoryReport>,
    pub sequencer_log_path: Option<String>,
}

pub async fn run_ack_benchmark(config: AckRunConfig) -> BenchResult<AckRunReport> {
    let domain = config.domain.eip712_domain();
    let timeout = Duration::from_millis(config.request_timeout_ms);
    let client = SequencerClient::new_with_timeout(config.endpoint.clone(), timeout)
        .map_err(|e| crate::support::err(format!("invalid endpoint '{}': {e}", config.endpoint)))?;
    let mut workload = WorkloadState::initialize(&config.workload, config.seed_offset)?;
    let effective_concurrency = if let Some(cap) = workload.concurrency_cap() {
        let capped = config.concurrency.min(cap);
        if capped < config.concurrency {
            println!(
                "workload concurrency capped: requested={}, effective={}, funded_accounts={}",
                config.concurrency, capped, cap
            );
        }
        capped
    } else {
        config.concurrency
    };
    let mut accepted_ack_samples = Vec::with_capacity(config.count as usize);
    let mut rejected_ack_samples = Vec::new();
    let mut accepted = 0_u64;
    let mut rejected = 0_u64;
    let mut first_rejection: Option<String> = None;
    let mut rejection_breakdown = BTreeMap::<String, u64>::new();
    let started = now();

    while accepted.saturating_add(rejected) < config.count {
        let remaining = config
            .count
            .saturating_sub(accepted.saturating_add(rejected));
        let batch_size = remaining.min(effective_concurrency as u64) as usize;

        let mut inflight = Vec::with_capacity(batch_size);
        for _ in 0..batch_size {
            let fixture = workload.next_fixture(config.max_fee, &domain)?;
            let client = client.clone();
            let sent_at = now();
            inflight.push(async move {
                let outcome = client.submit_tx_with_status(&fixture.request).await;
                (sent_at.elapsed(), outcome)
            });
        }

        for (ack_latency, outcome) in join_all(inflight).await {
            match classify_rejection(outcome) {
                None => {
                    accepted = accepted.saturating_add(1);
                    accepted_ack_samples.push(ack_latency);
                }
                Some(rejection) => {
                    rejected = rejected.saturating_add(1);
                    rejected_ack_samples.push(ack_latency);
                    *rejection_breakdown
                        .entry(rejection.key.clone())
                        .or_insert(0) += 1;
                    if first_rejection.is_none() {
                        first_rejection = Some(rejection.detail);
                    }
                }
            }
        }

        let processed = accepted.saturating_add(rejected);
        if DEFAULT_PROGRESS_EVERY > 0
            && processed > 0
            && processed.is_multiple_of(DEFAULT_PROGRESS_EVERY)
        {
            println!(
                "progress: processed={processed}/{}, accepted={accepted}, rejected={rejected}",
                config.count
            );
        }
    }

    if config.fail_on_rejection && rejected > 0 {
        let reason = first_rejection
            .clone()
            .unwrap_or_else(|| "unknown rejection".to_string());
        return Err(std::io::Error::other(format!(
            "ack benchmark saw {rejected} rejection(s): {reason}"
        ))
        .into());
    }

    if accepted_ack_samples.is_empty() {
        return Err(std::io::Error::other("ack benchmark had no accepted txs").into());
    }

    let total_wall = started.elapsed();
    let ack_stats = summarize(accepted_ack_samples.as_slice())?;
    let rejected_stats = if rejected_ack_samples.is_empty() {
        None
    } else {
        Some(summarize(rejected_ack_samples.as_slice())?)
    };

    Ok(AckRunReport {
        count: config.count,
        endpoint: config.endpoint,
        concurrency: config.concurrency,
        accepted,
        rejected,
        rejection_rate: rejection_rate(accepted, rejected),
        rejection_breakdown,
        first_rejection,
        total_wall,
        ack_latency_accepted: ack_stats,
        ack_latency_rejected: rejected_stats,
        memory: None,
        sequencer_log_path: None,
    })
}
