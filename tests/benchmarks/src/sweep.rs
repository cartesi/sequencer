// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{
    AckRunReport, RoundTripRunReport,
    rejection::{
        client_failure_count, has_http_429, has_http_rejection, http_429_count,
        http_rejection_count,
    },
    report::print_memory_report,
    runtime,
    stats::{StatsMs, throughput_tx_per_s},
};

/// One measured step of a sweep. `round_trip_ms` is set only in round-trip
/// mode.
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
    pub ack_ms: StatsMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_trip_ms: Option<StatsMs>,
    pub rejection_breakdown: BTreeMap<String, u64>,
}

impl SweepRow {
    pub fn from_ack(report: &AckRunReport) -> Self {
        Self::new(
            report.concurrency,
            throughput_tx_per_s(report.accepted as usize, report.total_wall),
            report.accepted,
            report.rejected,
            report.rejection_rate,
            report.rejection_breakdown.clone(),
            report.ack_latency_accepted_ms.clone(),
        )
    }

    pub fn from_round_trip(report: &RoundTripRunReport) -> Self {
        Self {
            round_trip_ms: Some(report.round_trip_latency_accepted_ms.clone()),
            ..Self::new(
                report.concurrency,
                throughput_tx_per_s(report.accepted as usize, report.total_wall),
                report.accepted,
                report.rejected,
                report.rejection_rate,
                report.rejection_breakdown.clone(),
                report.ack_latency_accepted_ms.clone(),
            )
        }
    }

    fn new(
        concurrency: usize,
        accepted_tps: f64,
        accepted_count: u64,
        rejected_count: u64,
        rejection_rate: f64,
        rejection_breakdown: BTreeMap<String, u64>,
        ack_ms: StatsMs,
    ) -> Self {
        Self {
            concurrency,
            accepted_tps,
            accepted_count,
            rejected_count,
            http_rejected_count: http_rejection_count(&rejection_breakdown),
            http_429_count: http_429_count(&rejection_breakdown),
            client_failure_count: client_failure_count(rejected_count, &rejection_breakdown),
            rejection_rate,
            ack_ms,
            round_trip_ms: None,
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
    use super::{SweepRow, compute_capacity_summary};
    use crate::stats::StatsMs;
    use std::collections::BTreeMap;

    fn ms(value: f64) -> StatsMs {
        StatsMs {
            count: 1,
            min_ms: value,
            max_ms: value,
            mean_ms: value,
            p50_ms: value,
            p95_ms: value,
            p99_ms: value,
            p999_ms: value,
        }
    }

    #[test]
    fn capacity_summary_tracks_http_and_client_failures_separately() {
        let rows = vec![
            SweepRow::new(1, 10.0, 100, 0, 0.0, BTreeMap::new(), ms(1.0)),
            SweepRow::new(
                2,
                20.0,
                100,
                1,
                1.0,
                BTreeMap::from([("io_connect".to_string(), 1_u64)]),
                ms(2.0),
            ),
            SweepRow::new(
                3,
                30.0,
                100,
                1,
                1.0,
                BTreeMap::from([("http_429".to_string(), 1_u64)]),
                ms(3.0),
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
