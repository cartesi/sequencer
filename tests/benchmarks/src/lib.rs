// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod ack;
mod domain;
mod rejection;
mod report;
mod round_trip;
pub mod runtime;
mod stats;
mod support;
mod sweep;
mod workload;

pub use ack::{AckRunConfig, AckRunReport, run_ack_benchmark};
pub use domain::{
    BenchmarkDomain, DEFAULT_ENDPOINT, DEFAULT_INTERNAL_ENDPOINT, parse_address,
    resolve_external_benchmark_domain,
};
pub use report::{
    BenchmarkJsonOutput, NETWORK_PROFILE, default_json_output_path, load_sweep_results,
    print_ack_report, print_memory_report, print_round_trip_report, write_json_output,
};
pub use round_trip::{RoundTripRunConfig, RoundTripRunReport, run_round_trip_benchmark};
pub use sequencer_core::{DOMAIN_NAME, DOMAIN_VERSION};
pub use stats::{
    Stats, StatsMs, format_optional_f64, print_stats, rejection_rate, summarize,
    throughput_tx_per_s,
};
pub use sweep::{
    SweepRow, SweepRunReport, SweepSummary, compute_capacity_summary, print_sweep_report,
};
pub use workload::{DEFAULT_WORKLOAD_TRANSFER_AMOUNT, WorkloadConfig, WorkloadKind};

pub type BenchResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
