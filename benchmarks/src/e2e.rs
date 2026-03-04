// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use futures_util::{StreamExt, future::join_all};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use sequencer_core::api::WsTxMessage;
use sequencer_rust_client::SequencerClient;

use crate::{
    BenchResult,
    domain::BenchmarkDomain,
    rejection::classify_rejection,
    runtime,
    stats::{Stats, rejection_rate, summarize},
    support::{DEFAULT_PROGRESS_EVERY, io_err, now},
    workload::{WorkloadConfig, WorkloadState},
};

const DEFAULT_BACKLOG_DRAIN_IDLE_MS: u64 = 25;
const DEFAULT_BACKLOG_DRAIN_MAX_MS: u64 = 2_000;

#[derive(Debug, Clone)]
pub struct E2eRunConfig {
    pub endpoint: String,
    pub domain: BenchmarkDomain,
    pub from_offset: u64,
    pub count: u64,
    pub concurrency: usize,
    pub seed_offset: u64,
    pub max_fee: u32,
    pub request_timeout_ms: u64,
    pub max_ws_wait_ms: u64,
    pub fail_on_rejection: bool,
    pub workload: WorkloadConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eRunReport {
    pub count: u64,
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
    pub ack_latency_accepted: Stats,
    pub ack_latency_rejected: Option<Stats>,
    pub e2e_latency_accepted: Stats,
    pub memory: Option<runtime::MemoryReport>,
    pub sequencer_log_path: Option<String>,
}

pub async fn run_e2e_benchmark(config: E2eRunConfig) -> BenchResult<E2eRunReport> {
    let timeout = Duration::from_millis(config.request_timeout_ms);
    let client = SequencerClient::new_with_timeout(config.endpoint.clone(), timeout)
        .map_err(|e| crate::support::err(format!("invalid endpoint '{}': {e}", config.endpoint)))?;
    let ws_subscribe_url = client.ws_subscribe_url(config.from_offset);
    let domain = config.domain.eip712_domain();
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

    let mut ws = connect_async(ws_subscribe_url.as_str())
        .await
        .map(|(stream, _)| stream)
        .map_err(|e| {
            io_err(format!(
                "ws connect failed: url={ws_subscribe_url}, error={e}"
            ))
        })?;
    let mut consumed_ws_events_total = 0_u64;

    let drained_ws_backlog_events = drain_existing_ws_backlog(
        &mut ws,
        Duration::from_millis(DEFAULT_BACKLOG_DRAIN_IDLE_MS),
        Duration::from_millis(DEFAULT_BACKLOG_DRAIN_MAX_MS),
    )
    .await?;
    consumed_ws_events_total = consumed_ws_events_total.saturating_add(drained_ws_backlog_events);
    println!("drained_ws_backlog_events: {drained_ws_backlog_events}");

    let mut accepted_ack_samples = Vec::with_capacity(config.count as usize);
    let mut rejected_ack_samples = Vec::new();
    let mut e2e_samples = Vec::with_capacity(config.count as usize);
    let mut accepted = 0_u64;
    let mut rejected = 0_u64;
    let mut first_rejection: Option<String> = None;
    let mut rejection_breakdown = BTreeMap::<String, u64>::new();
    let started = now();

    let mut processed = 0_u64;
    while processed < config.count {
        let remaining = config.count.saturating_sub(processed);
        let batch_size = remaining.min(effective_concurrency as u64) as usize;

        let mut inflight = Vec::with_capacity(batch_size);
        for _ in 0..batch_size {
            let fixture = workload.next_fixture(config.max_fee, &domain)?;
            let match_key = fixture_match_key(
                fixture.expected_sender.as_str(),
                fixture.expected_data_hex.as_str(),
            );
            let client = client.clone();
            let submit_started = now();
            inflight.push(async move {
                let outcome = client.submit_tx_with_status(&fixture.request).await;
                (match_key, submit_started, submit_started.elapsed(), outcome)
            });
        }

        let mut expected_submit_starts = HashMap::<String, Vec<Instant>>::with_capacity(batch_size);
        for (match_key, submit_started, ack_latency, outcome) in join_all(inflight).await {
            match classify_rejection(outcome) {
                None => {
                    accepted = accepted.saturating_add(1);
                    accepted_ack_samples.push(ack_latency);
                    expected_submit_starts
                        .entry(match_key)
                        .or_default()
                        .push(submit_started);
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

        if !expected_submit_starts.is_empty() {
            let mut matched = wait_for_matching_user_ops(
                &mut ws,
                &mut expected_submit_starts,
                Duration::from_millis(config.max_ws_wait_ms),
            )
            .await?;
            consumed_ws_events_total =
                consumed_ws_events_total.saturating_add(matched.consumed_events);
            e2e_samples.append(&mut matched.e2e_samples);
        }

        processed = processed.saturating_add(batch_size as u64);
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
            "e2e benchmark saw {rejected} rejection(s): {reason}"
        ))
        .into());
    }

    if accepted_ack_samples.is_empty() {
        return Err(std::io::Error::other("e2e benchmark had no accepted txs").into());
    }
    if e2e_samples.len() != accepted as usize {
        return Err(std::io::Error::other(format!(
            "e2e sample mismatch: accepted={accepted}, matched_ws_events={}",
            e2e_samples.len()
        ))
        .into());
    }

    let total_wall = started.elapsed();
    let ack_stats = summarize(accepted_ack_samples.as_slice())?;
    let e2e_stats = summarize(e2e_samples.as_slice())?;
    let rejected_stats = if rejected_ack_samples.is_empty() {
        None
    } else {
        Some(summarize(rejected_ack_samples.as_slice())?)
    };

    Ok(E2eRunReport {
        count: config.count,
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
        ack_latency_accepted: ack_stats,
        ack_latency_rejected: rejected_stats,
        e2e_latency_accepted: e2e_stats,
        memory: None,
        sequencer_log_path: None,
    })
}

struct MatchResult {
    e2e_samples: Vec<Duration>,
    consumed_events: u64,
}

async fn wait_for_matching_user_ops(
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    expected_submit_starts: &mut HashMap<String, Vec<Instant>>,
    max_wait: Duration,
) -> BenchResult<MatchResult> {
    let deadline = tokio::time::Instant::now() + max_wait;
    let expected_total: usize = expected_submit_starts.values().map(Vec::len).sum();
    let mut e2e_samples = Vec::with_capacity(expected_total);
    let mut consumed_events = 0_u64;

    while expected_submit_starts
        .values()
        .any(|entries| !entries.is_empty())
    {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            let pending: usize = expected_submit_starts.values().map(Vec::len).sum();
            return Err(io_err(format!(
                "timed out waiting for {pending} ws event(s)"
            )));
        }
        let remaining = deadline - now;
        let maybe_frame = tokio::time::timeout(remaining, ws.next())
            .await
            .map_err(|_| io_err("ws timeout"))?;
        let frame = maybe_frame
            .ok_or_else(|| io_err("ws stream closed"))?
            .map_err(|err| io_err(format!("ws frame read failed: {err}")))?;

        let Message::Text(text) = frame else {
            continue;
        };
        let event: WsTxMessage = serde_json::from_str(text.as_str())?;
        consumed_events = consumed_events.saturating_add(1);

        if let WsTxMessage::UserOp { sender, data, .. } = event {
            let key = event_match_key(sender.as_str(), data.as_str());
            if let Some(entries) = expected_submit_starts.get_mut(key.as_str())
                && let Some(submit_started) = entries.pop()
            {
                e2e_samples.push(submit_started.elapsed());
            }
        }
    }

    Ok(MatchResult {
        e2e_samples,
        consumed_events,
    })
}

async fn drain_existing_ws_backlog(
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
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

fn event_match_key(sender: &str, data_hex: &str) -> String {
    fixture_match_key(sender, data_hex)
}
