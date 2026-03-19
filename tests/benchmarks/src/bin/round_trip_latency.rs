// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use alloy_primitives::Address;
use benchmarks::{
    BenchResult, DEFAULT_ENDPOINT, DEFAULT_WORKLOAD_TRANSFER_AMOUNT, DOMAIN_NAME, DOMAIN_VERSION,
    NetworkProfile, RoundTripRunConfig, WorkloadConfig, WorkloadKind, default_json_output_path,
    default_seed_offset, evaluate_soft_confirm_target, parse_address, print_round_trip_report,
    print_target_evaluation, resolve_external_benchmark_domain, run_round_trip_benchmark,
    runtime::{
        DEFAULT_MEMORY_SAMPLE_INTERVAL_MS, DEFAULT_SEQUENCER_BIN, MemorySampler, benchmark_domain,
        bootstrap_funded_workload, managed_sequencer_config,
    },
    write_json_output,
};
use clap::Parser;
use serde_json::json;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "round_trip_latency",
    about = "round-trip latency benchmark",
    version,
    after_help = "Examples:\n  cargo run -p benchmarks --bin round_trip_latency -- --self-contained --count 1000 --concurrency 16 --max-fee 0 --from-offset 0\n  cargo run -p benchmarks --bin round_trip_latency -- --endpoint http://127.0.0.1:3000 --domain-chain-id 31337 --domain-verifying-contract 0x1111111111111111111111111111111111111111 --count 1000 --concurrency 16 --max-fee 0 --from-offset 0\n  cargo run -p benchmarks --bin round_trip_latency -- --self-contained --count 5000 --concurrency 16 --evaluate"
)]
struct Args {
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
    #[arg(long, default_value_t = false)]
    allow_rejections: bool,
    #[arg(long, default_value_t = false)]
    evaluate: bool,
    #[arg(long)]
    json_out: Option<String>,
}

#[tokio::main]
async fn main() -> BenchResult<()> {
    let args = Args::parse();
    let effective_concurrency = args.concurrency.max(1);
    let network_profile = NetworkProfile::same_host_baseline();
    let json_out = args.json_out.clone().or_else(|| {
        args.self_contained
            .then(|| default_json_output_path("round-trip-latency"))
    });
    let workload = WorkloadConfig {
        kind: args.workload,
        accounts_file: args.accounts_file.clone(),
        transfer_amount: args.transfer_amount,
    };
    let mut managed = if args.self_contained {
        Some(
            rollups_harness::ManagedSequencer::spawn(managed_sequencer_config(
                "round-trip-latency-self-contained",
                &args.sequencer_bin,
            ))
            .await?,
        )
    } else {
        None
    };
    if let Some(runtime) = managed.as_ref() {
        bootstrap_funded_workload(runtime, &workload, args.count).await?;
    }
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
    let endpoint = managed
        .as_ref()
        .map(|value| value.endpoint().to_string())
        .unwrap_or_else(|| args.endpoint.clone());

    println!(
        "round-trip config: endpoint={}, self_contained={}, domain_chain_id={}, domain_verifying_contract={}, from_offset={}, count={}, concurrency={}, max_fee={}, request_timeout_ms={}, max_ws_wait_ms={}, allow_rejections={}, evaluate={}, workload={}",
        endpoint,
        args.self_contained,
        domain.chain_id,
        domain.verifying_contract,
        args.from_offset,
        args.count,
        effective_concurrency,
        args.max_fee,
        args.request_timeout_ms,
        args.max_ws_wait_ms,
        args.allow_rejections,
        args.evaluate,
        args.workload.as_str(),
    );

    let memory_sampler = managed.as_ref().and_then(|value| value.pid()).map(|pid| {
        MemorySampler::start(
            pid,
            Duration::from_millis(DEFAULT_MEMORY_SAMPLE_INTERVAL_MS),
        )
    });

    let config = RoundTripRunConfig {
        endpoint,
        domain,
        from_offset: args.from_offset,
        count: args.count,
        concurrency: effective_concurrency,
        seed_offset: args.seed_offset.unwrap_or_else(default_seed_offset),
        max_fee: args.max_fee,
        request_timeout_ms: args.request_timeout_ms,
        max_ws_wait_ms: args.max_ws_wait_ms,
        fail_on_rejection: !args.allow_rejections,
        workload: workload.clone(),
    };

    let mut report_result = run_round_trip_benchmark(config).await;
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

    let report = report_result?;
    let evaluation = args
        .evaluate
        .then(|| evaluate_soft_confirm_target(&report, network_profile.clone()));

    print_round_trip_report(&report);
    if let Some(value) = evaluation.as_ref() {
        print_target_evaluation(value);
    }

    if let Some(path) = json_out.as_ref() {
        let config_json = json!({
            "endpoint": report.endpoint,
            "self_contained": args.self_contained,
            "domain_name": DOMAIN_NAME,
            "domain_version": DOMAIN_VERSION,
            "domain_chain_id": domain.chain_id,
            "domain_verifying_contract": domain.verifying_contract.to_string(),
            "from_offset": args.from_offset,
            "count": args.count,
            "concurrency": effective_concurrency,
            "max_fee": args.max_fee,
            "round_trip_request_timeout_ms": args.request_timeout_ms,
            "round_trip_max_ws_wait_ms": args.max_ws_wait_ms,
            "allow_rejections": args.allow_rejections,
            "evaluation_requested": args.evaluate,
            "network_profile": network_profile,
            "workload": args.workload.as_str(),
            "accounts_file": args.accounts_file,
            "transfer_amount": args.transfer_amount,
        });
        write_json_output(
            Path::new(path),
            "round_trip_latency",
            &config_json,
            &report,
            evaluation.as_ref(),
        )?;
        println!("round-trip json: {path}");
    }
    Ok(())
}
