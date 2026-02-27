// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use benchmarks::{
    AckRunConfig, BenchResult, DEFAULT_ENDPOINT, DEFAULT_WORKLOAD_INITIAL_BALANCE,
    DEFAULT_WORKLOAD_TRANSFER_AMOUNT, E2eRunConfig, WorkloadConfig, WorkloadKind,
    default_seed_offset, print_ack_report, print_e2e_report, run_ack_benchmark, run_e2e_benchmark,
    runtime::{
        DEFAULT_MEMORY_SAMPLE_INTERVAL_MS, DEFAULT_RUNTIME_METRICS_LOG_INTERVAL_MS,
        DEFAULT_SEQUENCER_BIN, DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT_MS,
        DEFAULT_SEQUENCER_START_TIMEOUT_MS, ManagedSequencer, ManagedSequencerConfig,
        MemorySampler, default_sequencer_log_path, parse_inclusion_lane_profile_from_log,
    },
};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SweepMode {
    #[value(name = "ack")]
    Ack,
    #[value(name = "e2e")]
    E2e,
}

impl SweepMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ack => "ack",
            Self::E2e => "e2e",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliWorkload {
    #[value(name = "synthetic")]
    Synthetic,
    #[value(name = "funded-transfer")]
    FundedTransfer,
}

impl From<CliWorkload> for WorkloadKind {
    fn from(value: CliWorkload) -> Self {
        match value {
            CliWorkload::Synthetic => Self::Synthetic,
            CliWorkload::FundedTransfer => Self::FundedTransfer,
        }
    }
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "sweep",
    about = "benchmark sweep runner",
    version,
    after_help = "Examples:\n  cargo run -p benchmarks --bin sweep -- --mode e2e --endpoint http://127.0.0.1:3000 --count 1000 --concurrency-list \"1 2 4 8 16 32 64\"\n  cargo run -p benchmarks --bin sweep -- --mode e2e --concurrency-range 1:128:8 --json-out benchmarks/results/e2e-latest.json\n  cargo run -p benchmarks --bin sweep --release -- --mode ack --count 5000 --concurrency-list \"1 2 4 8 16 32 64 96 128\""
)]
struct Args {
    #[arg(long, value_enum, default_value_t = SweepMode::E2e)]
    mode: SweepMode,
    #[arg(long, default_value_t = 1_000_u64)]
    count: u64,
    #[arg(long, visible_alias = "url", default_value = DEFAULT_ENDPOINT)]
    endpoint: String,
    #[arg(long, default_value_t = false)]
    self_contained: bool,
    #[arg(long, default_value = DEFAULT_SEQUENCER_BIN)]
    sequencer_bin: String,
    #[arg(long, default_value_t = DEFAULT_SEQUENCER_START_TIMEOUT_MS)]
    sequencer_start_timeout_ms: u64,
    #[arg(long, default_value_t = DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT_MS)]
    sequencer_shutdown_timeout_ms: u64,
    #[arg(long, default_value_t = true)]
    temp_db: bool,
    #[arg(long)]
    sequencer_log_path: Option<PathBuf>,
    #[arg(long, default_value_t = true)]
    sequencer_runtime_metrics_enabled: bool,
    #[arg(long, default_value_t = DEFAULT_RUNTIME_METRICS_LOG_INTERVAL_MS)]
    sequencer_runtime_metrics_log_interval_ms: u64,
    #[arg(long, default_value = "info")]
    sequencer_rust_log: String,
    #[arg(long, default_value_t = DEFAULT_MEMORY_SAMPLE_INTERVAL_MS)]
    memory_sample_interval_ms: u64,
    #[arg(long, value_enum, default_value_t = CliWorkload::Synthetic)]
    workload: CliWorkload,
    #[arg(long)]
    accounts_file: Option<String>,
    #[arg(long, default_value_t = DEFAULT_WORKLOAD_INITIAL_BALANCE)]
    initial_balance: u64,
    #[arg(long, default_value_t = DEFAULT_WORKLOAD_TRANSFER_AMOUNT)]
    transfer_amount: u64,
    #[arg(long, default_value_t = 0_u32)]
    max_fee: u32,
    #[arg(long, default_value_t = 0_u64)]
    from_offset: u64,
    #[arg(
        long,
        conflicts_with = "concurrency_range",
        value_delimiter = ' ',
        num_args = 1..
    )]
    concurrency_list: Option<Vec<usize>>,
    #[arg(
        long,
        conflicts_with = "concurrency_list",
        value_name = "START:END:STEP"
    )]
    concurrency_range: Option<String>,
    #[arg(long, default_value = "benchmarks/results")]
    results_dir: String,
    #[arg(long)]
    json_out: Option<String>,
    #[arg(long, default_value_t = 10_000_u64)]
    e2e_request_timeout_ms: u64,
    #[arg(long, default_value_t = 20_000_u64)]
    e2e_max_ws_wait_ms: u64,
    #[arg(long, default_value_t = false)]
    stop_on_first_non_200: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SweepRow {
    concurrency: usize,
    accepted_tps: f64,
    accepted_count: u64,
    rejected_count: u64,
    rejection_rate: f64,
    p95_ms: f64,
    p99_ms: f64,
    p999_ms: f64,
    rejection_breakdown: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize)]
struct SweepSummary {
    tps_at_first_non_200: Option<f64>,
    tps_at_first_429: Option<f64>,
    max_sustainable_tps_at_0_rejections: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct SweepJson {
    mode: String,
    endpoint: String,
    count: u64,
    max_fee: u32,
    from_offset: u64,
    rows: Vec<SweepRow>,
    summary: SweepSummary,
    memory: Option<benchmarks::runtime::MemoryReport>,
    sequencer_log_path: Option<String>,
    inclusion_lane_profile: Option<benchmarks::runtime::InclusionLaneProfileReport>,
}

#[tokio::main]
async fn main() -> BenchResult<()> {
    let args = Args::parse();
    let json_out = args.json_out.clone().or_else(|| {
        args.self_contained
            .then(|| default_json_output_path("sweep"))
    });
    let concurrencies = resolve_concurrency_list(&args)?;
    if concurrencies.is_empty() {
        return Err(std::io::Error::other("concurrency list cannot be empty").into());
    }

    fs::create_dir_all(args.results_dir.as_str())?;
    let timestamp = timestamp_string();
    let csv_path = format!(
        "{}/{}-sweep-{}.csv",
        args.results_dir,
        args.mode.as_str(),
        timestamp
    );

    let mut managed = if args.self_contained {
        Some(
            ManagedSequencer::spawn(ManagedSequencerConfig {
                sequencer_bin: args.sequencer_bin.clone(),
                start_timeout: Duration::from_millis(args.sequencer_start_timeout_ms),
                shutdown_timeout: Duration::from_millis(args.sequencer_shutdown_timeout_ms),
                temp_db: args.temp_db,
                log_path: args
                    .sequencer_log_path
                    .clone()
                    .or_else(|| Some(default_sequencer_log_path("sweep-self-contained"))),
                runtime_metrics_enabled: args.sequencer_runtime_metrics_enabled,
                runtime_metrics_log_interval: Duration::from_millis(
                    args.sequencer_runtime_metrics_log_interval_ms,
                ),
                rust_log: args.sequencer_rust_log.clone(),
            })
            .await?,
        )
    } else {
        None
    };

    let endpoint = managed
        .as_ref()
        .map(|value| value.endpoint.clone())
        .unwrap_or_else(|| args.endpoint.clone());
    let ws_subscribe_url = managed.as_ref().map(|value| value.ws_subscribe_url.clone());
    let sequencer_log_path = managed
        .as_ref()
        .map(|value| value.log_path().to_string_lossy().to_string());
    let memory_sampler = managed.as_ref().and_then(|value| value.pid()).map(|pid| {
        MemorySampler::start(pid, Duration::from_millis(args.memory_sample_interval_ms))
    });

    println!(
        "starting {} sweep: endpoint={} self_contained={} count={} max_fee={} workload={:?} stop_on_first_non_200={} concs={:?}",
        args.mode.as_str(),
        endpoint,
        args.self_contained,
        args.count,
        args.max_fee,
        args.workload,
        args.stop_on_first_non_200,
        concurrencies
    );
    println!("host fd soft limit (ulimit -n): {}", fd_soft_limit_string());

    let mut rows = Vec::new();
    let mut total_accepted = 0_u64;
    let mut current_from_offset = args.from_offset;
    let mut seed_offset = default_seed_offset();

    let workload = WorkloadConfig {
        kind: args.workload.into(),
        accounts_file: args.accounts_file.clone(),
        initial_balance: args.initial_balance,
        transfer_amount: args.transfer_amount,
    };

    let mut run_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;

    for concurrency in concurrencies.iter().copied() {
        println!();
        println!(
            "=== sweep mode={} concurrency={} ===",
            args.mode.as_str(),
            concurrency
        );

        let result = match args.mode {
            SweepMode::Ack => {
                let config = AckRunConfig {
                    endpoint: endpoint.clone(),
                    count: args.count,
                    concurrency,
                    seed_offset,
                    max_fee: args.max_fee,
                    request_timeout_ms: 3_000,
                    progress_every: 500,
                    fail_on_rejection: false,
                    workload: workload.clone(),
                };
                run_ack_benchmark(config).await.map(|report| {
                    seed_offset = seed_offset.saturating_add(args.count);
                    print_ack_report(&report);
                    SweepRow {
                        concurrency,
                        accepted_tps: tx_per_second(report.accepted as usize, report.total_wall),
                        accepted_count: report.accepted,
                        rejected_count: report.rejected,
                        rejection_rate: report.rejection_rate,
                        p95_ms: report.ack_latency_accepted.p95.as_secs_f64() * 1000.0,
                        p99_ms: report.ack_latency_accepted.p99.as_secs_f64() * 1000.0,
                        p999_ms: report.ack_latency_accepted.p999.as_secs_f64() * 1000.0,
                        rejection_breakdown: report.rejection_breakdown,
                    }
                })
            }
            SweepMode::E2e => {
                let config = E2eRunConfig {
                    endpoint: endpoint.clone(),
                    ws_subscribe_url: ws_subscribe_url.clone(),
                    from_offset: current_from_offset,
                    count: args.count,
                    concurrency,
                    seed_offset,
                    max_fee: args.max_fee,
                    request_timeout_ms: args.e2e_request_timeout_ms,
                    max_ws_wait_ms: args.e2e_max_ws_wait_ms,
                    progress_every: 500,
                    drain_backlog_before_bench: true,
                    backlog_drain_idle_ms: 25,
                    backlog_drain_max_ms: 2_000,
                    fail_on_rejection: false,
                    workload: workload.clone(),
                };
                run_e2e_benchmark(config).await.map(|report| {
                    seed_offset = seed_offset.saturating_add(args.count);
                    current_from_offset =
                        current_from_offset.saturating_add(report.consumed_ws_events_total);
                    print_e2e_report(&report);
                    SweepRow {
                        concurrency,
                        accepted_tps: tx_per_second(report.accepted as usize, report.total_wall),
                        accepted_count: report.accepted,
                        rejected_count: report.rejected,
                        rejection_rate: report.rejection_rate,
                        p95_ms: report.e2e_latency_accepted.p95.as_secs_f64() * 1000.0,
                        p99_ms: report.e2e_latency_accepted.p99.as_secs_f64() * 1000.0,
                        p999_ms: report.e2e_latency_accepted.p999.as_secs_f64() * 1000.0,
                        rejection_breakdown: report.rejection_breakdown,
                    }
                })
            }
        };

        match result {
            Ok(row) => {
                total_accepted = total_accepted.saturating_add(row.accepted_count);
                let should_stop = args.stop_on_first_non_200 && row.rejected_count > 0;
                rows.push(row);
                if should_stop {
                    println!("stopping sweep at first non-200 response");
                    break;
                }
            }
            Err(err) => {
                let message = err.to_string();
                if message.contains("Too many open files") {
                    println!();
                    println!(
                        "sweep stopped: hit file-descriptor limit at concurrency={concurrency}"
                    );
                    println!(
                        "hint: raise soft limit before running sweep (example: ulimit -n 4096)"
                    );
                    println!("or use a smaller conc_list.");
                }
                run_error = Some(err);
                break;
            }
        }
    }

    let mut memory_report = None;
    if let Some(sampler) = memory_sampler {
        memory_report = Some(sampler.stop(total_accepted).await?);
    }

    if let Some(value) = managed.take() {
        let shutdown_result = value.shutdown().await;
        if run_error.is_none() && shutdown_result.is_err() {
            return shutdown_result;
        }
    }
    let inclusion_lane_profile = if let Some(path) = sequencer_log_path.as_ref() {
        parse_inclusion_lane_profile_from_log(PathBuf::from(path).as_path())?
    } else {
        None
    };

    if let Some(err) = run_error {
        return Err(err);
    }

    write_csv(csv_path.as_str(), rows.as_slice())?;
    let summary = compute_capacity_summary(rows.as_slice());

    println!();
    println!("sweep csv: {csv_path}");
    println!(
        "tps_at_first_non_200: {}",
        format_optional(summary.tps_at_first_non_200)
    );
    println!(
        "tps_at_first_429: {}",
        format_optional(summary.tps_at_first_429)
    );
    println!(
        "max_sustainable_tps_at_0_rejections: {}",
        format_optional(summary.max_sustainable_tps_at_0_rejections)
    );
    if let Some(memory) = memory_report.as_ref() {
        benchmarks::print_memory_report(memory);
    }
    if let Some(path) = sequencer_log_path.as_ref() {
        println!("sequencer_log_path: {path}");
    }
    if let Some(profile) = inclusion_lane_profile.as_ref() {
        println!("inclusion_lane_profile:");
        println!("  samples: {}", profile.samples);
        println!(
            "  latest_user_op_app_share_pct_of_app_plus_persist: {}",
            format_optional(profile.latest_user_op_app_share_pct_of_app_plus_persist)
        );
        println!(
            "  latest_user_op_persist_share_pct_of_app_plus_persist: {}",
            format_optional(profile.latest_user_op_persist_share_pct_of_app_plus_persist)
        );
        println!(
            "  avg_user_op_app_share_pct_of_app_plus_persist: {}",
            format_optional(profile.avg_user_op_app_share_pct_of_app_plus_persist)
        );
        println!(
            "  avg_user_op_persist_share_pct_of_app_plus_persist: {}",
            format_optional(profile.avg_user_op_persist_share_pct_of_app_plus_persist)
        );
    }

    if let Some(path) = json_out.as_ref() {
        let json_path = PathBuf::from(path);
        if let Some(parent) = json_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = SweepJson {
            mode: args.mode.as_str().to_string(),
            endpoint,
            count: args.count,
            max_fee: args.max_fee,
            from_offset: args.from_offset,
            rows,
            summary,
            memory: memory_report,
            sequencer_log_path,
            inclusion_lane_profile,
        };
        fs::write(path, serde_json::to_vec_pretty(&payload)?)?;
        println!("sweep json: {path}");
    }
    Ok(())
}

fn resolve_concurrency_list(args: &Args) -> BenchResult<Vec<usize>> {
    if let Some(values) = args.concurrency_list.as_ref() {
        return Ok(values.iter().copied().filter(|value| *value > 0).collect());
    }
    if let Some(range) = args.concurrency_range.as_ref() {
        return parse_concurrency_range(range);
    }
    Ok(vec![1, 2, 4, 8, 16, 32, 64, 96, 128])
}

fn parse_concurrency_range(value: &str) -> BenchResult<Vec<usize>> {
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() != 3 {
        return Err(std::io::Error::other(format!(
            "invalid --concurrency-range '{value}', expected START:END:STEP"
        ))
        .into());
    }

    let start = parts[0].parse::<usize>().map_err(|_| {
        std::io::Error::other(format!(
            "invalid range start in --concurrency-range: '{value}'"
        ))
    })?;
    let end = parts[1].parse::<usize>().map_err(|_| {
        std::io::Error::other(format!(
            "invalid range end in --concurrency-range: '{value}'"
        ))
    })?;
    let step = parts[2].parse::<usize>().map_err(|_| {
        std::io::Error::other(format!(
            "invalid range step in --concurrency-range: '{value}'"
        ))
    })?;

    if start == 0 || end == 0 || step == 0 {
        return Err(std::io::Error::other("concurrency range values must all be > 0").into());
    }
    if start > end {
        return Err(std::io::Error::other("concurrency range start must be <= end").into());
    }

    let mut out = Vec::new();
    let mut current = start;
    while current <= end {
        out.push(current);
        current = current.saturating_add(step);
        if current == usize::MAX {
            break;
        }
    }
    Ok(out)
}

fn write_csv(path: &str, rows: &[SweepRow]) -> BenchResult<()> {
    let mut out = String::from(
        "concurrency,accepted_tps,accepted_count,rejected_count,rejection_rate,p95_ms,p99_ms,p999_ms\n",
    );
    for row in rows {
        out.push_str(
            format!(
                "{},{:.6},{},{},{:.6},{:.6},{:.6},{:.6}\n",
                row.concurrency,
                row.accepted_tps,
                row.accepted_count,
                row.rejected_count,
                row.rejection_rate,
                row.p95_ms,
                row.p99_ms,
                row.p999_ms,
            )
            .as_str(),
        );
    }
    fs::write(path, out)?;
    Ok(())
}

fn compute_capacity_summary(rows: &[SweepRow]) -> SweepSummary {
    let tps_at_first_non_200 = rows
        .iter()
        .find(|row| row.rejected_count > 0)
        .map(|row| row.accepted_tps);

    let tps_at_first_429 = rows
        .iter()
        .find(|row| {
            row.rejection_breakdown
                .get("http_429")
                .copied()
                .unwrap_or(0)
                > 0
        })
        .map(|row| row.accepted_tps);

    let max_sustainable_tps_at_0_rejections = rows
        .iter()
        .filter(|row| row.rejected_count == 0)
        .map(|row| row.accepted_tps)
        .max_by(|a, b| a.total_cmp(b));

    SweepSummary {
        tps_at_first_non_200,
        tps_at_first_429,
        max_sustainable_tps_at_0_rejections,
    }
}

fn tx_per_second(count: usize, total_wall: std::time::Duration) -> f64 {
    if total_wall.is_zero() {
        0.0
    } else {
        count as f64 / total_wall.as_secs_f64()
    }
}

fn timestamp_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    secs.to_string()
}

fn default_json_output_path(prefix: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    format!("benchmarks/results/{prefix}-{ts}.json")
}

fn format_optional(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.2}"),
        None => "not reached".to_string(),
    }
}

fn fd_soft_limit_string() -> String {
    #[cfg(unix)]
    {
        let out = Command::new("sh")
            .arg("-c")
            .arg("ulimit -n")
            .output()
            .ok()
            .and_then(|value| String::from_utf8(value.stdout).ok())
            .map(|value| value.trim().to_string());
        out.unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(not(unix))]
    {
        "n/a".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{SweepRow, compute_capacity_summary};
    use std::collections::BTreeMap;

    #[test]
    fn capacity_summary_equal_case() {
        let rows = vec![
            SweepRow {
                concurrency: 1,
                accepted_tps: 10.0,
                accepted_count: 100,
                rejected_count: 0,
                rejection_rate: 0.0,
                p95_ms: 1.0,
                p99_ms: 1.0,
                p999_ms: 1.0,
                rejection_breakdown: BTreeMap::new(),
            },
            SweepRow {
                concurrency: 2,
                accepted_tps: 20.0,
                accepted_count: 100,
                rejected_count: 1,
                rejection_rate: 1.0,
                p95_ms: 2.0,
                p99_ms: 2.0,
                p999_ms: 2.0,
                rejection_breakdown: BTreeMap::from([("http_429".to_string(), 1_u64)]),
            },
        ];

        let summary = compute_capacity_summary(rows.as_slice());
        assert_eq!(summary.tps_at_first_non_200, Some(20.0));
        assert_eq!(summary.tps_at_first_429, Some(20.0));
        assert_eq!(summary.max_sustainable_tps_at_0_rejections, Some(10.0));
    }

    #[test]
    fn capacity_summary_diverging_case() {
        let rows = vec![
            SweepRow {
                concurrency: 1,
                accepted_tps: 10.0,
                accepted_count: 100,
                rejected_count: 0,
                rejection_rate: 0.0,
                p95_ms: 1.0,
                p99_ms: 1.0,
                p999_ms: 1.0,
                rejection_breakdown: BTreeMap::new(),
            },
            SweepRow {
                concurrency: 2,
                accepted_tps: 18.0,
                accepted_count: 100,
                rejected_count: 1,
                rejection_rate: 1.0,
                p95_ms: 1.5,
                p99_ms: 1.5,
                p999_ms: 1.5,
                rejection_breakdown: BTreeMap::from([("http_422".to_string(), 1_u64)]),
            },
            SweepRow {
                concurrency: 4,
                accepted_tps: 25.0,
                accepted_count: 100,
                rejected_count: 1,
                rejection_rate: 1.0,
                p95_ms: 2.0,
                p99_ms: 2.0,
                p999_ms: 2.0,
                rejection_breakdown: BTreeMap::from([("http_429".to_string(), 1_u64)]),
            },
        ];

        let summary = compute_capacity_summary(rows.as_slice());
        assert_eq!(summary.tps_at_first_non_200, Some(18.0));
        assert_eq!(summary.tps_at_first_429, Some(25.0));
        assert_eq!(summary.max_sustainable_tps_at_0_rejections, Some(10.0));
    }
}
