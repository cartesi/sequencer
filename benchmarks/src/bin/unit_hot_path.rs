// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use benchmarks::{
    BenchResult, default_domain, make_signed_fixture, now, print_stats, summarize,
    throughput_tx_per_s,
};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "unit_hot_path",
    about = "unit benchmark for signing + request JSON encoding",
    version,
    after_help = "Examples:\n  cargo run -p benchmarks --bin unit_hot_path -- --count 10000 --max-fee 0\n  cargo run -p benchmarks --bin unit_hot_path --release -- --count 50000"
)]
struct Args {
    #[arg(long, default_value_t = 10_000_u64)]
    count: u64,
    #[arg(long, default_value_t = 0_u32)]
    max_fee: u32,
}

fn main() -> BenchResult<()> {
    let args = Args::parse();
    println!(
        "unit config: count={}, max_fee={}",
        args.count, args.max_fee
    );
    let domain = default_domain();

    let mut fixture_build_samples = Vec::with_capacity(args.count as usize);
    let mut json_encode_samples = Vec::with_capacity(args.count as usize);
    let started = now();

    for i in 0..args.count {
        let build_started = now();
        let fixture = make_signed_fixture(i, args.max_fee, &domain)?;
        fixture_build_samples.push(build_started.elapsed());

        let json_started = now();
        let _json = serde_json::to_string(&fixture.request)?;
        json_encode_samples.push(json_started.elapsed());
    }

    let total_wall = started.elapsed();
    let build_stats = summarize(fixture_build_samples.as_slice())?;
    let json_stats = summarize(json_encode_samples.as_slice())?;

    println!("unit hot-path benchmark completed: count={}", args.count);
    println!(
        "unit_ops_per_s: {:.2}",
        throughput_tx_per_s(args.count as usize, total_wall)
    );
    print_stats("fixture_build (sign+encode payload)", &build_stats);
    print_stats("request_json_encode", &json_stats);
    Ok(())
}
