// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use serde::{Deserialize, Serialize};

use crate::{AckRunReport, E2eRunReport};

pub const TARGET_EVALUATION_MIN_ACCEPTED_COUNT: u64 = 5_000;
pub const DIAGNOSTIC_P999_MIN_ACCEPTED_COUNT: u64 = 10_000;
pub const ACK_P99_TARGET_MS: f64 = 500.0;
pub const SOFT_CONFIRM_P99_TARGET_MS: f64 = 1_000.0;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProfileKind {
    SameHostBaseline,
    CanonicalNetworkAware,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkProfile {
    pub kind: NetworkProfileKind,
    pub shaping_method: String,
    pub shaping_config: String,
}

impl NetworkProfile {
    pub fn same_host_baseline() -> Self {
        Self {
            kind: NetworkProfileKind::SameHostBaseline,
            shaping_method: "none".to_string(),
            shaping_config:
                "no injected latency; benchmark client connects directly to the target sequencer"
                    .to_string(),
        }
    }

    fn is_canonical_target_scenario(&self) -> bool {
        matches!(self.kind, NetworkProfileKind::CanonicalNetworkAware)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetVerdict {
    Pass,
    Fail,
    NotEvaluated,
}

impl TargetVerdict {
    fn as_line(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::NotEvaluated => "NOT_EVALUATED",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum P999Confidence {
    DiagnosticLowConfidence,
    Sufficient,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetEvaluation {
    pub label: String,
    pub verdict: TargetVerdict,
    pub threshold_p99_ms: f64,
    pub observed_p99_ms: f64,
    pub observed_p999_ms: f64,
    pub meets_latency_threshold: bool,
    pub zero_rejection: bool,
    pub sample_size_valid: bool,
    pub accepted_count: u64,
    pub rejected_count: u64,
    pub p999_confidence: P999Confidence,
    pub network_profile: NetworkProfile,
    pub reason: Option<String>,
}

pub fn evaluate_ack_target(
    report: &AckRunReport,
    network_profile: NetworkProfile,
) -> TargetEvaluation {
    build_target_evaluation(
        "ACK_TARGET",
        report.accepted,
        report.rejected,
        report.ack_latency_accepted.p99.as_secs_f64() * 1000.0,
        report.ack_latency_accepted.p999.as_secs_f64() * 1000.0,
        ACK_P99_TARGET_MS,
        network_profile,
    )
}

pub fn evaluate_soft_confirm_target(
    report: &E2eRunReport,
    network_profile: NetworkProfile,
) -> TargetEvaluation {
    build_target_evaluation(
        "SOFT_CONFIRM_TARGET",
        report.accepted,
        report.rejected,
        report.e2e_latency_accepted.p99.as_secs_f64() * 1000.0,
        report.e2e_latency_accepted.p999.as_secs_f64() * 1000.0,
        SOFT_CONFIRM_P99_TARGET_MS,
        network_profile,
    )
}

pub fn print_target_evaluation(evaluation: &TargetEvaluation) {
    if let Some(reason) = evaluation.reason.as_ref() {
        println!(
            "{}: {} ({reason})",
            evaluation.label,
            evaluation.verdict.as_line()
        );
    } else {
        println!("{}: {}", evaluation.label, evaluation.verdict.as_line());
    }
    println!(
        "  observed_p99_ms: {:.3} (threshold {:.3})",
        evaluation.observed_p99_ms, evaluation.threshold_p99_ms
    );
    println!("  observed_p99.9_ms: {:.3}", evaluation.observed_p999_ms);
    println!(
        "  latency_threshold_met: {}",
        evaluation.meets_latency_threshold
    );
    println!("  zero_rejection: {}", evaluation.zero_rejection);
    println!("  sample_size_valid: {}", evaluation.sample_size_valid);
    println!("  accepted_count: {}", evaluation.accepted_count);
    println!("  rejected_count: {}", evaluation.rejected_count);
    println!(
        "  p99.9_confidence: {}",
        match evaluation.p999_confidence {
            P999Confidence::DiagnosticLowConfidence => "diagnostic-low-confidence",
            P999Confidence::Sufficient => "sufficient",
        }
    );
    println!(
        "  network_profile: {:?} ({})",
        evaluation.network_profile.kind, evaluation.network_profile.shaping_method
    );
}

fn build_target_evaluation(
    label: &str,
    accepted_count: u64,
    rejected_count: u64,
    observed_p99_ms: f64,
    observed_p999_ms: f64,
    threshold_p99_ms: f64,
    network_profile: NetworkProfile,
) -> TargetEvaluation {
    let zero_rejection = rejected_count == 0;
    let sample_size_valid = accepted_count >= TARGET_EVALUATION_MIN_ACCEPTED_COUNT;
    let meets_latency_threshold = observed_p99_ms <= threshold_p99_ms;
    let p999_confidence = if accepted_count >= DIAGNOSTIC_P999_MIN_ACCEPTED_COUNT {
        P999Confidence::Sufficient
    } else {
        P999Confidence::DiagnosticLowConfidence
    };

    let (verdict, reason) = if !network_profile.is_canonical_target_scenario() {
        (
            TargetVerdict::NotEvaluated,
            Some(
                "canonical network-aware scenario is not configured in this harness yet"
                    .to_string(),
            ),
        )
    } else if !sample_size_valid {
        (
            TargetVerdict::NotEvaluated,
            Some(format!(
                "accepted_count={} is below the target-evaluation minimum of {}",
                accepted_count, TARGET_EVALUATION_MIN_ACCEPTED_COUNT
            )),
        )
    } else if !zero_rejection {
        (
            TargetVerdict::NotEvaluated,
            Some(format!(
                "rejected_count={} but target evaluation requires zero rejections",
                rejected_count
            )),
        )
    } else if meets_latency_threshold {
        (TargetVerdict::Pass, None)
    } else {
        (TargetVerdict::Fail, None)
    };

    TargetEvaluation {
        label: label.to_string(),
        verdict,
        threshold_p99_ms,
        observed_p99_ms,
        observed_p999_ms,
        meets_latency_threshold,
        zero_rejection,
        sample_size_valid,
        accepted_count,
        rejected_count,
        p999_confidence,
        network_profile,
        reason,
    }
}
