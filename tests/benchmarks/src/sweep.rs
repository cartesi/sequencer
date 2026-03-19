// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::{
    BenchResult,
    rejection::{
        client_failure_count, has_http_429, has_http_rejection, http_429_count,
        http_rejection_count,
    },
    report::print_memory_report,
    runtime,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepRow {
    pub concurrency: usize,
    pub accepted_tps: f64,
    pub accepted_count: u64,
    pub rejected_count: u64,
    pub http_rejected_count: u64,
    pub http_429_count: u64,
    pub client_failure_count: u64,
    pub rejection_rate: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    pub rejection_breakdown: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepMeasurements {
    pub accepted_count: u64,
    pub rejected_count: u64,
    pub rejection_rate: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
    pub rejection_breakdown: BTreeMap<String, u64>,
}

impl SweepRow {
    pub fn new(concurrency: usize, accepted_tps: f64, metrics: SweepMeasurements) -> Self {
        let SweepMeasurements {
            accepted_count,
            rejected_count,
            rejection_rate,
            p95_ms,
            p99_ms,
            p999_ms,
            rejection_breakdown,
        } = metrics;
        let http_rejected_count = http_rejection_count(&rejection_breakdown);
        let http_429_count = http_429_count(&rejection_breakdown);
        let client_failure_count = client_failure_count(rejected_count, &rejection_breakdown);
        Self {
            concurrency,
            accepted_tps,
            accepted_count,
            rejected_count,
            http_rejected_count,
            http_429_count,
            client_failure_count,
            rejection_rate,
            p95_ms,
            p99_ms,
            p999_ms,
            rejection_breakdown,
        }
    }

    pub fn has_http_rejection(&self) -> bool {
        has_http_rejection(&self.rejection_breakdown)
    }

    pub fn has_http_429(&self) -> bool {
        has_http_429(&self.rejection_breakdown)
    }

    pub fn has_client_failure(&self) -> bool {
        self.client_failure_count > 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepSummary {
    pub tps_at_first_any_rejection: Option<f64>,
    pub tps_at_first_non_200: Option<f64>,
    pub tps_at_first_429: Option<f64>,
    pub tps_at_first_client_failure: Option<f64>,
    pub max_sustainable_tps_at_0_rejections: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepRunReport {
    pub rows: Vec<SweepRow>,
    pub summary: SweepSummary,
    pub memory: Option<runtime::MemoryReport>,
    pub sequencer_log_path: Option<String>,
}

pub fn compute_capacity_summary(rows: &[SweepRow]) -> SweepSummary {
    let tps_at_first_any_rejection = rows
        .iter()
        .find(|row| row.rejected_count > 0)
        .map(|row| row.accepted_tps);

    let tps_at_first_non_200 = rows
        .iter()
        .find(|row| row.has_http_rejection())
        .map(|row| row.accepted_tps);

    let tps_at_first_429 = rows
        .iter()
        .find(|row| row.has_http_429())
        .map(|row| row.accepted_tps);

    let tps_at_first_client_failure = rows
        .iter()
        .find(|row| row.has_client_failure())
        .map(|row| row.accepted_tps);

    let max_sustainable_tps_at_0_rejections = rows
        .iter()
        .filter(|row| row.rejected_count == 0)
        .map(|row| row.accepted_tps)
        .max_by(|a, b| a.total_cmp(b));

    SweepSummary {
        tps_at_first_any_rejection,
        tps_at_first_non_200,
        tps_at_first_429,
        tps_at_first_client_failure,
        max_sustainable_tps_at_0_rejections,
    }
}

pub fn write_csv(path: &Path, rows: &[SweepRow]) -> BenchResult<()> {
    let mut out = String::from(
        "concurrency,accepted_tps,accepted_count,rejected_count,http_rejected_count,http_429_count,client_failure_count,rejection_rate,p95_ms,p99_ms,p999_ms\n",
    );
    for row in rows {
        out.push_str(
            format!(
                "{},{:.6},{},{},{},{},{},{:.6},{:.6},{:.6},{:.6}\n",
                row.concurrency,
                row.accepted_tps,
                row.accepted_count,
                row.rejected_count,
                row.http_rejected_count,
                row.http_429_count,
                row.client_failure_count,
                row.rejection_rate,
                row.p95_ms,
                row.p99_ms,
                row.p999_ms,
            )
            .as_str(),
        );
    }
    fs::write(path, out)?;
    Ok(())
}

pub fn print_sweep_report(report: &SweepRunReport) {
    println!(
        "tps_at_first_any_rejection: {}",
        format_optional(report.summary.tps_at_first_any_rejection)
    );
    println!(
        "tps_at_first_non_200: {}",
        format_optional(report.summary.tps_at_first_non_200)
    );
    println!(
        "tps_at_first_429: {}",
        format_optional(report.summary.tps_at_first_429)
    );
    println!(
        "tps_at_first_client_failure: {}",
        format_optional(report.summary.tps_at_first_client_failure)
    );
    println!(
        "max_sustainable_tps_at_0_rejections: {}",
        format_optional(report.summary.max_sustainable_tps_at_0_rejections)
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

#[cfg(test)]
mod tests {
    use super::{SweepMeasurements, SweepRow, compute_capacity_summary};
    use std::collections::BTreeMap;

    #[test]
    fn capacity_summary_tracks_http_and_client_failures_separately() {
        let rows = vec![
            SweepRow::new(
                1,
                10.0,
                SweepMeasurements {
                    accepted_count: 100,
                    rejected_count: 0,
                    rejection_rate: 0.0,
                    p95_ms: 1.0,
                    p99_ms: 1.0,
                    p999_ms: 1.0,
                    rejection_breakdown: BTreeMap::new(),
                },
            ),
            SweepRow::new(
                2,
                20.0,
                SweepMeasurements {
                    accepted_count: 100,
                    rejected_count: 1,
                    rejection_rate: 1.0,
                    p95_ms: 2.0,
                    p99_ms: 2.0,
                    p999_ms: 2.0,
                    rejection_breakdown: BTreeMap::from([("io_connect".to_string(), 1_u64)]),
                },
            ),
            SweepRow::new(
                3,
                30.0,
                SweepMeasurements {
                    accepted_count: 100,
                    rejected_count: 1,
                    rejection_rate: 1.0,
                    p95_ms: 3.0,
                    p99_ms: 3.0,
                    p999_ms: 3.0,
                    rejection_breakdown: BTreeMap::from([("http_429".to_string(), 1_u64)]),
                },
            ),
        ];

        let summary = compute_capacity_summary(rows.as_slice());
        assert_eq!(summary.max_sustainable_tps_at_0_rejections, Some(10.0));
        assert_eq!(summary.tps_at_first_any_rejection, Some(20.0));
        assert_eq!(summary.tps_at_first_client_failure, Some(20.0));
        assert_eq!(summary.tps_at_first_non_200, Some(30.0));
        assert_eq!(summary.tps_at_first_429, Some(30.0));
    }
}
