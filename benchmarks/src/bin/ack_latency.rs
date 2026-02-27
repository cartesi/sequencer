// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use benchmarks::{
    AckRunConfig, BenchResult, DEFAULT_ENDPOINT, DEFAULT_WORKLOAD_INITIAL_BALANCE,
    DEFAULT_WORKLOAD_TRANSFER_AMOUNT, WorkloadConfig, WorkloadKind, default_seed_offset,
    print_ack_report, run_ack_benchmark,
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
    name = "ack_latency",
    about = "ack latency benchmark",
    version,
    after_help = "Examples:\n  cargo run -p benchmarks --bin ack_latency -- --endpoint http://127.0.0.1:3000 --count 1000 --concurrency 32 --max-fee 0\n  cargo run -p benchmarks --bin ack_latency --release -- --count 5000 --allow-rejections"
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
    #[arg(long, default_value_t = 200_u64)]
    count: u64,
    #[arg(long, default_value_t = 1_usize)]
    concurrency: usize,
    #[arg(long)]
    seed_offset: Option<u64>,
    #[arg(long, default_value_t = 0_u32)]
    max_fee: u32,
    #[arg(long, default_value_t = 3_000_u64)]
    request_timeout_ms: u64,
    #[arg(long, default_value_t = 0_u64)]
    progress_every: u64,
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
struct AckJsonConfig {
    endpoint: String,
    self_contained: bool,
    count: u64,
    concurrency: usize,
    max_fee: u32,
    request_timeout_ms: u64,
    allow_rejections: bool,
    workload: String,
    accounts_file: Option<String>,
    initial_balance: u64,
    transfer_amount: u64,
}

#[derive(Debug, Serialize)]
struct AckJsonOutput {
    benchmark: &'static str,
    config: AckJsonConfig,
    report: benchmarks::AckRunReport,
}

#[tokio::main]
async fn main() -> BenchResult<()> {
    let args = Args::parse();
    let json_out = args.json_out.clone().or_else(|| {
        args.self_contained
            .then(|| default_json_output_path("ack-latency"))
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
                    .or_else(|| Some(default_sequencer_log_path("ack-latency-self-contained"))),
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

    println!(
        "ack config: endpoint={}, self_contained={}, count={}, concurrency={}, max_fee={}, request_timeout_ms={}, allow_rejections={}, workload={:?}",
        endpoint,
        args.self_contained,
        args.count,
        args.concurrency.max(1),
        args.max_fee,
        args.request_timeout_ms,
        args.allow_rejections,
        args.workload
    );

    let memory_sampler = managed.as_ref().and_then(|value| value.pid()).map(|pid| {
        MemorySampler::start(pid, Duration::from_millis(args.memory_sample_interval_ms))
    });

    let config = AckRunConfig {
        endpoint,
        count: args.count,
        concurrency: args.concurrency.max(1),
        seed_offset: args.seed_offset.unwrap_or_else(default_seed_offset),
        max_fee: args.max_fee,
        request_timeout_ms: args.request_timeout_ms,
        progress_every: args.progress_every,
        fail_on_rejection: !args.allow_rejections,
        workload: WorkloadConfig {
            kind: args.workload.into(),
            accounts_file: args.accounts_file.clone(),
            initial_balance: args.initial_balance,
            transfer_amount: args.transfer_amount,
        },
    };

    let mut report_result = run_ack_benchmark(config).await;
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
    print_ack_report(&report);
    if let Some(path) = json_out.as_ref() {
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = AckJsonOutput {
            benchmark: "ack_latency",
            config: AckJsonConfig {
                endpoint: report.endpoint.clone(),
                self_contained: args.self_contained,
                count: args.count,
                concurrency: args.concurrency.max(1),
                max_fee: args.max_fee,
                request_timeout_ms: args.request_timeout_ms,
                allow_rejections: args.allow_rejections,
                workload: args.workload.as_str().to_string(),
                accounts_file: args.accounts_file.clone(),
                initial_balance: args.initial_balance,
                transfer_amount: args.transfer_amount,
            },
            report,
        };
        fs::write(path, serde_json::to_vec_pretty(&payload)?)?;
        println!("ack json: {path}");
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
