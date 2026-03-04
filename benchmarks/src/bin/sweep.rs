// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::Address;
use benchmarks::{
    AckRunConfig, BenchResult, DEFAULT_ENDPOINT, DEFAULT_WORKLOAD_TRANSFER_AMOUNT, DOMAIN_NAME,
    DOMAIN_VERSION, E2eRunConfig, NetworkProfile, SweepRow, SweepRunReport, WorkloadConfig,
    WorkloadKind, compute_capacity_summary, default_json_output_path, default_seed_offset,
    parse_address, print_ack_report, print_e2e_report, print_sweep_report,
    resolve_external_benchmark_domain, run_ack_benchmark, run_e2e_benchmark,
    runtime::{
        DEFAULT_MEMORY_SAMPLE_INTERVAL_MS, DEFAULT_SEQUENCER_BIN, ManagedSequencer,
        ManagedSequencerConfig, MemorySampler,
    },
    write_json_output, write_sweep_csv,
};
use clap::{Parser, ValueEnum};
use serde_json::json;
use std::path::{Path, PathBuf};
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

#[derive(Debug, Clone, Parser)]
#[command(
    name = "sweep",
    about = "benchmark sweep runner",
    version,
    after_help = "Examples:\n  cargo run -p benchmarks --bin sweep -- --self-contained --mode e2e --count 1000 --concurrency-list \"1 2 4 8 16 32 64\"\n  cargo run -p benchmarks --bin sweep -- --endpoint http://127.0.0.1:3000 --domain-chain-id 31337 --domain-verifying-contract 0x1111111111111111111111111111111111111111 --mode e2e --count 1000 --concurrency-range 1:128:8 --json-out benchmarks/results/e2e-sweep-latest.json"
)]
struct Args {
    #[arg(long, value_enum, default_value_t = SweepMode::E2e)]
    mode: SweepMode,
    #[arg(long, default_value_t = 1_000_u64)]
    count: u64,
    #[arg(long, default_value = DEFAULT_ENDPOINT)]
    endpoint: String,
    #[arg(long, default_value_t = false)]
    self_contained: bool,
    #[arg(long, default_value = DEFAULT_SEQUENCER_BIN)]
    sequencer_bin: String,
    #[arg(long)]
    domain_chain_id: Option<u64>,
    #[arg(long, value_parser = parse_address)]
    domain_verifying_contract: Option<Address>,
    #[arg(long, value_enum, default_value_t = WorkloadKind::Synthetic)]
    workload: WorkloadKind,
    #[arg(long)]
    accounts_file: Option<String>,
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

#[tokio::main]
async fn main() -> BenchResult<()> {
    let args = Args::parse();
    let network_profile = NetworkProfile::same_host_baseline();
    let json_prefix = format!("{}-sweep", args.mode.as_str());
    let json_out = args.json_out.clone().or_else(|| {
        args.self_contained
            .then(|| default_json_output_path(json_prefix.as_str()))
    });
    let concurrencies = resolve_concurrency_list(&args)?;
    if concurrencies.is_empty() {
        return Err(std::io::Error::other("concurrency list cannot be empty").into());
    }

    std::fs::create_dir_all(args.results_dir.as_str())?;
    let timestamp = timestamp_string();
    let csv_path = PathBuf::from(format!(
        "{}/{}-sweep-{}.csv",
        args.results_dir,
        args.mode.as_str(),
        timestamp
    ));

    let mut managed = if args.self_contained {
        Some(
            ManagedSequencer::spawn(ManagedSequencerConfig {
                sequencer_bin: args.sequencer_bin.clone(),
                log_prefix: "sweep-self-contained",
            })
            .await?,
        )
    } else {
        None
    };
    let domain = if let Some(value) = managed.as_ref() {
        if args.domain_chain_id.is_some() || args.domain_verifying_contract.is_some() {
            return Err(std::io::Error::other(
                "self-contained benchmarks use the deployed local Application; remove explicit --domain-* args",
            )
            .into());
        }
        value.domain()
    } else {
        resolve_external_benchmark_domain(args.domain_chain_id, args.domain_verifying_contract)?
    };

    let endpoint = managed
        .as_ref()
        .map(|value| value.endpoint.clone())
        .unwrap_or_else(|| args.endpoint.clone());
    let sequencer_log_path = managed
        .as_ref()
        .map(|value| value.log_path().to_string_lossy().to_string());
    let memory_sampler = managed.as_ref().and_then(|value| value.pid()).map(|pid| {
        MemorySampler::start(
            pid,
            Duration::from_millis(DEFAULT_MEMORY_SAMPLE_INTERVAL_MS),
        )
    });

    println!(
        "starting {} sweep: endpoint={} self_contained={} domain_chain_id={} domain_verifying_contract={} count={} max_fee={} workload={} stop_on_first_non_200={} concs={:?}",
        args.mode.as_str(),
        endpoint,
        args.self_contained,
        domain.chain_id,
        domain.verifying_contract,
        args.count,
        args.max_fee,
        args.workload.as_str(),
        args.stop_on_first_non_200,
        concurrencies
    );
    println!("host fd soft limit (ulimit -n): {}", fd_soft_limit_string());

    let mut rows = Vec::new();
    let mut total_accepted = 0_u64;
    let mut current_from_offset = args.from_offset;
    let mut seed_offset = default_seed_offset();

    let workload = WorkloadConfig {
        kind: args.workload,
        accounts_file: args.accounts_file.clone(),
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
                    domain,
                    count: args.count,
                    concurrency,
                    seed_offset,
                    max_fee: args.max_fee,
                    request_timeout_ms: 3_000,
                    fail_on_rejection: false,
                    workload: workload.clone(),
                };
                run_ack_benchmark(config).await.map(|report| {
                    seed_offset = seed_offset.saturating_add(args.count);
                    print_ack_report(&report);
                    SweepRow::new(
                        concurrency,
                        tx_per_second(report.accepted as usize, report.total_wall),
                        report.accepted,
                        report.rejected,
                        report.rejection_rate,
                        report.ack_latency_accepted.p95.as_secs_f64() * 1000.0,
                        report.ack_latency_accepted.p99.as_secs_f64() * 1000.0,
                        report.ack_latency_accepted.p999.as_secs_f64() * 1000.0,
                        report.rejection_breakdown,
                    )
                })
            }
            SweepMode::E2e => {
                let config = E2eRunConfig {
                    endpoint: endpoint.clone(),
                    domain,
                    from_offset: current_from_offset,
                    count: args.count,
                    concurrency,
                    seed_offset,
                    max_fee: args.max_fee,
                    request_timeout_ms: args.e2e_request_timeout_ms,
                    max_ws_wait_ms: args.e2e_max_ws_wait_ms,
                    fail_on_rejection: false,
                    workload: workload.clone(),
                };
                run_e2e_benchmark(config).await.map(|report| {
                    seed_offset = seed_offset.saturating_add(args.count);
                    current_from_offset =
                        current_from_offset.saturating_add(report.consumed_ws_events_total);
                    print_e2e_report(&report);
                    SweepRow::new(
                        concurrency,
                        tx_per_second(report.accepted as usize, report.total_wall),
                        report.accepted,
                        report.rejected,
                        report.rejection_rate,
                        report.e2e_latency_accepted.p95.as_secs_f64() * 1000.0,
                        report.e2e_latency_accepted.p99.as_secs_f64() * 1000.0,
                        report.e2e_latency_accepted.p999.as_secs_f64() * 1000.0,
                        report.rejection_breakdown,
                    )
                })
            }
        };

        match result {
            Ok(row) => {
                total_accepted = total_accepted.saturating_add(row.accepted_count);
                let should_stop = args.stop_on_first_non_200 && row.has_http_rejection();
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
    if let Some(err) = run_error {
        return Err(err);
    }

    write_sweep_csv(csv_path.as_path(), rows.as_slice())?;
    let summary = compute_capacity_summary(rows.as_slice());
    let report = SweepRunReport {
        rows,
        summary,
        memory: memory_report,
        sequencer_log_path,
    };

    println!();
    println!("sweep csv: {}", csv_path.display());
    print_sweep_report(&report);

    if let Some(path) = json_out.as_ref() {
        let config_json = json!({
            "mode": args.mode.as_str(),
            "endpoint": endpoint,
            "self_contained": args.self_contained,
            "domain_name": DOMAIN_NAME,
            "domain_version": DOMAIN_VERSION,
            "domain_chain_id": domain.chain_id,
            "domain_verifying_contract": domain.verifying_contract.to_string(),
            "count": args.count,
            "max_fee": args.max_fee,
            "from_offset": args.from_offset,
            "results_dir": args.results_dir,
            "stop_on_first_non_200": args.stop_on_first_non_200,
            "network_profile": network_profile,
            "workload": args.workload.as_str(),
            "accounts_file": args.accounts_file,
            "transfer_amount": args.transfer_amount,
            "concurrency_list": concurrencies,
            "e2e_request_timeout_ms": args.e2e_request_timeout_ms,
            "e2e_max_ws_wait_ms": args.e2e_max_ws_wait_ms,
            "csv_path": csv_path,
        });
        write_json_output(
            Path::new(path),
            "sweep",
            &config_json,
            &report,
            Option::<&serde_json::Value>::None,
        )?;
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
