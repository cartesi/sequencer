// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod ack;
mod domain;
mod evaluation;
mod rejection;
mod report;
mod round_trip;
mod rt_sweep;
pub mod runtime;
mod stats;
mod support;
mod sweep;
mod workload;

pub use ack::{AckRunConfig, AckRunReport, run_ack_benchmark};
pub use domain::{
    BenchmarkDomain, DEFAULT_ENDPOINT, DOMAIN_NAME, DOMAIN_VERSION, parse_address,
    resolve_external_benchmark_domain,
};
pub use evaluation::{
    ACK_P99_TARGET_MS, DIAGNOSTIC_P999_MIN_ACCEPTED_COUNT, NetworkProfile, NetworkProfileKind,
    P999Confidence, SOFT_CONFIRM_P99_TARGET_MS, TARGET_EVALUATION_MIN_ACCEPTED_COUNT,
    TargetEvaluation, TargetVerdict, evaluate_ack_target, evaluate_soft_confirm_target,
    print_target_evaluation,
};
pub use report::{
    BenchmarkJsonOutput, default_json_output_path, print_ack_report, print_memory_report,
    print_round_trip_report, write_json_output,
};
pub use round_trip::{RoundTripRunConfig, RoundTripRunReport, run_round_trip_benchmark};
pub use rt_sweep::{
    RtSweepMeasurements, RtSweepRow, RtSweepRunReport, RtSweepSummary, compute_rt_sweep_summary,
    print_rt_sweep_report, write_csv as write_rt_sweep_csv,
};
pub use stats::{
    Stats, StatsMs, format_optional_f64, print_stats, rejection_rate, summarize,
    throughput_tx_per_s,
};
pub use support::trailing_number;
pub use sweep::{
    SweepMeasurements, SweepRow, SweepRunReport, SweepSummary, compute_capacity_summary,
    print_sweep_report, write_csv as write_sweep_csv,
};
pub use workload::{DEFAULT_WORKLOAD_TRANSFER_AMOUNT, WorkloadConfig, WorkloadKind};

pub type BenchResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
