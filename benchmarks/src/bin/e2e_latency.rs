// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use benchmarks::{
    BenchResult, DEFAULT_ENDPOINT, DEFAULT_WORKLOAD_INITIAL_BALANCE,
    DEFAULT_WORKLOAD_TRANSFER_AMOUNT, E2eRunConfig, WorkloadConfig, WorkloadKind,
    default_seed_offset, print_e2e_report, run_e2e_benchmark,
    runtime::{
        DEFAULT_MEMORY_SAMPLE_INTERVAL_MS, DEFAULT_RUNTIME_METRICS_LOG_INTERVAL_MS,
        DEFAULT_SEQUENCER_BIN, DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT_MS,
        DEFAULT_SEQUENCER_START_TIMEOUT_MS, ManagedSequencer, ManagedSequencerConfig,
        MemorySampler, default_sequencer_log_path, parse_inclusion_lane_profile_from_log,
    },
};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "e2e_latency",
    about = "end-to-end latency benchmark",
    version,
    after_help = "Examples:\n  cargo run -p benchmarks --bin e2e_latency -- --endpoint http://127.0.0.1:3000 --count 1000 --concurrency 16 --max-fee 0 --from-offset 0\n  cargo run -p benchmarks --bin e2e_latency --release -- --count 2000 --concurrency 64 --allow-rejections"
)]
struct Args {
    #[arg(long, visible_alias = "http-url", default_value = DEFAULT_ENDPOINT)]
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
    #[arg(long, visible_alias = "ws-url")]
    ws_subscribe_url: Option<String>,
    #[arg(long, default_value_t = 0_u64)]
    from_offset: u64,
    #[arg(long, default_value_t = 100_u64)]
    count: u64,
    #[arg(long, default_value_t = 1_usize)]
    concurrency: usize,
    #[arg(long)]
    seed_offset: Option<u64>,
    #[arg(long, default_value_t = 0_u32)]
    max_fee: u32,
    #[arg(long, default_value_t = 3_000_u64)]
    request_timeout_ms: u64,
    #[arg(long, default_value_t = 5_000_u64)]
    max_ws_wait_ms: u64,
    #[arg(long, default_value_t = 0_u64)]
    progress_every: u64,
    #[arg(long, default_value_t = false)]
    skip_backlog_drain: bool,
    #[arg(long, default_value_t = 25_u64)]
    backlog_drain_idle_ms: u64,
    #[arg(long, default_value_t = 2_000_u64)]
    backlog_drain_max_ms: u64,
    #[arg(long, default_value_t = false)]
    allow_rejections: bool,
    #[arg(long)]
    json_out: Option<String>,
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

impl CliWorkload {
    fn as_str(self) -> &'static str {
        match self {
            Self::Synthetic => "synthetic",
            Self::FundedTransfer => "funded-transfer",
        }
    }
}

#[derive(Debug, Serialize)]
struct E2eJsonConfig {
    endpoint: String,
    ws_subscribe_url: Option<String>,
    self_contained: bool,
    from_offset: u64,
    count: u64,
    concurrency: usize,
    max_fee: u32,
    request_timeout_ms: u64,
    max_ws_wait_ms: u64,
    allow_rejections: bool,
    workload: String,
    accounts_file: Option<String>,
    initial_balance: u64,
    transfer_amount: u64,
}

#[derive(Debug, Serialize)]
struct E2eJsonOutput {
    benchmark: &'static str,
    config: E2eJsonConfig,
    report: benchmarks::E2eRunReport,
}

#[tokio::main]
async fn main() -> BenchResult<()> {
    let args = Args::parse();
    let json_out = args.json_out.clone().or_else(|| {
        args.self_contained
            .then(|| default_json_output_path("e2e-latency"))
    });
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
                    .or_else(|| Some(default_sequencer_log_path("e2e-latency-self-contained"))),
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
    let ws_subscribe_url = if args.self_contained {
        managed.as_ref().map(|value| value.ws_subscribe_url.clone())
    } else {
        args.ws_subscribe_url.clone()
    };

    println!(
        "e2e config: endpoint={}, self_contained={}, ws_subscribe_url={:?}, from_offset={}, count={}, concurrency={}, max_fee={}, request_timeout_ms={}, max_ws_wait_ms={}, allow_rejections={}, workload={:?}",
        endpoint,
        args.self_contained,
        ws_subscribe_url,
        args.from_offset,
        args.count,
        args.concurrency.max(1),
        args.max_fee,
        args.request_timeout_ms,
        args.max_ws_wait_ms,
        args.allow_rejections,
        args.workload
    );

    let memory_sampler = managed.as_ref().and_then(|value| value.pid()).map(|pid| {
        MemorySampler::start(pid, Duration::from_millis(args.memory_sample_interval_ms))
    });

    let config = E2eRunConfig {
        endpoint,
        ws_subscribe_url,
        from_offset: args.from_offset,
        count: args.count,
        concurrency: args.concurrency.max(1),
        seed_offset: args.seed_offset.unwrap_or_else(default_seed_offset),
        max_fee: args.max_fee,
        request_timeout_ms: args.request_timeout_ms,
        max_ws_wait_ms: args.max_ws_wait_ms,
        progress_every: args.progress_every,
        drain_backlog_before_bench: !args.skip_backlog_drain,
        backlog_drain_idle_ms: args.backlog_drain_idle_ms,
        backlog_drain_max_ms: args.backlog_drain_max_ms,
        fail_on_rejection: !args.allow_rejections,
        workload: WorkloadConfig {
            kind: args.workload.into(),
            accounts_file: args.accounts_file.clone(),
            initial_balance: args.initial_balance,
            transfer_amount: args.transfer_amount,
        },
    };

    let mut report_result = run_e2e_benchmark(config).await;
    if let Some(path) = managed
        .as_ref()
        .map(|value| value.log_path().to_string_lossy().to_string())
        && let Ok(report) = report_result.as_mut()
    {
        report.sequencer_log_path = Some(path);
    }
    if let Some(sampler) = memory_sampler {
        match report_result.as_mut() {
            Ok(report) => report.memory = Some(sampler.stop(report.accepted).await?),
            Err(_) => {
                let _ = sampler.stop(0).await;
            }
        }
    }
    if let Some(value) = managed.take() {
        let shutdown_result = value.shutdown().await;
        if let Err(err) = shutdown_result
            && report_result.is_ok()
        {
            return Err(err);
        }
    }

    if let Some(path) = report_result
        .as_ref()
        .ok()
        .and_then(|report| report.sequencer_log_path.clone())
        && let Some(profile) = parse_inclusion_lane_profile_from_log(PathBuf::from(path).as_path())?
        && let Ok(report) = report_result.as_mut()
    {
        report.inclusion_lane_profile = Some(profile);
    }

    let report = report_result?;
    print_e2e_report(&report);
    if let Some(path) = json_out.as_ref() {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = E2eJsonOutput {
            benchmark: "e2e_latency",
            config: E2eJsonConfig {
                endpoint: report.endpoint.clone(),
                ws_subscribe_url: if args.self_contained {
                    Some(report.ws_subscribe_url.clone())
                } else {
                    args.ws_subscribe_url.clone()
                },
                self_contained: args.self_contained,
                from_offset: args.from_offset,
                count: args.count,
                concurrency: args.concurrency.max(1),
                max_fee: args.max_fee,
                request_timeout_ms: args.request_timeout_ms,
                max_ws_wait_ms: args.max_ws_wait_ms,
                allow_rejections: args.allow_rejections,
                workload: args.workload.as_str().to_string(),
                accounts_file: args.accounts_file.clone(),
                initial_balance: args.initial_balance,
                transfer_amount: args.transfer_amount,
            },
            report,
        };
        fs::write(path, serde_json::to_vec_pretty(&payload)?)?;
        println!("e2e json: {path}");
    }
    Ok(())
}

fn default_json_output_path(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    format!("benchmarks/results/{prefix}-{ts}.json")
}
