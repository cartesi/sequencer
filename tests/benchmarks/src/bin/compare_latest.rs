// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use benchmarks::{
    BenchResult, StatsMs, SweepRow, SweepSummary, format_optional_f64, load_sweep_results,
    runtime::{DEFAULT_RESULTS_DIR, DrainReport, MemoryReport, StorageReport},
};
use clap::{Parser, ValueEnum};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CompareKind {
    Ack,
    RoundTrip,
    RtSweep,
    Sweep,
    All,
}

/// `(kind, label, artifact prefix, multi-row)`. Single-concurrency runs are
/// the baselines; multi-concurrency runs are the sweeps.
const KINDS: [(CompareKind, &str, &str, bool); 4] = [
    (CompareKind::Ack, "ACK", "ack-sweep", false),
    (CompareKind::RoundTrip, "ROUND-TRIP", "rt-sweep", false),
    (CompareKind::RtSweep, "RT SWEEP", "rt-sweep", true),
    (CompareKind::Sweep, "ACK SWEEP", "ack-sweep", true),
];

#[derive(Debug, Parser)]
#[command(name = "compare_latest")]
#[command(about = "Compare the latest two benchmark result files")]
struct Cli {
    #[arg(long, default_value = DEFAULT_RESULTS_DIR)]
    results_dir: PathBuf,
    #[arg(long, value_enum, default_value_t = CompareKind::All)]
    kind: CompareKind,
}

fn main() -> BenchResult<()> {
    let cli = Cli::parse();
    let selected = KINDS
        .iter()
        .filter(|(kind, ..)| cli.kind == CompareKind::All || cli.kind == *kind);
    for (index, (_, label, prefix, multi_row)) in selected.enumerate() {
        if index > 0 {
            println!();
        }
        compare(&cli.results_dir, label, prefix, *multi_row)?;
    }
    Ok(())
}

fn compare(results_dir: &Path, label: &str, prefix: &str, multi_row: bool) -> BenchResult<()> {
    let runs: Vec<_> = load_sweep_results(results_dir, prefix)?
        .into_iter()
        .filter(|(_, run)| (run.report.rows.len() > 1) == multi_row)
        .collect();
    let [.., (old_path, old), (new_path, new)] = runs.as_slice() else {
        let shape = if multi_row { "multi" } else { "single" };
        println!("{label}: n/a (need at least 2 {shape}-concurrency {prefix}-*.json files)");
        return Ok(());
    };
    let (old, new) = (&old.report, &new.report);

    println!(
        "{label} latest two:\n  old: {}\n  new: {}",
        old_path.display(),
        new_path.display()
    );
    let old_rows: BTreeMap<usize, &SweepRow> =
        old.rows.iter().map(|row| (row.concurrency, row)).collect();
    let new_rows: BTreeMap<usize, &SweepRow> =
        new.rows.iter().map(|row| (row.concurrency, row)).collect();
    let concurrencies: BTreeSet<usize> = old_rows.keys().chain(new_rows.keys()).copied().collect();
    for concurrency in concurrencies {
        match (old_rows.get(&concurrency), new_rows.get(&concurrency)) {
            (Some(old), Some(new)) => print_row_delta(old, new),
            _ => println!("  c={concurrency}: n/a (not in both runs)"),
        }
    }
    if multi_row {
        print_summary_delta(&old.summary, &new.summary);
    }
    print_memory_delta(old.memory.as_ref(), new.memory.as_ref());
    print_storage_delta(old.storage.as_ref(), new.storage.as_ref());
    print_drain_delta(old.drain.as_ref(), new.drain.as_ref());
    Ok(())
}

fn print_row_delta(old: &SweepRow, new: &SweepRow) {
    println!("  c={} delta (new - old):", new.concurrency);
    print_metric_delta("accepted_tps", old.accepted_tps, new.accepted_tps);
    print_stats_ms_delta("ack", &old.ack_ms, &new.ack_ms);
    if let (Some(old_rt), Some(new_rt)) = (&old.round_trip_ms, &new.round_trip_ms) {
        print_stats_ms_delta("round_trip", old_rt, new_rt);
    }
    print_count_delta("rejected", old.rejected_count, new.rejected_count);
    print_count_delta(
        "client_failure",
        old.client_failure_count,
        new.client_failure_count,
    );
    print_count_delta("http_429", old.http_429_count, new.http_429_count);
}

fn print_stats_ms_delta(name: &str, old: &StatsMs, new: &StatsMs) {
    print_metric_delta(&format!("{name} p50"), old.p50_ms, new.p50_ms);
    print_metric_delta(&format!("{name} p95"), old.p95_ms, new.p95_ms);
    print_metric_delta(&format!("{name} p99"), old.p99_ms, new.p99_ms);
    print_metric_delta(&format!("{name} p99.9"), old.p999_ms, new.p999_ms);
    print_metric_delta(&format!("{name} max"), old.max_ms, new.max_ms);
}

fn print_summary_delta(old: &SweepSummary, new: &SweepSummary) {
    println!("  summary:");
    for (label, old_value, new_value) in [
        (
            "max_sustainable_tps_at_0_rejections",
            old.max_sustainable_tps_at_0_rejections,
            new.max_sustainable_tps_at_0_rejections,
        ),
        (
            "tps_at_first_any_rejection",
            old.tps_at_first_any_rejection,
            new.tps_at_first_any_rejection,
        ),
        (
            "tps_at_first_non_200",
            old.tps_at_first_non_200,
            new.tps_at_first_non_200,
        ),
        (
            "tps_at_first_429",
            old.tps_at_first_429,
            new.tps_at_first_429,
        ),
        (
            "tps_at_first_client_failure",
            old.tps_at_first_client_failure,
            new.tps_at_first_client_failure,
        ),
    ] {
        println!(
            "    {label}: old={}, new={}",
            format_optional_f64(old_value),
            format_optional_f64(new_value)
        );
    }
}

fn print_memory_delta(old: Option<&MemoryReport>, new: Option<&MemoryReport>) {
    let (Some(old), Some(new)) = (old, new) else {
        return;
    };
    println!("  memory delta (new - old):");
    print_optional_metric_delta("rss_start_mb", old.rss_start_mb, new.rss_start_mb);
    print_optional_metric_delta("rss_peak_mb", old.rss_peak_mb, new.rss_peak_mb);
    print_optional_metric_delta("rss_growth_mb", old.rss_growth_mb, new.rss_growth_mb);
}

fn print_storage_delta(old: Option<&StorageReport>, new: Option<&StorageReport>) {
    let (Some(old), Some(new)) = (old, new) else {
        return;
    };
    let mb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    println!("  storage delta (new - old):");
    if let (Some(old_logical), Some(new_logical)) = (old.logical_db_bytes, new.logical_db_bytes) {
        print_metric_delta("logical_db_mb", mb(old_logical), mb(new_logical));
    }
    print_metric_delta("db_mb", mb(old.db_bytes), mb(new.db_bytes));
    print_metric_delta("wal_mb", mb(old.wal_bytes), mb(new.wal_bytes));
}

fn print_drain_delta(old: Option<&DrainReport>, new: Option<&DrainReport>) {
    let (Some(old), Some(new)) = (old, new) else {
        return;
    };
    println!("  drain delta (new - old):");
    print_count_delta("batches_pending", old.batches_pending, new.batches_pending);
    print_optional_metric_delta("drain_secs", old.drain_secs, new.drain_secs);
}

fn print_count_delta(label: &str, old_value: u64, new_value: u64) {
    println!(
        "    {label}: {old_value} -> {new_value} (delta {:+})",
        i128::from(new_value) - i128::from(old_value)
    );
}

fn print_metric_delta(label: &str, old_value: f64, new_value: f64) {
    println!(
        "    {label}: {:.3} -> {:.3} (delta {:+.3}, {:+.2}%)",
        old_value,
        new_value,
        new_value - old_value,
        pct(new_value, old_value)
    );
}

fn print_optional_metric_delta(label: &str, old_value: Option<f64>, new_value: Option<f64>) {
    let (Some(old_value), Some(new_value)) = (old_value, new_value) else {
        println!("    {label}: n/a");
        return;
    };
    print_metric_delta(label, old_value, new_value);
}

fn pct(new_value: f64, old_value: f64) -> f64 {
    if old_value == 0.0 {
        0.0
    } else {
        ((new_value / old_value) - 1.0) * 100.0
    }
}
