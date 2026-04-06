// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use sequencer_rust_client::SequencerClient;

use crate::{
    BenchResult,
    domain::BenchmarkDomain,
    rejection::{RejectionOutcome, classify_rejection},
    runtime,
    stats::{Stats, StatsMs, rejection_rate, summarize, throughput_tx_per_s},
    workload::{WorkloadConfig, build_worker_contexts},
};

#[derive(Debug, Clone)]
pub struct AckRunConfig {
    pub endpoint: String,
    pub domain: BenchmarkDomain,
    pub duration: Duration,
    pub concurrency: usize,
    /// Per-worker nonce offsets (worker i starts at nonce_offsets[i]).
    /// Missing entries default to 0.
    pub nonce_offsets: Vec<u64>,
    pub max_fee: u16,
    pub request_timeout_ms: u64,
    pub workload: WorkloadConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckRunReport {
    pub accepted: u64,
    pub rejected: u64,
    pub rejection_rate: f64,
    pub rejection_breakdown: BTreeMap<String, u64>,
    /// First detail string observed for each breakdown key.
    pub rejection_examples: BTreeMap<String, String>,
    pub first_rejection: Option<String>,
    pub total_wall: Duration,
    pub throughput_tps: f64,
    pub endpoint: String,
    pub concurrency: usize,
    /// Per-worker accepted counts. Worker i accepted nonce_advances[i] ops.
    /// Use these to compute nonce offsets for a subsequent run.
    pub nonce_advances: Vec<u64>,
    pub ack_latency_accepted: Stats,
    pub ack_latency_accepted_ms: StatsMs,
    pub ack_latency_rejected: Option<Stats>,
    pub memory: Option<runtime::MemoryReport>,
    pub sequencer_log_path: Option<String>,
}

struct AckSample {
    latency: Duration,
    rejection: Option<RejectionOutcome>,
}

pub async fn run_ack_benchmark(config: AckRunConfig) -> BenchResult<AckRunReport> {
    let domain = config.domain.eip712_domain();
    let timeout = Duration::from_millis(config.request_timeout_ms);
    let client =
        SequencerClient::new_with_timeout(config.endpoint.clone(), timeout).map_err(|e| {
            crate::support::io_err(format!("invalid endpoint '{}': {e}", config.endpoint))
        })?;

    let workers = config.concurrency;

    // Build lightweight worker contexts — no signing happens here.
    let worker_contexts = build_worker_contexts(
        &config.workload,
        workers,
        config.max_fee,
        &domain,
        &config.nonce_offsets,
    )?;

    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<AckSample>();
    let progress_counter = Arc::new(AtomicU64::new(0));
    let duration = config.duration;

    // Start the clock — signing happens JIT per-op but is excluded from
    // the per-request latency measurement.
    let started = Instant::now();
    let deadline = started + duration;

    let handles: Vec<_> = worker_contexts
        .into_iter()
        .map(|ctx| {
            let client = client.clone();
            let tx = result_tx.clone();
            let progress = Arc::clone(&progress_counter);
            tokio::spawn(async move {
                let mut nonce_index: u64 = 0;
                loop {
                    if Instant::now() >= deadline {
                        break;
                    }
                    let request = match ctx.build_request(nonce_index) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("worker build_request failed: {e}");
                            break;
                        }
                    };
                    let sent_at = Instant::now();
                    let outcome = client.submit_tx_with_status(&request).await;
                    let latency = sent_at.elapsed();
                    let rejection = classify_rejection(outcome);
                    if rejection.is_none() {
                        nonce_index += 1;
                    }
                    let _ = tx.send(AckSample { latency, rejection });
                    let processed = progress.fetch_add(1, Ordering::Relaxed) + 1;
                    if processed.is_multiple_of(1000) {
                        println!("progress: {processed} ops");
                    }
                }
                nonce_index
            })
        })
        .collect();
    drop(result_tx);

    // Collect all results.
    let mut accepted_ack_samples = Vec::new();
    let mut rejected_ack_samples = Vec::new();
    let mut accepted = 0_u64;
    let mut rejected = 0_u64;
    let mut first_rejection: Option<String> = None;
    let mut rejection_breakdown = BTreeMap::<String, u64>::new();
    let mut rejection_examples = BTreeMap::<String, String>::new();

    while let Some(sample) = result_rx.recv().await {
        match sample.rejection {
            None => {
                accepted += 1;
                accepted_ack_samples.push(sample.latency);
            }
            Some(rejection) => {
                rejected += 1;
                rejected_ack_samples.push(sample.latency);
                *rejection_breakdown
                    .entry(rejection.key.clone())
                    .or_insert(0) += 1;
                rejection_examples
                    .entry(rejection.key.clone())
                    .or_insert_with(|| rejection.detail.clone());
                if first_rejection.is_none() {
                    first_rejection = Some(rejection.detail);
                }
            }
        }
    }

    // Wait for all workers and collect their nonce advances.
    let join_results = join_all(handles).await;
    let mut nonce_advances = Vec::with_capacity(workers);
    for result in join_results {
        nonce_advances.push(result.unwrap_or(0));
    }
    let total_wall = started.elapsed();

    let ack_stats = if accepted_ack_samples.is_empty() {
        Stats::zero()
    } else {
        summarize(accepted_ack_samples.as_slice())?
    };
    let rejected_stats = if rejected_ack_samples.is_empty() {
        None
    } else {
        Some(summarize(rejected_ack_samples.as_slice())?)
    };

    Ok(AckRunReport {
        endpoint: config.endpoint,
        concurrency: config.concurrency,
        accepted,
        rejected,
        rejection_rate: rejection_rate(accepted, rejected),
        rejection_breakdown,
        rejection_examples,
        first_rejection,
        total_wall,
        throughput_tps: throughput_tx_per_s(ack_stats.count, total_wall),
        nonce_advances,
        ack_latency_accepted_ms: ack_stats.to_ms(),
        ack_latency_accepted: ack_stats,
        ack_latency_rejected: rejected_stats,
        memory: None,
        sequencer_log_path: None,
    })
}
