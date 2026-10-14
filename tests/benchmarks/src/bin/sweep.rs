// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::Address;
use benchmarks::{
    AckRunConfig, BenchResult, DEFAULT_ENDPOINT, DEFAULT_INTERNAL_ENDPOINT,
    DEFAULT_WORKLOAD_TRANSFER_AMOUNT, DOMAIN_NAME, DOMAIN_VERSION, NETWORK_PROFILE,
    RoundTripRunConfig, SweepRow, SweepRunReport, WorkloadConfig, WorkloadKind,
    compute_capacity_summary, default_json_output_path, parse_address, print_ack_report,
    print_round_trip_report, print_sweep_report, resolve_external_benchmark_domain,
    run_ack_benchmark, run_round_trip_benchmark,
    runtime::{
        DEFAULT_MEMORY_SAMPLE_INTERVAL_MS, DEFAULT_RESULTS_DIR, DEFAULT_SEQUENCER_BIN,
        DRAIN_DEADLINE, MemorySampler, benchmark_domain, bootstrap_funded_workload,
        managed_sequencer_config, measure_drain, sample_storage, storage_report,
    },
    write_json_output,
};
use clap::Parser;
use serde_json::json;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Conservative upper bound on how many txs a single worker might send during
/// a benchmark run. Used to pre-fund accounts generously. If a worker exhausts
/// its balance it will receive rejections and retry until the deadline.
const FUNDING_ESTIMATE_TXS_PER_WORKER: u64 = 100_000;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "sweep",
    about = "ACK or round-trip latency sweep across concurrency levels",
    version,
    after_help = "Examples:\n  cargo run -p benchmarks --bin sweep -- --self-contained --duration-secs 30 --concurrency-list \"1 2 4 8 16 32 64 96\"\n  cargo run -p benchmarks --bin sweep -- --round-trip --self-contained --duration-secs 30 --concurrency-list \"1 2 4 8\"\n  cargo run -p benchmarks --bin sweep -- --endpoint http://127.0.0.1:3000 --domain-chain-id 31337 --domain-verifying-contract 0x1111111111111111111111111111111111111111 --duration-secs 30 --concurrency-range 1:96:8"
)]
struct Args {
    #[arg(long, default_value_t = 45_u64)]
    duration_secs: u64,
    #[arg(long, default_value = DEFAULT_ENDPOINT)]
    endpoint: String,
    /// The sequencer's internal listener (feed), for round-trip runs.
    #[arg(long, default_value = DEFAULT_INTERNAL_ENDPOINT)]
    internal_endpoint: String,
    #[arg(long, default_value_t = false)]
    self_contained: bool,
    #[arg(long, default_value = DEFAULT_SEQUENCER_BIN)]
    sequencer_bin: String,
    #[arg(long)]
    domain_chain_id: Option<u64>,
    #[arg(long, value_parser = parse_address)]
    domain_verifying_contract: Option<Address>,
    #[arg(long, value_enum, default_value_t = WorkloadKind::FundedTransfer)]
    workload: WorkloadKind,
    #[arg(long)]
    accounts_file: Option<String>,
    #[arg(long, default_value_t = DEFAULT_WORKLOAD_TRANSFER_AMOUNT)]
    transfer_amount: u64,
    #[arg(long, default_value_t = 2000_u16)]
    max_fee: u16,
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
    #[arg(long, default_value = DEFAULT_RESULTS_DIR)]
    results_dir: String,
    #[arg(long)]
    json_out: Option<String>,
    /// Run in round-trip mode (measures WS broadcast latency in addition to ACK).
    #[arg(long, default_value_t = false)]
    round_trip: bool,
    #[arg(long, default_value_t = 3_000_u64)]
    request_timeout_ms: u64,
    /// Max wait time (ms) for remaining WS events after workers finish (round-trip mode only).
    #[arg(long, default_value_t = 5_000_u64)]
    max_ws_wait_ms: u64,
    /// Run a warmup phase before the first sweep step.
    #[arg(long, default_value_t = 0_u64)]
    warmup_secs: u64,
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Ack,
    RoundTrip,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Ack => "ack",
            Self::RoundTrip => "round-trip",
        }
    }

    fn artifact_prefix(self) -> &'static str {
        match self {
            Self::Ack => "ack-sweep",
            Self::RoundTrip => "rt-sweep",
        }
    }
}

#[tokio::main]
async fn main() -> BenchResult<()> {
    let args = Args::parse();
    let mode = if args.round_trip {
        Mode::RoundTrip
    } else {
        Mode::Ack
    };
    let json_out = args
        .json_out
        .clone()
        .unwrap_or_else(|| default_json_output_path(&args.results_dir, mode.artifact_prefix()));
    let concurrencies = resolve_concurrency_list(&args)?;
    if concurrencies.is_empty() {
        return Err(std::io::Error::other("concurrency list cannot be empty").into());
    }

    std::fs::create_dir_all(args.results_dir.as_str())?;

    let mut managed = if args.self_contained {
        Some(
            rollups_harness::ManagedSequencer::spawn(managed_sequencer_config(
                "sweep-self-contained",
                &args.sequencer_bin,
            ))
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
        benchmark_domain(value)
    } else {
        resolve_external_benchmark_domain(args.domain_chain_id, args.domain_verifying_contract)?
    };

    let workload = WorkloadConfig {
        kind: args.workload,
        accounts_file: args.accounts_file.clone(),
        transfer_amount: args.transfer_amount,
    };

    // Fund accounts generously — we can't predict how many txs each worker
    // will send in a time-based run, so use a flat upper bound per worker.
    let max_workers = concurrencies.iter().copied().max().unwrap_or(1);
    let per_worker_counts = vec![FUNDING_ESTIMATE_TXS_PER_WORKER; max_workers];
    if let Some(runtime) = managed.as_ref() {
        bootstrap_funded_workload(runtime, &workload, &per_worker_counts, args.max_fee).await?;
    }

    let endpoint = managed
        .as_ref()
        .map(|value| value.endpoint().to_string())
        .unwrap_or_else(|| args.endpoint.clone());
    let internal_endpoint = managed
        .as_ref()
        .map(|value| value.internal_endpoint().to_string())
        .unwrap_or_else(|| args.internal_endpoint.clone());
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
        "starting {} sweep: endpoint={} self_contained={} domain_chain_id={} domain_verifying_contract={} duration={}s max_fee={} workload={} concs={:?}",
        mode.label(),
        endpoint,
        args.self_contained,
        domain.chain_id,
        domain.verifying_contract,
        args.duration_secs,
        args.max_fee,
        args.workload.as_str(),
        concurrencies
    );
    println!("host fd soft limit (ulimit -n): {}", fd_soft_limit_string());

    // Each step resumes every worker's wallet after the ops it got accepted
    // in earlier steps.
    let mut nonce_offsets = vec![0_u64; max_workers];
    let step_config =
        |concurrency: usize, duration: Duration, nonce_offsets: &[u64]| RoundTripRunConfig {
            endpoint: endpoint.clone(),
            internal_endpoint: internal_endpoint.clone(),
            domain,
            duration,
            concurrency,
            nonce_offsets: nonce_offsets[..concurrency].to_vec(),
            max_fee: args.max_fee,
            request_timeout_ms: args.request_timeout_ms,
            max_ws_wait_ms: args.max_ws_wait_ms,
            workload: workload.clone(),
        };

    if args.warmup_secs > 0 {
        let first_concurrency = concurrencies[0];
        println!(
            "running warmup: {}s ({} workers)",
            args.warmup_secs, first_concurrency
        );
        let config = step_config(
            first_concurrency,
            Duration::from_secs(args.warmup_secs),
            &nonce_offsets,
        );
        let (row, advances) = run_step(mode, config).await?;
        for (offset, advance) in nonce_offsets.iter_mut().zip(advances) {
            *offset += advance;
        }
        println!(
            "warmup complete: accepted={}, rejected={}",
            row.accepted_count, row.rejected_count
        );
    }

    // After warmup, so storage growth covers the window total_accepted counts.
    let storage_start = managed.as_ref().map(sample_storage).transpose()?;
    let duration = Duration::from_secs(args.duration_secs);
    let mut rows = Vec::new();
    let mut total_accepted = 0_u64;
    let mut run_error = None;
    for concurrency in concurrencies.iter().copied() {
        println!();
        println!("=== sweep concurrency={concurrency} ({}) ===", mode.label());
        match run_step(mode, step_config(concurrency, duration, &nonce_offsets)).await {
            Ok((row, advances)) => {
                for (offset, advance) in nonce_offsets.iter_mut().zip(advances) {
                    *offset += advance;
                }
                total_accepted = total_accepted.saturating_add(row.accepted_count);
                rows.push(row);
            }
            Err(err) => {
                if err.to_string().contains("Too many open files") {
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

    let memory = match memory_sampler {
        Some(sampler) => Some(sampler.stop(total_accepted).await?),
        None => None,
    };
    // Before shutdown: a clean close checkpoints the WAL away, and the data
    // dir is deleted with the runtime.
    let (storage, drain) = match (managed.as_ref(), storage_start) {
        (Some(runtime), Some(start)) if run_error.is_none() => {
            println!();
            println!(
                "draining sealed batches (up to {}s)",
                DRAIN_DEADLINE.as_secs()
            );
            let measured = async {
                let drain = measure_drain(runtime).await?;
                let end = sample_storage(runtime)?;
                BenchResult::Ok((storage_report(start, end, total_accepted), drain))
            }
            .await;
            match measured {
                Ok((storage, drain)) => (Some(storage), Some(drain)),
                Err(err) => {
                    run_error = Some(err);
                    (None, None)
                }
            }
        }
        _ => (None, None),
    };
    if let Some(value) = managed.take() {
        let shutdown_result = value.shutdown().await;
        if run_error.is_none() && shutdown_result.is_err() {
            return shutdown_result;
        }
    }
    if let Some(err) = run_error {
        return Err(err);
    }

    let report = SweepRunReport {
        summary: compute_capacity_summary(rows.as_slice()),
        rows,
        memory,
        storage,
        drain,
        sequencer_log_path,
    };

    println!();
    print_sweep_report(&report);

    let config_json = json!({
        "mode": mode.label(),
        "endpoint": endpoint,
        "internal_endpoint": internal_endpoint,
        "self_contained": args.self_contained,
        "domain_name": DOMAIN_NAME,
        "domain_version": DOMAIN_VERSION,
        "domain_chain_id": domain.chain_id,
        "domain_verifying_contract": domain.verifying_contract.to_string(),
        "duration_secs": args.duration_secs,
        "warmup_secs": args.warmup_secs,
        "max_fee": args.max_fee,
        "request_timeout_ms": args.request_timeout_ms,
        "max_ws_wait_ms": args.max_ws_wait_ms,
        "results_dir": args.results_dir,
        "network_profile": NETWORK_PROFILE,
        "workload": args.workload.as_str(),
        "accounts_file": args.accounts_file,
        "transfer_amount": args.transfer_amount,
        "concurrency_list": concurrencies,
    });
    write_json_output(
        Path::new(&json_out),
        mode.artifact_prefix(),
        &config_json,
        &report,
    )?;
    println!("sweep json: {json_out}");

    Ok(())
}

/// Runs one step, warmup or measured, and prints its report. Returns the row
/// and each worker's accepted-op count.
async fn run_step(mode: Mode, config: RoundTripRunConfig) -> BenchResult<(SweepRow, Vec<u64>)> {
    match mode {
        Mode::Ack => {
            let report = run_ack_benchmark(AckRunConfig {
                endpoint: config.endpoint,
                domain: config.domain,
                duration: config.duration,
                concurrency: config.concurrency,
                nonce_offsets: config.nonce_offsets,
                max_fee: config.max_fee,
                request_timeout_ms: config.request_timeout_ms,
                workload: config.workload,
            })
            .await?;
            print_ack_report(&report);
            Ok((SweepRow::from_ack(&report), report.nonce_advances))
        }
        Mode::RoundTrip => {
            let report = run_round_trip_benchmark(config).await?;
            print_round_trip_report(&report);
            Ok((SweepRow::from_round_trip(&report), report.nonce_advances))
        }
    }
}

fn resolve_concurrency_list(args: &Args) -> BenchResult<Vec<usize>> {
    if let Some(values) = args.concurrency_list.as_ref() {
        return Ok(values.iter().copied().filter(|value| *value > 0).collect());
    }
    if let Some(range) = args.concurrency_range.as_ref() {
        return parse_concurrency_range(range);
    }
    if args.round_trip {
        Ok(vec![1, 2, 4, 8])
    } else {
        Ok(vec![1, 2, 4, 8, 16, 32, 64, 96])
    }
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
