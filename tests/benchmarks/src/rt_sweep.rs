// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::{BenchResult, report::print_memory_report, runtime};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtSweepRow {
    pub concurrency: usize,
    pub accepted_tps: f64,
    pub accepted_count: u64,
    pub rejected_count: u64,
    pub rejection_rate: f64,
    pub ack_p50_ms: f64,
    pub ack_p99_ms: f64,
    pub rt_p50_ms: f64,
    pub rt_p99_ms: f64,
    pub rt_p999_ms: f64,
    pub rejection_breakdown: BTreeMap<String, u64>,
}

#[derive(Debug, Clone)]
pub struct RtSweepMeasurements {
    pub accepted_count: u64,
    pub rejected_count: u64,
    pub rejection_rate: f64,
    pub ack_p50_ms: f64,
    pub ack_p99_ms: f64,
    pub rt_p50_ms: f64,
    pub rt_p99_ms: f64,
    pub rt_p999_ms: f64,
    pub rejection_breakdown: BTreeMap<String, u64>,
}

impl RtSweepRow {
    pub fn new(concurrency: usize, accepted_tps: f64, m: RtSweepMeasurements) -> Self {
        Self {
            concurrency,
            accepted_tps,
            accepted_count: m.accepted_count,
            rejected_count: m.rejected_count,
            rejection_rate: m.rejection_rate,
            ack_p50_ms: m.ack_p50_ms,
            ack_p99_ms: m.ack_p99_ms,
            rt_p50_ms: m.rt_p50_ms,
            rt_p99_ms: m.rt_p99_ms,
            rt_p999_ms: m.rt_p999_ms,
            rejection_breakdown: m.rejection_breakdown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtSweepSummary {
    pub max_sustainable_tps_at_0_rejections: Option<f64>,
    pub tps_at_first_any_rejection: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtSweepRunReport {
    pub rows: Vec<RtSweepRow>,
    pub summary: RtSweepSummary,
    pub memory: Option<runtime::MemoryReport>,
    pub sequencer_log_path: Option<String>,
}

pub fn compute_rt_sweep_summary(rows: &[RtSweepRow]) -> RtSweepSummary {
    let max_sustainable_tps_at_0_rejections = rows
        .iter()
        .filter(|row| row.rejected_count == 0)
        .map(|row| row.accepted_tps)
        .max_by(|a, b| a.total_cmp(b));

    let tps_at_first_any_rejection = rows
        .iter()
        .find(|row| row.rejected_count > 0)
        .map(|row| row.accepted_tps);

    RtSweepSummary {
        max_sustainable_tps_at_0_rejections,
        tps_at_first_any_rejection,
    }
}

pub fn write_csv(path: &Path, rows: &[RtSweepRow]) -> BenchResult<()> {
    let mut out = String::from(
        "concurrency,accepted_tps,accepted_count,rejected_count,rejection_rate,ack_p50_ms,ack_p99_ms,rt_p50_ms,rt_p99_ms,rt_p999_ms\n",
    );
    for row in rows {
        out.push_str(
            format!(
                "{},{:.6},{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}\n",
                row.concurrency,
                row.accepted_tps,
                row.accepted_count,
                row.rejected_count,
                row.rejection_rate,
                row.ack_p50_ms,
                row.ack_p99_ms,
                row.rt_p50_ms,
                row.rt_p99_ms,
                row.rt_p999_ms,
            )
            .as_str(),
        );
    }
    fs::write(path, out)?;
    Ok(())
}

pub fn print_rt_sweep_report(report: &RtSweepRunReport) {
    println!(
        "max_sustainable_tps_at_0_rejections: {}",
        format_optional(report.summary.max_sustainable_tps_at_0_rejections)
    );
    println!(
        "tps_at_first_any_rejection: {}",
        format_optional(report.summary.tps_at_first_any_rejection)
    );
    if let Some(memory) = report.memory.as_ref() {
        print_memory_report(memory);
    }
    if let Some(path) = report.sequencer_log_path.as_ref() {
        println!("sequencer_log_path: {path}");
    }
}

fn format_optional(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.2}"),
        None => "not reached".to_string(),
    }
}
