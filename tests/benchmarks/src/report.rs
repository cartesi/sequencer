// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    AckRunReport, BenchResult, RoundTripRunReport, TargetEvaluation, runtime,
    stats::{format_optional_f64, print_stats, throughput_tx_per_s},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkJsonOutput<R, E = TargetEvaluation> {
    pub benchmark: String,
    pub config: Value,
    pub report: R,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<E>,
}

pub fn write_json_output<C, R, E>(
    path: &Path,
    benchmark: &str,
    config: &C,
    report: &R,
    evaluation: Option<&E>,
) -> BenchResult<()>
where
    C: Serialize,
    R: Serialize,
    E: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = BenchmarkJsonOutput {
        benchmark: benchmark.to_string(),
        config: serde_json::to_value(config)?,
        report,
        evaluation,
    };
    fs::write(path, serde_json::to_vec_pretty(&payload)?)?;
    Ok(())
}

pub fn default_json_output_path(prefix: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    format!("{}/{prefix}-{ts}.json", runtime::DEFAULT_RESULTS_DIR)
}

pub fn print_ack_report(report: &AckRunReport) {
    println!(
        "ack benchmark completed: duration={:.1}s, endpoint={}, concurrency={}",
        report.total_wall.as_secs_f64(),
        report.endpoint,
        report.concurrency
    );
    println!("  accepted: {}", report.accepted);
    println!("  rejected: {}", report.rejected);
    println!("  rejection_rate: {:.4}%", report.rejection_rate);
    println!(
        "accepted_completed_per_s: {:.2} tx/s",
        throughput_tx_per_s(report.ack_latency_accepted.count, report.total_wall)
    );
    if let Some(reason) = report.first_rejection.as_ref() {
        println!("  first_rejection: {reason}");
    }
    if !report.rejection_breakdown.is_empty() {
        println!("  rejection_breakdown:");
        for (key, count) in &report.rejection_breakdown {
            println!("    {key}: {count}");
        }
    }
    print_stats("ack_latency_accepted", &report.ack_latency_accepted);
    if let Some(stats) = report.ack_latency_rejected.as_ref() {
        print_stats("ack_latency_rejected", stats);
    }
    if let Some(memory) = report.memory.as_ref() {
        print_memory_report(memory);
    }
    if let Some(path) = report.sequencer_log_path.as_ref() {
        println!("sequencer_log_path: {path}");
    }
}

pub fn print_round_trip_report(report: &RoundTripRunReport) {
    println!(
        "round-trip benchmark completed: duration={:.1}s, endpoint={}, ws={}, concurrency={}",
        report.total_wall.as_secs_f64(),
        report.endpoint,
        report.ws_subscribe_url,
        report.concurrency
    );
    println!("  accepted: {}", report.accepted);
    println!("  rejected: {}", report.rejected);
    println!("  rejection_rate: {:.4}%", report.rejection_rate);
    println!(
        "accepted_completed_per_s: {:.2} tx/s",
        throughput_tx_per_s(report.round_trip_latency_accepted.count, report.total_wall)
    );
    println!(
        "consumed_ws_events_total: {}",
        report.consumed_ws_events_total
    );
    if let Some(reason) = report.first_rejection.as_ref() {
        println!("  first_rejection: {reason}");
    }
    if !report.rejection_breakdown.is_empty() {
        println!("  rejection_breakdown:");
        for (key, count) in &report.rejection_breakdown {
            println!("    {key}: {count}");
        }
    }
    print_stats("ack_latency_accepted", &report.ack_latency_accepted);
    if let Some(stats) = report.ack_latency_rejected.as_ref() {
        print_stats("ack_latency_rejected", stats);
    }
    print_stats(
        "round_trip_latency_accepted",
        &report.round_trip_latency_accepted,
    );
    if let Some(memory) = report.memory.as_ref() {
        print_memory_report(memory);
    }
    if let Some(path) = report.sequencer_log_path.as_ref() {
        println!("sequencer_log_path: {path}");
    }
}

pub fn print_memory_report(memory: &runtime::MemoryReport) {
    println!("memory:");
    println!("  method: {}", memory.method);
    println!("  sample_interval_ms: {}", memory.sample_interval_ms);
    println!(
        "  rss_start_mb: {}",
        format_optional_f64(memory.rss_start_mb)
    );
    println!("  rss_peak_mb: {}", format_optional_f64(memory.rss_peak_mb));
    println!("  rss_end_mb: {}", format_optional_f64(memory.rss_end_mb));
    println!(
        "  rss_growth_mb: {}",
        format_optional_f64(memory.rss_growth_mb)
    );
    println!(
        "  rss_growth_per_1k_accepted_tx_mb: {}",
        format_optional_f64(memory.rss_growth_per_1k_accepted_tx_mb)
    );
}
