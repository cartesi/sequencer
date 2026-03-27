// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::Address;
use benchmarks::{
    AckRunConfig, BenchResult, DEFAULT_ENDPOINT, DEFAULT_WORKLOAD_TRANSFER_AMOUNT, DOMAIN_NAME,
    DOMAIN_VERSION, NetworkProfile, RoundTripRunConfig, RtSweepMeasurements, RtSweepRow,
    RtSweepRunReport, SweepMeasurements, SweepRow, SweepRunReport, WorkloadConfig, WorkloadKind,
    compute_capacity_summary, compute_rt_sweep_summary, default_json_output_path,
    evaluate_ack_target, parse_address, print_ack_report, print_round_trip_report,
    print_sweep_report, print_target_evaluation, resolve_external_benchmark_domain,
    run_ack_benchmark, run_round_trip_benchmark,
    runtime::{
        DEFAULT_MEMORY_SAMPLE_INTERVAL_MS, DEFAULT_RESULTS_DIR, DEFAULT_SEQUENCER_BIN,
        MemorySampler, benchmark_domain, bootstrap_funded_workload, managed_sequencer_config,
    },
    throughput_tx_per_s, write_json_output, write_rt_sweep_csv, write_sweep_csv,
};
use clap::Parser;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

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
    #[arg(long, default_value_t = 1200_u16)]
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
    /// Max wait time (ms) for remaining WS events after workers finish (round-trip mode only).
    #[arg(long, default_value_t = 5_000_u64)]
    max_ws_wait_ms: u64,
    /// Initial WS subscribe offset (round-trip mode only).
    #[arg(long, default_value_t = 0_u64)]
    from_offset: u64,
    /// Run a warmup phase before the first sweep step.
    #[arg(long, default_value_t = 0_u64)]
    warmup_secs: u64,
    /// Evaluate latency targets (single-concurrency runs).
    #[arg(long, default_value_t = false)]
    evaluate: bool,
}

#[tokio::main]
async fn main() -> BenchResult<()> {
    let args = Args::parse();
    let network_profile = NetworkProfile::same_host_baseline();
    let json_prefix = if args.round_trip {
        "rt-sweep"
    } else {
        "ack-sweep"
    };
    let json_out = args.json_out.clone().or_else(|| {
        args.self_contained
            .then(|| default_json_output_path(json_prefix))
    });
    let concurrencies = resolve_concurrency_list(&args)?;
    if concurrencies.is_empty() {
        return Err(std::io::Error::other("concurrency list cannot be empty").into());
    }

    std::fs::create_dir_all(args.results_dir.as_str())?;
    let timestamp = timestamp_string();
    let csv_path = PathBuf::from(format!(
        "{}/{}-{}.csv",
        args.results_dir, json_prefix, timestamp
    ));

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
    let sequencer_log_path = managed
        .as_ref()
        .map(|value| value.log_path().to_string_lossy().to_string());
    let memory_sampler = managed.as_ref().and_then(|value| value.pid()).map(|pid| {
        MemorySampler::start(
            pid,
            Duration::from_millis(DEFAULT_MEMORY_SAMPLE_INTERVAL_MS),
        )
    });

    let duration = Duration::from_secs(args.duration_secs);
    let mode_str = if args.round_trip { "round-trip" } else { "ack" };
    println!(
        "starting {mode_str} sweep: endpoint={} self_contained={} domain_chain_id={} domain_verifying_contract={} duration={}s max_fee={} workload={} concs={:?}",
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

    let max_workers = concurrencies.iter().copied().max().unwrap_or(1);
    let mut nonce_offsets = vec![0_u64; max_workers];
    let mut ws_from_offset = args.from_offset;
    let mut total_accepted = 0_u64;

    // Warmup phase.
    if args.warmup_secs > 0 {
        let first_concurrency = concurrencies[0];
        let warmup_duration = Duration::from_secs(args.warmup_secs);
        println!(
            "running warmup: {}s ({} workers)",
            args.warmup_secs, first_concurrency
        );

        let offsets_for_warmup = nonce_offsets[..first_concurrency].to_vec();
        if args.round_trip {
            let warmup_config = RoundTripRunConfig {
                endpoint: endpoint.clone(),
                domain,
                from_offset: ws_from_offset,
                duration: warmup_duration,
                concurrency: first_concurrency,
                nonce_offsets: offsets_for_warmup,
                max_fee: args.max_fee,
                request_timeout_ms: 3_000,
                max_ws_wait_ms: args.max_ws_wait_ms,
                workload: workload.clone(),
            };
            let warmup_report = run_round_trip_benchmark(warmup_config).await?;
            for (i, advance) in warmup_report.nonce_advances.iter().enumerate() {
                nonce_offsets[i] += advance;
            }
            ws_from_offset = ws_from_offset.saturating_add(warmup_report.consumed_ws_events_total);
            println!(
                "warmup complete: accepted={}, rejected={}",
                warmup_report.accepted, warmup_report.rejected
            );
        } else {
            let warmup_config = AckRunConfig {
                endpoint: endpoint.clone(),
                domain,
                duration: warmup_duration,
                concurrency: first_concurrency,
                nonce_offsets: offsets_for_warmup,
                max_fee: args.max_fee,
                request_timeout_ms: 3_000,
                workload: workload.clone(),
            };
            let warmup_report = run_ack_benchmark(warmup_config).await?;
            for (i, advance) in warmup_report.nonce_advances.iter().enumerate() {
                nonce_offsets[i] += advance;
            }
            println!(
                "warmup complete: accepted={}, rejected={}",
                warmup_report.accepted, warmup_report.rejected
            );
        }
    }

    let mut run_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;

    if args.round_trip {
        let mut rows = Vec::new();

        for concurrency in concurrencies.iter().copied() {
            println!();
            println!("=== sweep concurrency={concurrency} (round-trip) ===");

            let offsets_for_step = nonce_offsets[..concurrency].to_vec();

            let config = RoundTripRunConfig {
                endpoint: endpoint.clone(),
                domain,
                from_offset: ws_from_offset,
                duration,
                concurrency,
                nonce_offsets: offsets_for_step,
                max_fee: args.max_fee,
                request_timeout_ms: 3_000,
                max_ws_wait_ms: args.max_ws_wait_ms,
                workload: workload.clone(),
            };
            let result = run_round_trip_benchmark(config).await.map(|report| {
                // Accumulate nonce offsets from actual accepted counts per worker.
                for (i, advance) in report.nonce_advances.iter().enumerate() {
                    nonce_offsets[i] += advance;
                }
                ws_from_offset = ws_from_offset.saturating_add(report.consumed_ws_events_total);

                print_round_trip_report(&report);
                RtSweepRow::new(
                    concurrency,
                    throughput_tx_per_s(report.accepted as usize, report.total_wall),
                    RtSweepMeasurements {
                        accepted_count: report.accepted,
                        rejected_count: report.rejected,
                        rejection_rate: report.rejection_rate,
                        ack_p50_ms: report.ack_latency_accepted.p50.as_secs_f64() * 1000.0,
                        ack_p99_ms: report.ack_latency_accepted.p99.as_secs_f64() * 1000.0,
                        rt_p50_ms: report.round_trip_latency_accepted.p50.as_secs_f64() * 1000.0,
                        rt_p99_ms: report.round_trip_latency_accepted.p99.as_secs_f64() * 1000.0,
                        rt_p999_ms: report.round_trip_latency_accepted.p999.as_secs_f64() * 1000.0,
                        rejection_breakdown: report.rejection_breakdown,
                    },
                )
            });

            match result {
                Ok(row) => {
                    total_accepted = total_accepted.saturating_add(row.accepted_count);
                    rows.push(row);
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

        write_rt_sweep_csv(csv_path.as_path(), rows.as_slice())?;
        let summary = compute_rt_sweep_summary(rows.as_slice());
        let report = RtSweepRunReport {
            rows,
            summary,
            memory: memory_report,
            sequencer_log_path,
        };

        println!();
        println!("sweep csv: {}", csv_path.display());
        benchmarks::print_rt_sweep_report(&report);

        if let Some(path) = json_out.as_ref() {
            let config_json = json!({
                "endpoint": endpoint,
                "self_contained": args.self_contained,
                "domain_name": DOMAIN_NAME,
                "domain_version": DOMAIN_VERSION,
                "domain_chain_id": domain.chain_id,
                "domain_verifying_contract": domain.verifying_contract.to_string(),
                "duration_secs": args.duration_secs,
                "max_fee": args.max_fee,
                "results_dir": args.results_dir,
                "network_profile": network_profile,
                "workload": args.workload.as_str(),
                "accounts_file": args.accounts_file,
                "transfer_amount": args.transfer_amount,
                "concurrency_list": concurrencies,
                "csv_path": csv_path,
                "max_ws_wait_ms": args.max_ws_wait_ms,
                "warmup_secs": args.warmup_secs,
            });
            write_json_output(
                Path::new(path),
                "rt_sweep",
                &config_json,
                &report,
                Option::<&serde_json::Value>::None,
            )?;
            println!("sweep json: {path}");
        }
    } else {
        // ACK mode (default).
        let mut rows = Vec::new();

        for concurrency in concurrencies.iter().copied() {
            println!();
            println!("=== sweep concurrency={concurrency} ===");

            let offsets_for_step = nonce_offsets[..concurrency].to_vec();

            let config = AckRunConfig {
                endpoint: endpoint.clone(),
                domain,
                duration,
                concurrency,
                nonce_offsets: offsets_for_step,
                max_fee: args.max_fee,
                request_timeout_ms: 3_000,
                workload: workload.clone(),
            };
            let result = run_ack_benchmark(config).await.map(|report| {
                // Accumulate nonce offsets from actual accepted counts per worker.
                for (i, advance) in report.nonce_advances.iter().enumerate() {
                    nonce_offsets[i] += advance;
                }
                print_ack_report(&report);

                // Evaluate target if requested and this is a single-concurrency run.
                if args.evaluate && concurrencies.len() == 1 {
                    let evaluation = evaluate_ack_target(&report, network_profile.clone());
                    print_target_evaluation(&evaluation);
                }

                SweepRow::new(
                    concurrency,
                    throughput_tx_per_s(report.accepted as usize, report.total_wall),
                    SweepMeasurements {
                        accepted_count: report.accepted,
                        rejected_count: report.rejected,
                        rejection_rate: report.rejection_rate,
                        p50_ms: report.ack_latency_accepted.p50.as_secs_f64() * 1000.0,
                        p95_ms: report.ack_latency_accepted.p95.as_secs_f64() * 1000.0,
                        p99_ms: report.ack_latency_accepted.p99.as_secs_f64() * 1000.0,
                        p999_ms: report.ack_latency_accepted.p999.as_secs_f64() * 1000.0,
                        rejection_breakdown: report.rejection_breakdown,
                    },
                )
            });

            match result {
                Ok(row) => {
                    total_accepted = total_accepted.saturating_add(row.accepted_count);
                    rows.push(row);
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
                "endpoint": endpoint,
                "self_contained": args.self_contained,
                "domain_name": DOMAIN_NAME,
                "domain_version": DOMAIN_VERSION,
                "domain_chain_id": domain.chain_id,
                "domain_verifying_contract": domain.verifying_contract.to_string(),
                "duration_secs": args.duration_secs,
                "max_fee": args.max_fee,
                "results_dir": args.results_dir,
                "network_profile": network_profile,
                "workload": args.workload.as_str(),
                "accounts_file": args.accounts_file,
                "transfer_amount": args.transfer_amount,
                "concurrency_list": concurrencies,
                "csv_path": csv_path,
                "warmup_secs": args.warmup_secs,
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
