// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use futures_util::{StreamExt, future::join_all};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use sequencer_core::api::WsTxMessage;
use sequencer_rust_client::{SequencerClient, SubscribeStream};

use crate::{
    BenchResult,
    domain::BenchmarkDomain,
    rejection::{RejectionOutcome, classify_rejection},
    runtime,
    stats::{Stats, StatsMs, rejection_rate, summarize, throughput_tx_per_s},
    support::io_err,
    workload::{WorkloadConfig, build_worker_contexts},
};

const DEFAULT_BACKLOG_DRAIN_IDLE_MS: u64 = 25;
const DEFAULT_BACKLOG_DRAIN_MAX_MS: u64 = 2_000;

#[derive(Debug, Clone)]
pub struct RoundTripRunConfig {
    pub endpoint: String,
    pub domain: BenchmarkDomain,
    pub from_offset: u64,
    pub duration: Duration,
    pub concurrency: usize,
    /// Per-worker nonce offsets (worker i starts at nonce_offsets[i]).
    /// Missing entries default to 0.
    pub nonce_offsets: Vec<u64>,
    pub max_fee: u16,
    pub request_timeout_ms: u64,
    pub max_ws_wait_ms: u64,
    pub workload: WorkloadConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundTripRunReport {
    pub endpoint: String,
    pub ws_subscribe_url: String,
    pub concurrency: usize,
    pub accepted: u64,
    pub rejected: u64,
    pub rejection_rate: f64,
    pub rejection_breakdown: BTreeMap<String, u64>,
    pub first_rejection: Option<String>,
    pub drained_ws_backlog_events: u64,
    pub consumed_ws_events_total: u64,
    pub total_wall: Duration,
    pub throughput_tps: f64,
    /// Per-worker accepted counts. Worker i accepted nonce_advances[i] ops.
    /// Use these to compute nonce offsets for a subsequent run.
    pub nonce_advances: Vec<u64>,
    pub ack_latency_accepted: Stats,
    pub ack_latency_accepted_ms: StatsMs,
    pub ack_latency_rejected: Option<Stats>,
    pub round_trip_latency_accepted: Stats,
    pub round_trip_latency_accepted_ms: StatsMs,
    pub memory: Option<runtime::MemoryReport>,
    pub sequencer_log_path: Option<String>,
}

struct RoundTripAckSample {
    /// Match key for WS event matching. None for rejected txs.
    match_key: Option<String>,
    submit_started: Instant,
    ack_latency: Duration,
    rejection: Option<RejectionOutcome>,
}

pub async fn run_round_trip_benchmark(
    config: RoundTripRunConfig,
) -> BenchResult<RoundTripRunReport> {
    let timeout = Duration::from_millis(config.request_timeout_ms);
    let client =
        SequencerClient::new_with_timeout(config.endpoint.clone(), timeout).map_err(|e| {
            crate::support::io_err(format!("invalid endpoint '{}': {e}", config.endpoint))
        })?;
    let ws_subscribe_url = client.ws_subscribe_url(config.from_offset);
    let domain = config.domain.eip712_domain();

    let workers = config.concurrency;

    // Build lightweight worker contexts — no signing happens here.
    let worker_contexts = build_worker_contexts(
        &config.workload,
        workers,
        config.max_fee,
        &domain,
        &config.nonce_offsets,
    )?;

    // Connect WS and drain backlog.
    let mut ws = client.subscribe(config.from_offset).await.map_err(|err| {
        io_err(format!(
            "ws connect failed: url={ws_subscribe_url}, error={err}"
        ))
    })?;
    let mut consumed_ws_events_total = 0_u64;
    let drained_ws_backlog_events = drain_existing_ws_backlog(
        &mut ws,
        Duration::from_millis(DEFAULT_BACKLOG_DRAIN_IDLE_MS),
        Duration::from_millis(DEFAULT_BACKLOG_DRAIN_MAX_MS),
    )
    .await?;
    consumed_ws_events_total += drained_ws_backlog_events;
    println!("drained_ws_backlog_events: {drained_ws_backlog_events}");

    // Shared map: match_key -> Vec<Instant> (WS arrival timestamps).
    // The concurrent WS reader writes here; the main task reads after shutdown.
    let ws_arrivals: Arc<Mutex<HashMap<String, Vec<Instant>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let ws_event_count = Arc::new(AtomicU64::new(0));

    // Shutdown signal for the WS reader.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Spawn concurrent WS reader task.
    let ws_arrivals_writer = Arc::clone(&ws_arrivals);
    let ws_event_count_writer = Arc::clone(&ws_event_count);
    let ws_reader_handle = tokio::spawn(async move {
        run_ws_reader(ws, ws_arrivals_writer, ws_event_count_writer, shutdown_rx).await
    });

    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<RoundTripAckSample>();
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
                    // Compute match key from the signed request before submitting.
                    let match_key = fixture_match_key(
                        request.sender.as_str(),
                        alloy_primitives::hex::encode_prefixed(&request.message.data).as_str(),
                    );
                    let submit_started = Instant::now();
                    let outcome = client.submit_tx_with_status(&request).await;
                    let ack_latency = submit_started.elapsed();
                    let rejection = classify_rejection(outcome);
                    let accepted = rejection.is_none();
                    if accepted {
                        nonce_index += 1;
                    }
                    let _ = tx.send(RoundTripAckSample {
                        match_key: if accepted { Some(match_key) } else { None },
                        submit_started,
                        ack_latency,
                        rejection,
                    });
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

    // Collect ACK results and build submit-start map for later matching.
    let mut accepted_ack_samples = Vec::new();
    let mut rejected_ack_samples = Vec::new();
    let mut accepted = 0_u64;
    let mut rejected = 0_u64;
    let mut first_rejection: Option<String> = None;
    let mut rejection_breakdown = BTreeMap::<String, u64>::new();
    let mut expected_submit_starts = HashMap::<String, Vec<Instant>>::new();

    while let Some(sample) = result_rx.recv().await {
        match sample.rejection {
            None => {
                accepted += 1;
                accepted_ack_samples.push(sample.ack_latency);
                if let Some(key) = sample.match_key {
                    expected_submit_starts
                        .entry(key)
                        .or_default()
                        .push(sample.submit_started);
                }
            }
            Some(rejection) => {
                rejected += 1;
                rejected_ack_samples.push(sample.ack_latency);
                *rejection_breakdown
                    .entry(rejection.key.clone())
                    .or_insert(0) += 1;
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

    // Give the WS reader a grace period to catch remaining events, then shut it down.
    let grace = Duration::from_millis(config.max_ws_wait_ms);
    let ws_reader_result = tokio::select! {
        result = ws_reader_handle => result,
        _ = async {
            // Poll until all accepted events are matched or grace period expires.
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                let matched_count = {
                    let map = ws_arrivals.lock().unwrap();
                    map.values().map(Vec::len).sum::<usize>()
                };
                if matched_count >= accepted as usize {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            // Signal shutdown.
            let _ = shutdown_tx.send(());
            // Wait for the reader to finish.
            futures_util::future::pending::<()>().await;
        } => unreachable!(),
    };

    if let Err(e) = ws_reader_result {
        return Err(io_err(format!("WS reader task panicked: {e}")));
    }

    consumed_ws_events_total += ws_event_count.load(Ordering::Relaxed);

    let total_wall = started.elapsed();

    // Join submit starts with WS arrival timestamps to compute round-trip latencies.
    let ws_arrival_map = match Arc::try_unwrap(ws_arrivals) {
        Ok(mutex) => mutex.into_inner().unwrap(),
        Err(arc) => arc.lock().unwrap().clone(),
    };

    let (ack_stats, round_trip_stats) = if accepted_ack_samples.is_empty() {
        (Stats::zero(), Stats::zero())
    } else {
        let round_trip_samples =
            join_round_trip_samples(expected_submit_starts, ws_arrival_map, accepted)?;
        (
            summarize(accepted_ack_samples.as_slice())?,
            summarize(round_trip_samples.as_slice())?,
        )
    };
    let rejected_stats = if rejected_ack_samples.is_empty() {
        None
    } else {
        Some(summarize(rejected_ack_samples.as_slice())?)
    };

    Ok(RoundTripRunReport {
        endpoint: config.endpoint,
        ws_subscribe_url,
        concurrency: config.concurrency,
        accepted,
        rejected,
        rejection_rate: rejection_rate(accepted, rejected),
        rejection_breakdown,
        first_rejection,
        drained_ws_backlog_events,
        consumed_ws_events_total,
        total_wall,
        throughput_tps: throughput_tx_per_s(round_trip_stats.count, total_wall),
        nonce_advances,
        ack_latency_accepted_ms: ack_stats.to_ms(),
        ack_latency_accepted: ack_stats,
        ack_latency_rejected: rejected_stats,
        round_trip_latency_accepted_ms: round_trip_stats.to_ms(),
        round_trip_latency_accepted: round_trip_stats,
        memory: None,
        sequencer_log_path: None,
    })
}

/// Concurrent WS reader: reads events in real time and timestamps arrivals.
/// Runs until `shutdown_rx` fires or the WS stream closes.
async fn run_ws_reader(
    mut ws: SubscribeStream,
    arrivals: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    event_count: Arc<AtomicU64>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            frame = ws.next() => {
                let Some(Ok(Message::Text(text))) = frame else {
                    // Stream closed or non-text frame — stop reading.
                    if frame.is_none() || matches!(frame, Some(Err(_))) {
                        break;
                    }
                    continue;
                };
                let arrival = Instant::now();
                event_count.fetch_add(1, Ordering::Relaxed);

                if let Ok(WsTxMessage::UserOp { sender, data, .. }) =
                    serde_json::from_str::<WsTxMessage>(text.as_str())
                {
                    let key = fixture_match_key(sender.as_str(), data.as_str());
                    arrivals.lock().unwrap().entry(key).or_default().push(arrival);
                }
            }
        }
    }
}

/// Match submit-start timestamps with WS arrival timestamps to compute
/// round-trip latencies. Both maps use the same match keys.
///
/// For duplicate keys (same sender+data), we sort both sides by time and
/// pair them in order — earliest submit with earliest arrival.
fn join_round_trip_samples(
    mut submit_starts: HashMap<String, Vec<Instant>>,
    mut ws_arrivals: HashMap<String, Vec<Instant>>,
    expected_accepted: u64,
) -> BenchResult<Vec<Duration>> {
    let mut samples = Vec::with_capacity(expected_accepted as usize);

    for (key, submits) in submit_starts.iter_mut() {
        let Some(arrivals) = ws_arrivals.get_mut(key) else {
            return Err(io_err(format!(
                "round-trip: no WS events for match key (missing {} event(s))",
                submits.len()
            )));
        };
        if arrivals.len() < submits.len() {
            return Err(io_err(format!(
                "round-trip: WS event count mismatch for key: expected {}, got {}",
                submits.len(),
                arrivals.len()
            )));
        }
        // Sort both by time to pair earliest-with-earliest.
        submits.sort();
        arrivals.sort();
        for (submit, arrival) in submits.iter().zip(arrivals.iter()) {
            samples.push(arrival.duration_since(*submit));
        }
    }

    if samples.len() != expected_accepted as usize {
        return Err(io_err(format!(
            "round-trip sample mismatch: accepted={expected_accepted}, matched={}",
            samples.len()
        )));
    }

    Ok(samples)
}

async fn drain_existing_ws_backlog(
    ws: &mut SubscribeStream,
    idle_quiet_window: Duration,
    max_total: Duration,
) -> BenchResult<u64> {
    let mut drained = 0_u64;
    let hard_deadline = tokio::time::Instant::now() + max_total;

    loop {
        let now = tokio::time::Instant::now();
        if now >= hard_deadline {
            break;
        }
        let remaining_until_deadline = hard_deadline - now;
        let poll_timeout = remaining_until_deadline.min(idle_quiet_window);

        match tokio::time::timeout(poll_timeout, ws.next()).await {
            Err(_) => break,
            Ok(None) => return Err(io_err("ws stream closed while draining backlog")),
            Ok(Some(Err(err))) => {
                return Err(io_err(format!(
                    "ws frame read failed while draining backlog: {err}"
                )));
            }
            Ok(Some(Ok(_))) => {
                drained = drained.saturating_add(1);
            }
        }
    }

    Ok(drained)
}

fn fixture_match_key(sender: &str, data_hex: &str) -> String {
    format!(
        "{}|{}",
        sender.to_ascii_lowercase(),
        data_hex.to_ascii_lowercase()
    )
}
