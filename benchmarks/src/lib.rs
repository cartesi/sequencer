// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod ack;
mod domain;
mod e2e;
mod evaluation;
mod rejection;
mod report;
pub mod runtime;
mod stats;
mod support;
mod sweep;
mod workload;

pub use ack::{AckRunConfig, AckRunReport, run_ack_benchmark};
pub use domain::{
    BenchmarkDomain, DEFAULT_ENDPOINT, DOMAIN_NAME, DOMAIN_VERSION, SELF_CONTAINED_DOMAIN_CHAIN_ID,
    SELF_CONTAINED_DOMAIN_VERIFYING_CONTRACT, parse_address, resolve_external_benchmark_domain,
    self_contained_domain,
};
pub use e2e::{E2eRunConfig, E2eRunReport, run_e2e_benchmark};
pub use evaluation::{
    ACK_P99_TARGET_MS, DIAGNOSTIC_P999_MIN_ACCEPTED_COUNT, NetworkProfile, NetworkProfileKind,
    P999Confidence, SOFT_CONFIRM_P99_TARGET_MS, TARGET_EVALUATION_MIN_ACCEPTED_COUNT,
    TargetEvaluation, TargetVerdict, evaluate_ack_target, evaluate_soft_confirm_target,
    print_target_evaluation,
};
pub use report::{
    BenchmarkJsonOutput, default_json_output_path, print_ack_report, print_e2e_report,
    print_memory_report, write_json_output,
};
pub use stats::{Stats, print_stats, rejection_rate, summarize, throughput_tx_per_s};
pub use support::{default_seed_offset, now};
pub use sweep::{
    SweepRow, SweepRunReport, SweepSummary, compute_capacity_summary, print_sweep_report,
    write_csv as write_sweep_csv,
};
pub use workload::{
    DEFAULT_WORKLOAD_TRANSFER_AMOUNT, SignedTxFixture, WorkloadConfig, WorkloadKind,
    make_signed_fixture,
};

pub type BenchResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
