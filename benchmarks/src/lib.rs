// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

pub mod runtime;

use alloy_primitives::{Address, Signature, U256};
use alloy_sol_types::{Eip712Domain, SolStruct};
use app_core::application::{Deposit, Method, Transfer, Withdrawal};
use app_core::user_op::{SignedUserOp, UserOp};
use futures_util::StreamExt;
use futures_util::future::join_all;
use k256::ecdsa::SigningKey;
use k256::ecdsa::signature::hazmat::PrehashSigner;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fs;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

pub type BenchResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:3000";
pub const DEFAULT_WORKLOAD_INITIAL_BALANCE: u64 = 1_000_000;
pub const DEFAULT_WORKLOAD_TRANSFER_AMOUNT: u64 = 1;

const ANVIL_DEFAULT_PRIVATE_KEYS: [&str; 10] = [
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
    "0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a",
    "0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba",
    "0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e",
    "0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356",
    "0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97",
    "0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
];

#[derive(Debug, Clone, Serialize)]
pub struct TxRequest {
    pub message: UserOp,
    pub signature: String,
    pub sender: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TxResponse {
    pub ok: bool,
    pub sender: String,
    pub nonce: u32,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WsTxMessage {
    UserOp {
        offset: u64,
        sender: String,
        fee: u64,
        data: String,
    },
    DirectInput {
        offset: u64,
        payload: String,
    },
}

#[derive(Debug, Clone)]
pub struct SignedTxFixture {
    pub request: TxRequest,
    pub expected_sender: String,
    pub expected_data_hex: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub count: usize,
    pub min: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub p999: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    Synthetic,
    FundedTransfer,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkloadConfig {
    pub kind: WorkloadKind,
    pub accounts_file: Option<String>,
    pub initial_balance: u64,
    pub transfer_amount: u64,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        Self {
            kind: WorkloadKind::Synthetic,
            accounts_file: None,
            initial_balance: DEFAULT_WORKLOAD_INITIAL_BALANCE,
            transfer_amount: DEFAULT_WORKLOAD_TRANSFER_AMOUNT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AckRunConfig {
    pub endpoint: String,
    pub count: u64,
    pub concurrency: usize,
    pub seed_offset: u64,
    pub max_fee: u32,
    pub request_timeout_ms: u64,
    pub progress_every: u64,
    pub fail_on_rejection: bool,
    pub workload: WorkloadConfig,
}

#[derive(Debug, Clone, Serialize)]
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
    pub inclusion_lane_profile: Option<runtime::InclusionLaneProfileReport>,
}

#[derive(Debug, Clone)]
pub struct E2eRunConfig {
    pub endpoint: String,
    pub ws_subscribe_url: Option<String>,
    pub from_offset: u64,
    pub count: u64,
    pub concurrency: usize,
    pub seed_offset: u64,
    pub max_fee: u32,
    pub request_timeout_ms: u64,
    pub max_ws_wait_ms: u64,
    pub progress_every: u64,
    pub drain_backlog_before_bench: bool,
    pub backlog_drain_idle_ms: u64,
    pub backlog_drain_max_ms: u64,
    pub fail_on_rejection: bool,
    pub workload: WorkloadConfig,
}

#[derive(Debug, Clone, Serialize)]
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
    pub inclusion_lane_profile: Option<runtime::InclusionLaneProfileReport>,
}

#[derive(Debug)]
enum SubmitTxError {
    TimeoutConnect,
    TimeoutWrite,
    TimeoutFlush,
    TimeoutRead,
    IoConnect(String),
    IoWrite(String),
    IoFlush(String),
    IoRead(String),
    Parse(String),
}

impl SubmitTxError {
    fn breakdown_key(&self) -> &'static str {
        match self {
            Self::TimeoutConnect => "timeout_connect",
            Self::TimeoutWrite => "timeout_write",
            Self::TimeoutFlush => "timeout_flush",
            Self::TimeoutRead => "timeout_read",
            Self::IoConnect(_) => "io_connect",
            Self::IoWrite(_) => "io_write",
            Self::IoFlush(_) => "io_flush",
            Self::IoRead(_) => "io_read",
            Self::Parse(_) => "parse_error",
        }
    }
}

impl std::fmt::Display for SubmitTxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimeoutConnect => write!(f, "tcp connect timeout"),
            Self::TimeoutWrite => write!(f, "tcp write timeout"),
            Self::TimeoutFlush => write!(f, "tcp flush timeout"),
            Self::TimeoutRead => write!(f, "tcp read timeout"),
            Self::IoConnect(err) => write!(f, "tcp connect failed: {err}"),
            Self::IoWrite(err) => write!(f, "tcp write failed: {err}"),
            Self::IoFlush(err) => write!(f, "tcp flush failed: {err}"),
            Self::IoRead(err) => write!(f, "tcp read failed: {err}"),
            Self::Parse(err) => write!(f, "parse failed: {err}"),
        }
    }
}

impl Error for SubmitTxError {}

struct RejectionOutcome {
    key: String,
    detail: String,
}

struct WorkloadState {
    inner: WorkloadStateInner,
}

enum WorkloadStateInner {
    Synthetic {
        next_seed: u64,
    },
    FundedTransfer {
        accounts: Vec<FundedAccount>,
        round_robin_index: usize,
        transfer_amount: u64,
    },
}

#[derive(Clone)]
struct FundedAccount {
    signing_key: SigningKey,
    sender: Address,
    next_nonce: u32,
}

impl WorkloadState {
    async fn initialize(
        config: &WorkloadConfig,
        seed_offset: u64,
        endpoint: &str,
        timeout: Duration,
        max_fee: u32,
        domain: &Eip712Domain,
    ) -> BenchResult<Self> {
        match config.kind {
            WorkloadKind::Synthetic => Ok(Self {
                inner: WorkloadStateInner::Synthetic {
                    next_seed: seed_offset,
                },
            }),
            WorkloadKind::FundedTransfer => {
                let mut accounts = load_funded_accounts(config.accounts_file.as_deref())?;
                setup_funded_accounts(
                    endpoint,
                    timeout,
                    max_fee,
                    U256::from(config.initial_balance),
                    domain,
                    accounts.as_mut_slice(),
                )
                .await?;
                Ok(Self {
                    inner: WorkloadStateInner::FundedTransfer {
                        accounts,
                        round_robin_index: 0,
                        transfer_amount: config.transfer_amount,
                    },
                })
            }
        }
    }

    fn next_fixture(
        &mut self,
        max_fee: u32,
        domain: &Eip712Domain,
    ) -> BenchResult<SignedTxFixture> {
        match &mut self.inner {
            WorkloadStateInner::Synthetic { next_seed } => {
                let fixture = make_signed_fixture(*next_seed, max_fee, domain)?;
                *next_seed = next_seed.wrapping_add(1);
                Ok(fixture)
            }
            WorkloadStateInner::FundedTransfer {
                accounts,
                round_robin_index,
                transfer_amount,
            } => {
                if accounts.is_empty() {
                    return Err(err("funded workload has zero accounts"));
                }
                let sender_index = *round_robin_index % accounts.len();
                let recipient_index = (sender_index + 1) % accounts.len();
                let recipient = accounts[recipient_index].sender;
                let sender = &mut accounts[sender_index];

                let amount =
                    U256::from((*transfer_amount).saturating_add(u64::from(sender.next_nonce)));
                let method = Method::Transfer(Transfer {
                    amount,
                    to: recipient,
                });
                let data = ssz::Encode::as_ssz_bytes(&method);
                if data.len() > SignedUserOp::MAX_METHOD_PAYLOAD_BYTES {
                    return Err(err(format!(
                        "funded transfer payload too large: {} > {}",
                        data.len(),
                        SignedUserOp::MAX_METHOD_PAYLOAD_BYTES
                    )));
                }

                let user_op = UserOp {
                    nonce: sender.next_nonce,
                    max_fee,
                    data: data.into(),
                };
                let fixture =
                    make_signed_fixture_from_signing_key(&sender.signing_key, user_op, domain)?;
                sender.next_nonce = sender.next_nonce.wrapping_add(1);
                *round_robin_index = (*round_robin_index + 1) % accounts.len();
                Ok(fixture)
            }
        }
    }

    fn concurrency_cap(&self) -> Option<usize> {
        match &self.inner {
            WorkloadStateInner::Synthetic { .. } => None,
            WorkloadStateInner::FundedTransfer { accounts, .. } => Some(accounts.len().max(1)),
        }
    }
}

pub async fn run_ack_benchmark(config: AckRunConfig) -> BenchResult<AckRunReport> {
    let domain = default_domain();
    let timeout = Duration::from_millis(config.request_timeout_ms);
    let mut workload = WorkloadState::initialize(
        &config.workload,
        config.seed_offset,
        &config.endpoint,
        timeout,
        config.max_fee,
        &domain,
    )
    .await?;
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
            let endpoint = config.endpoint.clone();
            let sent_at = now();
            inflight.push(async move {
                let outcome =
                    submit_tx_with_status(endpoint.as_str(), &fixture.request, timeout).await;
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
        if config.progress_every > 0
            && processed > 0
            && processed.is_multiple_of(config.progress_every)
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
        inclusion_lane_profile: None,
    })
}

pub fn print_ack_report(report: &AckRunReport) {
    println!(
        "ack benchmark completed: count={}, endpoint={}, concurrency={}",
        report.count, report.endpoint, report.concurrency
    );
    println!("  accepted: {}", report.accepted);
    println!("  rejected: {}", report.rejected);
    println!("  rejection_rate: {:.4}%", report.rejection_rate);
    println!(
        "accepted_completed_per_s: {:.2} tx/s",
        throughput_tx_per_s(report.ack_latency_accepted.count, report.total_wall)
    );
    if let Some(reason) = report.first_rejection.as_ref() {
        println!("  first_rejection: {reason}");
    }
    if !report.rejection_breakdown.is_empty() {
        println!("  rejection_breakdown:");
        for (key, count) in &report.rejection_breakdown {
            println!("    {key}: {count}");
        }
    }
    print_stats("ack_latency_accepted", &report.ack_latency_accepted);
    if let Some(stats) = report.ack_latency_rejected.as_ref() {
        print_stats("ack_latency_rejected", stats);
    }
    if let Some(memory) = report.memory.as_ref() {
        print_memory_report(memory);
    }
    if let Some(path) = report.sequencer_log_path.as_ref() {
        println!("sequencer_log_path: {path}");
    }
    if let Some(profile) = report.inclusion_lane_profile.as_ref() {
        println!("inclusion_lane_profile:");
        println!("  samples: {}", profile.samples);
        println!(
            "  latest_user_op_app_share_pct_of_app_plus_persist: {}",
            format_optional_f64(profile.latest_user_op_app_share_pct_of_app_plus_persist)
        );
        println!(
            "  latest_user_op_persist_share_pct_of_app_plus_persist: {}",
            format_optional_f64(profile.latest_user_op_persist_share_pct_of_app_plus_persist)
        );
        println!(
            "  avg_user_op_app_share_pct_of_app_plus_persist: {}",
            format_optional_f64(profile.avg_user_op_app_share_pct_of_app_plus_persist)
        );
        println!(
            "  avg_user_op_persist_share_pct_of_app_plus_persist: {}",
            format_optional_f64(profile.avg_user_op_persist_share_pct_of_app_plus_persist)
        );
    }
}

pub async fn run_e2e_benchmark(config: E2eRunConfig) -> BenchResult<E2eRunReport> {
    let ws_base = config
        .ws_subscribe_url
        .clone()
        .unwrap_or_else(|| default_ws_subscribe_url(config.endpoint.as_str()));
    let ws_subscribe_url = with_from_offset(ws_base.as_str(), config.from_offset);

    let timeout = Duration::from_millis(config.request_timeout_ms);
    let domain = default_domain();
    let mut workload = WorkloadState::initialize(
        &config.workload,
        config.seed_offset,
        &config.endpoint,
        timeout,
        config.max_fee,
        &domain,
    )
    .await?;
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

    let (mut ws, _response) = connect_async(ws_subscribe_url.as_str())
        .await
        .map_err(|e| {
            io_err(format!(
                "ws connect failed: url={ws_subscribe_url}, error={e}"
            ))
        })?;
    let mut consumed_ws_events_total = 0_u64;
    let mut drained_ws_backlog_events = 0_u64;

    if config.drain_backlog_before_bench {
        let drained = drain_existing_ws_backlog(
            &mut ws,
            Duration::from_millis(config.backlog_drain_idle_ms),
            Duration::from_millis(config.backlog_drain_max_ms),
        )
        .await?;
        consumed_ws_events_total = consumed_ws_events_total.saturating_add(drained);
        drained_ws_backlog_events = drained;
        println!("drained_ws_backlog_events: {drained}");
    }

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
            let endpoint = config.endpoint.clone();
            let submit_started = now();
            inflight.push(async move {
                let outcome =
                    submit_tx_with_status(endpoint.as_str(), &fixture.request, timeout).await;
                (match_key, submit_started, submit_started.elapsed(), outcome)
            });
        }

        let mut expected_submit_starts =
            HashMap::<String, Vec<std::time::Instant>>::with_capacity(batch_size);
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
        if config.progress_every > 0
            && processed > 0
            && processed.is_multiple_of(config.progress_every)
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
        inclusion_lane_profile: None,
    })
}

pub fn print_e2e_report(report: &E2eRunReport) {
    println!(
        "e2e benchmark completed: count={}, endpoint={}, ws={}, concurrency={}",
        report.count, report.endpoint, report.ws_subscribe_url, report.concurrency
    );
    println!("  accepted: {}", report.accepted);
    println!("  rejected: {}", report.rejected);
    println!("  rejection_rate: {:.4}%", report.rejection_rate);
    println!(
        "accepted_completed_per_s: {:.2} tx/s",
        throughput_tx_per_s(report.e2e_latency_accepted.count, report.total_wall)
    );
    println!(
        "consumed_ws_events_total: {}",
        report.consumed_ws_events_total
    );
    if let Some(reason) = report.first_rejection.as_ref() {
        println!("  first_rejection: {reason}");
    }
    if !report.rejection_breakdown.is_empty() {
        println!("  rejection_breakdown:");
        for (key, count) in &report.rejection_breakdown {
            println!("    {key}: {count}");
        }
    }
    print_stats("ack_latency_accepted", &report.ack_latency_accepted);
    if let Some(stats) = report.ack_latency_rejected.as_ref() {
        print_stats("ack_latency_rejected", stats);
    }
    print_stats("e2e_latency_accepted", &report.e2e_latency_accepted);
    if let Some(memory) = report.memory.as_ref() {
        print_memory_report(memory);
    }
    if let Some(path) = report.sequencer_log_path.as_ref() {
        println!("sequencer_log_path: {path}");
    }
    if let Some(profile) = report.inclusion_lane_profile.as_ref() {
        println!("inclusion_lane_profile:");
        println!("  samples: {}", profile.samples);
        println!(
            "  latest_user_op_app_share_pct_of_app_plus_persist: {}",
            format_optional_f64(profile.latest_user_op_app_share_pct_of_app_plus_persist)
        );
        println!(
            "  latest_user_op_persist_share_pct_of_app_plus_persist: {}",
            format_optional_f64(profile.latest_user_op_persist_share_pct_of_app_plus_persist)
        );
        println!(
            "  avg_user_op_app_share_pct_of_app_plus_persist: {}",
            format_optional_f64(profile.avg_user_op_app_share_pct_of_app_plus_persist)
        );
        println!(
            "  avg_user_op_persist_share_pct_of_app_plus_persist: {}",
            format_optional_f64(profile.avg_user_op_persist_share_pct_of_app_plus_persist)
        );
    }
}

pub fn print_memory_report(memory: &runtime::MemoryReport) {
    println!("memory:");
    println!("  method: {}", memory.method);
    println!("  sample_interval_ms: {}", memory.sample_interval_ms);
    println!(
        "  rss_start_mb: {}",
        format_optional_f64(memory.rss_start_mb)
    );
    println!("  rss_peak_mb: {}", format_optional_f64(memory.rss_peak_mb));
    println!("  rss_end_mb: {}", format_optional_f64(memory.rss_end_mb));
    println!(
        "  rss_growth_mb: {}",
        format_optional_f64(memory.rss_growth_mb)
    );
    println!(
        "  rss_growth_per_1k_accepted_tx_mb: {}",
        format_optional_f64(memory.rss_growth_per_1k_accepted_tx_mb)
    );
}

pub fn default_domain() -> Eip712Domain {
    Eip712Domain {
        name: Some("CartesiAppSequencer".to_string().into()),
        version: Some("1".to_string().into()),
        chain_id: Some(U256::from(1_u64)),
        verifying_contract: Some(Address::from_slice(&[0_u8; 20])),
        salt: None,
    }
}

pub fn make_signed_fixture(
    seed: u64,
    max_fee: u32,
    domain: &Eip712Domain,
) -> BenchResult<SignedTxFixture> {
    let signing_key = signing_key_for_seed(seed)?;
    let sender = address_from_signing_key(&signing_key);
    let method = Method::Withdrawal(Withdrawal {
        amount: U256::from(seed.saturating_add(1)),
    });
    let data = ssz::Encode::as_ssz_bytes(&method);
    if data.len() > SignedUserOp::MAX_METHOD_PAYLOAD_BYTES {
        return Err(err(format!(
            "benchmark payload too large: {} > {}",
            data.len(),
            SignedUserOp::MAX_METHOD_PAYLOAD_BYTES
        )));
    }
    let message = UserOp {
        nonce: 0,
        max_fee,
        data: data.clone().into(),
    };

    let fixture = make_signed_fixture_from_signing_key(&signing_key, message, domain)?;
    if fixture.expected_sender != sender.to_string() {
        return Err(err("unexpected synthetic sender mismatch"));
    }
    Ok(fixture)
}

pub async fn submit_tx(
    endpoint: &str,
    req: &TxRequest,
    timeout: Duration,
) -> BenchResult<TxResponse> {
    let (status, response_body) = submit_tx_with_status(endpoint, req, timeout)
        .await
        .map_err(|e| err(format!("tx submit failed: {e}")))?;
    if status != 200 {
        return Err(err(format!(
            "/tx rejected with status {status}: {response_body}. Hint: sequencer frame fee and payload-size bounds must allow these txs."
        )));
    }

    let parsed: TxResponse = serde_json::from_str(response_body.as_str())?;
    Ok(parsed)
}

async fn setup_funded_accounts(
    endpoint: &str,
    timeout: Duration,
    max_fee: u32,
    initial_balance: U256,
    domain: &Eip712Domain,
    accounts: &mut [FundedAccount],
) -> BenchResult<()> {
    for account in accounts {
        let method = Method::Deposit(Deposit {
            amount: initial_balance,
            to: account.sender,
        });
        let data = ssz::Encode::as_ssz_bytes(&method);
        let user_op = UserOp {
            nonce: 0,
            max_fee,
            data: data.into(),
        };
        let fixture = make_signed_fixture_from_signing_key(&account.signing_key, user_op, domain)?;
        let (status, body) = submit_tx_with_status(endpoint, &fixture.request, timeout)
            .await
            .map_err(|e| {
                err(format!(
                    "funded setup tx failed for {}: {e}",
                    account.sender
                ))
            })?;
        if status != 200 {
            return Err(err(format!(
                "funded setup tx rejected for {}: status={status}, body={body}. Hint: self-contained mode (fresh db) avoids nonce conflicts for funded workload.",
                account.sender
            )));
        }
        account.next_nonce = 1;
    }
    Ok(())
}

fn load_funded_accounts(accounts_file: Option<&str>) -> BenchResult<Vec<FundedAccount>> {
    let keys = match accounts_file {
        Some(path) => load_private_keys_from_file(path)?,
        None => ANVIL_DEFAULT_PRIVATE_KEYS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    if keys.is_empty() {
        return Err(err("no private keys available for funded workload"));
    }

    let mut accounts = Vec::with_capacity(keys.len());
    for key_hex in keys {
        let signing_key = signing_key_from_hex(key_hex.as_str())?;
        let sender = address_from_signing_key(&signing_key);
        accounts.push(FundedAccount {
            signing_key,
            sender,
            next_nonce: 0,
        });
    }
    Ok(accounts)
}

fn load_private_keys_from_file(path: &str) -> BenchResult<Vec<String>> {
    let contents = fs::read_to_string(path)
        .map_err(|e| err(format!("failed reading accounts file '{path}': {e}")))?;
    let mut keys = Vec::new();
    for line in contents.lines() {
        let mut candidate = line.trim();
        if let Some((_, rhs)) = candidate.split_once(')') {
            candidate = rhs.trim();
        }
        if candidate.starts_with("0x") && candidate.len() == 66 {
            let is_hex = candidate
                .as_bytes()
                .iter()
                .skip(2)
                .all(|b| b.is_ascii_hexdigit());
            if is_hex {
                keys.push(candidate.to_string());
            }
        }
    }
    if keys.is_empty() {
        return Err(err(format!(
            "accounts file '{path}' did not contain any 32-byte hex private keys"
        )));
    }
    Ok(keys)
}

fn signing_key_from_hex(hex: &str) -> BenchResult<SigningKey> {
    let bytes = alloy_primitives::hex::decode(hex)
        .map_err(|e| err(format!("invalid private key hex '{hex}': {e}")))?;
    if bytes.len() != 32 {
        return Err(err(format!(
            "invalid private key length: expected 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut key_bytes = [0_u8; 32];
    key_bytes.copy_from_slice(&bytes);
    SigningKey::from_bytes((&key_bytes).into())
        .map_err(|e| err(format!("invalid private key material: {e}")))
}

fn make_signed_fixture_from_signing_key(
    signing_key: &SigningKey,
    user_op: UserOp,
    domain: &Eip712Domain,
) -> BenchResult<SignedTxFixture> {
    let sender = address_from_signing_key(signing_key);
    let signature = sign_user_op(domain, &user_op, signing_key)?;
    let data = user_op.data.to_vec();
    Ok(SignedTxFixture {
        request: TxRequest {
            message: user_op,
            signature,
            sender: Some(sender.to_string()),
        },
        expected_sender: sender.to_string(),
        expected_data_hex: alloy_primitives::hex::encode_prefixed(data),
    })
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

fn classify_rejection(outcome: Result<(u16, String), SubmitTxError>) -> Option<RejectionOutcome> {
    match outcome {
        Ok((200, _body)) => None,
        Ok((status, body)) => Some(RejectionOutcome {
            key: format!("http_{status}"),
            detail: format!("status={status}, body={body}"),
        }),
        Err(err) => Some(RejectionOutcome {
            key: err.breakdown_key().to_string(),
            detail: err.to_string(),
        }),
    }
}

async fn submit_tx_with_status(
    endpoint: &str,
    req: &TxRequest,
    timeout: Duration,
) -> Result<(u16, String), SubmitTxError> {
    let (host_port, path_prefix) = parse_http_url(endpoint).map_err(SubmitTxError::Parse)?;
    let path = format!("{}/tx", path_prefix.trim_end_matches('/'));
    let body = serde_json::to_string(req).map_err(|e| SubmitTxError::Parse(e.to_string()))?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let mut stream = tokio::time::timeout(timeout, TcpStream::connect(host_port.as_str()))
        .await
        .map_err(|_| SubmitTxError::TimeoutConnect)?
        .map_err(|e| SubmitTxError::IoConnect(e.to_string()))?;

    tokio::time::timeout(timeout, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| SubmitTxError::TimeoutWrite)?
        .map_err(|e| SubmitTxError::IoWrite(e.to_string()))?;

    tokio::time::timeout(timeout, stream.flush())
        .await
        .map_err(|_| SubmitTxError::TimeoutFlush)?
        .map_err(|e| SubmitTxError::IoFlush(e.to_string()))?;

    let mut response = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = tokio::time::timeout(timeout, stream.read(&mut chunk))
            .await
            .map_err(|_| SubmitTxError::TimeoutRead)?
            .map_err(|e| SubmitTxError::IoRead(e.to_string()))?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);

        if let Some((header_end, content_length)) = response_content_len(response.as_slice())
            && response.len() >= header_end.saturating_add(content_length)
        {
            break;
        }
    }

    parse_http_response(response.as_slice()).map_err(SubmitTxError::Parse)
}

pub fn summarize(samples: &[Duration]) -> BenchResult<Stats> {
    if samples.is_empty() {
        return Err(err("cannot summarize empty sample set"));
    }

    let mut nanos: Vec<u128> = samples.iter().map(Duration::as_nanos).collect();
    nanos.sort_unstable();
    let sum: u128 = nanos.iter().copied().sum();
    let count = nanos.len();

    Ok(Stats {
        count,
        min: duration_from_nanos(nanos[0]),
        max: duration_from_nanos(nanos[count - 1]),
        mean: duration_from_nanos(sum / count as u128),
        p50: duration_from_nanos(percentile(&nanos, 0.50)),
        p95: duration_from_nanos(percentile(&nanos, 0.95)),
        p99: duration_from_nanos(percentile(&nanos, 0.99)),
        p999: duration_from_nanos(percentile(&nanos, 0.999)),
    })
}

pub fn print_stats(name: &str, stats: &Stats) {
    println!("{name}:");
    println!("  count: {}", stats.count);
    println!("  min:   {}", format_ms(stats.min));
    println!("  p50:   {}", format_ms(stats.p50));
    println!("  p95:   {}", format_ms(stats.p95));
    println!("  p99:   {}", format_ms(stats.p99));
    println!("  p99.9: {}", format_ms(stats.p999));
    println!("  max:   {}", format_ms(stats.max));
    println!("  mean:  {}", format_ms(stats.mean));
}

pub fn throughput_tx_per_s(accepted_count: usize, total_wall: Duration) -> f64 {
    if total_wall.is_zero() {
        0.0
    } else {
        accepted_count as f64 / total_wall.as_secs_f64()
    }
}

pub fn rejection_rate(accepted: u64, rejected: u64) -> f64 {
    let total = accepted.saturating_add(rejected);
    if total == 0 {
        0.0
    } else {
        (rejected as f64 / total as f64) * 100.0
    }
}

pub fn default_ws_subscribe_url(http_url: &str) -> String {
    let scheme_replaced = if let Some(rest) = http_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{}", http_url.trim_end_matches('/'))
    };
    format!("{}/ws/subscribe", scheme_replaced.trim_end_matches('/'))
}

pub fn with_from_offset(ws_subscribe_url: &str, from_offset: u64) -> String {
    let separator = if ws_subscribe_url.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{ws_subscribe_url}{separator}from_offset={from_offset}")
}

pub fn now() -> Instant {
    Instant::now()
}

pub fn default_seed_offset() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn signing_key_for_seed(seed: u64) -> BenchResult<SigningKey> {
    let mut bytes = [0_u8; 32];
    bytes[24..32].copy_from_slice(&seed.saturating_add(1).to_be_bytes());
    SigningKey::from_bytes((&bytes).into())
        .map_err(|e| err(format!("build signing key failed: {e}")))
}

fn sign_user_op(
    domain: &Eip712Domain,
    user_op: &UserOp,
    signing_key: &SigningKey,
) -> BenchResult<String> {
    let hash = user_op.eip712_signing_hash(domain);
    let k256_sig = signing_key
        .sign_prehash(hash.as_slice())
        .map_err(|e| err(format!("sign user op prehash failed: {e}")))?;

    let expected_sender = address_from_signing_key(signing_key);
    let signature = [false, true]
        .into_iter()
        .map(|parity| Signature::from_signature_and_parity(k256_sig, parity))
        .find(|candidate| {
            candidate
                .recover_address_from_prehash(&hash)
                .ok()
                .map(|sender| sender == expected_sender)
                .unwrap_or(false)
        })
        .ok_or_else(|| err("could not recover parity for signature"))?;

    Ok(alloy_primitives::hex::encode_prefixed(signature.as_bytes()))
}

fn address_from_signing_key(signing_key: &SigningKey) -> Address {
    let verifying = signing_key.verifying_key().to_encoded_point(false);
    Address::from_raw_public_key(&verifying.as_bytes()[1..])
}

fn percentile(sorted_nanos: &[u128], p: f64) -> u128 {
    let last = sorted_nanos.len() - 1;
    let rank = (p * last as f64).ceil() as usize;
    sorted_nanos[rank.min(last)]
}

fn duration_from_nanos(value: u128) -> Duration {
    let nanos = u64::try_from(value).unwrap_or(u64::MAX);
    Duration::from_nanos(nanos)
}

fn format_ms(value: Duration) -> String {
    format!("{:.3} ms", value.as_secs_f64() * 1000.0)
}

fn format_optional_f64(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.3}"),
        None => "n/a".to_string(),
    }
}

fn parse_http_url(http_url: &str) -> Result<(String, String), String> {
    let stripped = http_url
        .trim_end_matches('/')
        .strip_prefix("http://")
        .ok_or_else(|| "only http:// URLs are supported for benchmark submissions".to_string())?;

    let (host_port, path_prefix) = if let Some((host, path)) = stripped.split_once('/') {
        (host.to_string(), format!("/{}", path))
    } else {
        (stripped.to_string(), String::new())
    };
    if host_port.is_empty() {
        return Err("missing host in http URL".to_string());
    }
    Ok((host_port, path_prefix))
}

fn parse_http_response(raw: &[u8]) -> Result<(u16, String), String> {
    let text = String::from_utf8(raw.to_vec()).map_err(|e| e.to_string())?;
    let mut sections = text.splitn(2, "\r\n\r\n");
    let headers = sections.next().unwrap_or_default();
    let body = sections.next().unwrap_or_default().to_string();

    let status_line = headers
        .lines()
        .next()
        .ok_or_else(|| "missing HTTP status line".to_string())?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "missing HTTP status code".to_string())?
        .parse::<u16>()
        .map_err(|e| e.to_string())?;
    Ok((status, body))
}

fn response_content_len(raw: &[u8]) -> Option<(usize, usize)> {
    let header_end = raw.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let headers = std::str::from_utf8(&raw[..header_end]).ok()?;
    let mut content_length = None;
    for line in headers.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
            break;
        }
    }
    content_length.map(|len| (header_end, len))
}

struct MatchResult {
    e2e_samples: Vec<Duration>,
    consumed_events: u64,
}

async fn wait_for_matching_user_ops(
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    expected_submit_starts: &mut HashMap<String, Vec<std::time::Instant>>,
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

fn io_err(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(std::io::Error::other(message.into()))
}

fn err(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(std::io::Error::other(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_includes_p999() {
        let samples: Vec<Duration> = (1_u64..=10_000).map(Duration::from_micros).collect();
        let stats = summarize(samples.as_slice()).expect("summarize");
        assert_eq!(stats.count, 10_000);
        assert!(stats.p999 >= stats.p99);
        assert!(stats.p999 <= stats.max);
    }

    #[test]
    fn classify_rejection_maps_http_and_transport() {
        let http = classify_rejection(Ok((429, "overloaded".to_string()))).expect("http rejection");
        assert_eq!(http.key, "http_429");

        let transport =
            classify_rejection(Err(SubmitTxError::TimeoutRead)).expect("transport rejection");
        assert_eq!(transport.key, "timeout_read");
    }

    #[test]
    fn funded_transfer_round_robin_nonce_progression() {
        let mut accounts = Vec::new();
        for key in ANVIL_DEFAULT_PRIVATE_KEYS.iter().take(2) {
            let signing_key = signing_key_from_hex(key).expect("signing key");
            accounts.push(FundedAccount {
                sender: address_from_signing_key(&signing_key),
                signing_key,
                next_nonce: 1,
            });
        }

        let domain = default_domain();
        let mut state = WorkloadState {
            inner: WorkloadStateInner::FundedTransfer {
                accounts,
                round_robin_index: 0,
                transfer_amount: 1,
            },
        };

        let one = state.next_fixture(0, &domain).expect("fixture 1");
        let two = state.next_fixture(0, &domain).expect("fixture 2");
        let three = state.next_fixture(0, &domain).expect("fixture 3");

        assert_ne!(one.expected_sender, two.expected_sender);
        assert_eq!(one.expected_sender, three.expected_sender);
        assert_eq!(one.request.message.nonce, 1);
        assert_eq!(two.request.message.nonce, 1);
        assert_eq!(three.request.message.nonce, 2);
    }
}
