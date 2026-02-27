// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use benchmarks::BenchResult;
use clap::{Parser, ValueEnum};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompareKind {
    Ack,
    E2e,
    Sweep,
    All,
}

#[derive(Debug, Parser)]
#[command(name = "compare_latest")]
#[command(about = "Compare the latest two benchmark result files")]
struct Cli {
    #[arg(long, default_value = "benchmarks/results")]
    results_dir: PathBuf,
    #[arg(long, value_enum, default_value_t = CompareKind::All)]
    kind: CompareKind,
}

#[derive(Debug, Deserialize)]
struct DurationJson {
    secs: u64,
    nanos: u32,
}

impl DurationJson {
    fn as_secs_f64(&self) -> f64 {
        self.secs as f64 + f64::from(self.nanos) / 1_000_000_000.0
    }

    fn as_ms_f64(&self) -> f64 {
        self.as_secs_f64() * 1_000.0
    }
}

#[derive(Debug, Deserialize)]
struct StatsJson {
    p50: DurationJson,
    p95: DurationJson,
    p99: DurationJson,
    p999: DurationJson,
    max: DurationJson,
}

#[derive(Debug, Deserialize)]
struct MemoryJson {
    rss_start_mb: f64,
    rss_peak_mb: f64,
    rss_growth_mb: f64,
}

#[derive(Debug, Deserialize)]
struct AckFileJson {
    report: AckReportJson,
}

#[derive(Debug, Deserialize)]
struct AckReportJson {
    accepted: u64,
    rejected: u64,
    total_wall: DurationJson,
    ack_latency_accepted: StatsJson,
    memory: Option<MemoryJson>,
}

#[derive(Debug, Deserialize)]
struct E2eFileJson {
    report: E2eReportJson,
}

#[derive(Debug, Deserialize)]
struct E2eReportJson {
    accepted: u64,
    rejected: u64,
    total_wall: DurationJson,
    ack_latency_accepted: StatsJson,
    e2e_latency_accepted: StatsJson,
    memory: Option<MemoryJson>,
}

#[derive(Debug, Clone)]
struct SweepCsvRow {
    concurrency: u64,
    accepted_tps: f64,
    rejected_count: u64,
    p95_ms: f64,
    p99_ms: f64,
    p999_ms: f64,
}

fn main() -> BenchResult<()> {
    let cli = Cli::parse();
    match cli.kind {
        CompareKind::Ack => compare_ack(&cli.results_dir)?,
        CompareKind::E2e => compare_e2e(&cli.results_dir)?,
        CompareKind::Sweep => compare_sweep(&cli.results_dir)?,
        CompareKind::All => {
            compare_ack(&cli.results_dir)?;
            println!();
            compare_e2e(&cli.results_dir)?;
            println!();
            compare_sweep(&cli.results_dir)?;
        }
    }
    Ok(())
}

fn compare_ack(results_dir: &Path) -> BenchResult<()> {
    let (old_path, new_path) = latest_two_files(results_dir, "ack-latency-", ".json")?;
    let old = read_json::<AckFileJson>(&old_path)?;
    let new = read_json::<AckFileJson>(&new_path)?;

    println!(
        "ACK latest two:\n  old: {}\n  new: {}",
        old_path.display(),
        new_path.display()
    );
    print_common(
        old.report.accepted,
        old.report.rejected,
        &old.report.total_wall,
    );
    print_common_delta(
        old.report.accepted,
        old.report.rejected,
        &old.report.total_wall,
        new.report.accepted,
        new.report.rejected,
        &new.report.total_wall,
    );
    print_stats_delta(
        "ack latency",
        &old.report.ack_latency_accepted,
        &new.report.ack_latency_accepted,
    );
    print_memory_delta(old.report.memory.as_ref(), new.report.memory.as_ref());
    Ok(())
}

fn compare_e2e(results_dir: &Path) -> BenchResult<()> {
    let (old_path, new_path) = latest_two_files(results_dir, "e2e-latency-", ".json")?;
    let old = read_json::<E2eFileJson>(&old_path)?;
    let new = read_json::<E2eFileJson>(&new_path)?;

    println!(
        "E2E latest two:\n  old: {}\n  new: {}",
        old_path.display(),
        new_path.display()
    );
    print_common(
        old.report.accepted,
        old.report.rejected,
        &old.report.total_wall,
    );
    print_common_delta(
        old.report.accepted,
        old.report.rejected,
        &old.report.total_wall,
        new.report.accepted,
        new.report.rejected,
        &new.report.total_wall,
    );
    print_stats_delta(
        "ack latency",
        &old.report.ack_latency_accepted,
        &new.report.ack_latency_accepted,
    );
    print_stats_delta(
        "e2e latency",
        &old.report.e2e_latency_accepted,
        &new.report.e2e_latency_accepted,
    );
    print_memory_delta(old.report.memory.as_ref(), new.report.memory.as_ref());
    Ok(())
}

fn compare_sweep(results_dir: &Path) -> BenchResult<()> {
    let (old_path, new_path) = latest_two_files(results_dir, "e2e-sweep-", ".csv")?;
    let old_rows = read_sweep_rows(&old_path)?;
    let new_rows = read_sweep_rows(&new_path)?;

    println!(
        "SWEEP latest two:\n  old: {}\n  new: {}",
        old_path.display(),
        new_path.display()
    );

    let old_by_concurrency = map_sweep_rows(&old_rows);
    let new_by_concurrency = map_sweep_rows(&new_rows);

    println!("  deltas by concurrency:");
    println!("  c,accepted_tps_delta,p95_ms_delta,p99_ms_delta,p999_ms_delta,rejected_delta");

    let mut all_concurrency: Vec<u64> = old_by_concurrency
        .keys()
        .chain(new_by_concurrency.keys())
        .copied()
        .collect();
    all_concurrency.sort_unstable();
    all_concurrency.dedup();

    for concurrency in all_concurrency {
        let old = old_by_concurrency.get(&concurrency);
        let new = new_by_concurrency.get(&concurrency);
        if let (Some(old), Some(new)) = (old, new) {
            println!(
                "  {},{:+.3},{:+.3},{:+.3},{:+.3},{:+}",
                concurrency,
                new.accepted_tps - old.accepted_tps,
                new.p95_ms - old.p95_ms,
                new.p99_ms - old.p99_ms,
                new.p999_ms - old.p999_ms,
                i128::from(new.rejected_count) - i128::from(old.rejected_count),
            );
        } else {
            println!("  {concurrency},n/a,n/a,n/a,n/a,n/a");
        }
    }

    let old_max_zero_rejection_tps = max_zero_rejection_tps(&old_rows);
    let new_max_zero_rejection_tps = max_zero_rejection_tps(&new_rows);
    let old_first_rejection_tps = first_rejection_tps(&old_rows);
    let new_first_rejection_tps = first_rejection_tps(&new_rows);

    println!("  summary:");
    println!(
        "    max_sustainable_tps_at_0_rejections: old={:.3}, new={:.3}, delta={:+.3}",
        old_max_zero_rejection_tps,
        new_max_zero_rejection_tps,
        new_max_zero_rejection_tps - old_max_zero_rejection_tps
    );
    println!(
        "    tps_at_first_non_200: old={}, new={}",
        fmt_opt(old_first_rejection_tps),
        fmt_opt(new_first_rejection_tps),
    );
    Ok(())
}

fn latest_two_files(
    results_dir: &Path,
    prefix: &str,
    suffix: &str,
) -> BenchResult<(PathBuf, PathBuf)> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(results_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if file_name.starts_with(prefix) && file_name.ends_with(suffix) {
            let timestamp = trailing_number(file_name).unwrap_or(0);
            candidates.push((timestamp, file_name.to_string(), path));
        }
    }

    candidates.sort_by(|a, b| a.cmp(b));
    if candidates.len() < 2 {
        return Err(std::io::Error::other(format!(
            "need at least 2 files for pattern {prefix}*{suffix} in {}",
            results_dir.display()
        ))
        .into());
    }
    let old = candidates[candidates.len() - 2].2.clone();
    let new = candidates[candidates.len() - 1].2.clone();
    Ok((old, new))
}

fn trailing_number(file_name: &str) -> Option<u64> {
    let stem = file_name
        .rsplit_once('.')
        .map(|(left, _)| left)
        .unwrap_or(file_name);
    let reversed_digits: String = stem
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if reversed_digits.is_empty() {
        return None;
    }
    let digits: String = reversed_digits.chars().rev().collect();
    digits.parse::<u64>().ok()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> BenchResult<T> {
    let raw = fs::read_to_string(path)?;
    let parsed = serde_json::from_str::<T>(&raw)?;
    Ok(parsed)
}

fn read_sweep_rows(path: &Path) -> BenchResult<Vec<SweepCsvRow>> {
    let content = fs::read_to_string(path)?;
    let mut lines = content.lines();
    let Some(header_line) = lines.next() else {
        return Err(std::io::Error::other(format!("empty CSV file: {}", path.display())).into());
    };
    let headers: Vec<&str> = header_line.split(',').collect();
    let mut header_idx = BTreeMap::new();
    for (idx, header) in headers.iter().enumerate() {
        header_idx.insert(*header, idx);
    }

    let mut rows = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        let concurrency = parse_csv_u64(&cols, &header_idx, "concurrency")?;
        let accepted_tps = parse_csv_f64(&cols, &header_idx, "accepted_tps")?;
        let rejected_count = parse_csv_u64(&cols, &header_idx, "rejected_count")?;
        let p95_ms = parse_csv_f64(&cols, &header_idx, "p95_ms")?;
        let p99_ms = parse_csv_f64(&cols, &header_idx, "p99_ms")?;
        let p999_ms = parse_csv_f64(&cols, &header_idx, "p999_ms")?;
        rows.push(SweepCsvRow {
            concurrency,
            accepted_tps,
            rejected_count,
            p95_ms,
            p99_ms,
            p999_ms,
        });
    }
    Ok(rows)
}

fn parse_csv_u64(cols: &[&str], headers: &BTreeMap<&str, usize>, key: &str) -> BenchResult<u64> {
    let idx = *headers
        .get(key)
        .ok_or_else(|| std::io::Error::other(format!("missing CSV column: {key}")))?;
    let value = cols
        .get(idx)
        .ok_or_else(|| std::io::Error::other(format!("missing CSV value at column: {key}")))?;
    let parsed = value
        .parse::<u64>()
        .map_err(|e| std::io::Error::other(format!("invalid u64 for {key}: {value} ({e})")))?;
    Ok(parsed)
}

fn parse_csv_f64(cols: &[&str], headers: &BTreeMap<&str, usize>, key: &str) -> BenchResult<f64> {
    let idx = *headers
        .get(key)
        .ok_or_else(|| std::io::Error::other(format!("missing CSV column: {key}")))?;
    let value = cols
        .get(idx)
        .ok_or_else(|| std::io::Error::other(format!("missing CSV value at column: {key}")))?;
    let parsed = value
        .parse::<f64>()
        .map_err(|e| std::io::Error::other(format!("invalid f64 for {key}: {value} ({e})")))?;
    Ok(parsed)
}

fn map_sweep_rows(rows: &[SweepCsvRow]) -> BTreeMap<u64, SweepCsvRow> {
    let mut out = BTreeMap::new();
    for row in rows {
        out.insert(row.concurrency, row.clone());
    }
    out
}

fn max_zero_rejection_tps(rows: &[SweepCsvRow]) -> f64 {
    rows.iter()
        .filter(|row| row.rejected_count == 0)
        .map(|row| row.accepted_tps)
        .fold(0.0, f64::max)
}

fn first_rejection_tps(rows: &[SweepCsvRow]) -> Option<f64> {
    rows.iter()
        .find(|row| row.rejected_count > 0)
        .map(|row| row.accepted_tps)
}

fn print_common(accepted: u64, rejected: u64, total_wall: &DurationJson) {
    let throughput = throughput(accepted, total_wall);
    println!("  old summary:");
    println!("    accepted: {accepted}");
    println!("    rejected: {rejected}");
    println!("    completed_per_s: {:.2} tx/s", throughput);
}

fn print_common_delta(
    old_accepted: u64,
    old_rejected: u64,
    old_total_wall: &DurationJson,
    new_accepted: u64,
    new_rejected: u64,
    new_total_wall: &DurationJson,
) {
    let old_tps = throughput(old_accepted, old_total_wall);
    let new_tps = throughput(new_accepted, new_total_wall);
    println!("  new summary:");
    println!(
        "    accepted: {new_accepted} (delta {:+})",
        i128::from(new_accepted) - i128::from(old_accepted)
    );
    println!(
        "    rejected: {new_rejected} (delta {:+})",
        i128::from(new_rejected) - i128::from(old_rejected)
    );
    println!(
        "    completed_per_s: {:.2} tx/s (delta {:+.2}, {:+.2}%)",
        new_tps,
        new_tps - old_tps,
        pct(new_tps, old_tps)
    );
}

fn print_stats_delta(name: &str, old: &StatsJson, new: &StatsJson) {
    println!("  {name} delta (new - old):");
    print_metric_delta("p50", old.p50.as_ms_f64(), new.p50.as_ms_f64());
    print_metric_delta("p95", old.p95.as_ms_f64(), new.p95.as_ms_f64());
    print_metric_delta("p99", old.p99.as_ms_f64(), new.p99.as_ms_f64());
    print_metric_delta("p99.9", old.p999.as_ms_f64(), new.p999.as_ms_f64());
    print_metric_delta("max", old.max.as_ms_f64(), new.max.as_ms_f64());
}

fn print_memory_delta(old: Option<&MemoryJson>, new: Option<&MemoryJson>) {
    let (Some(old), Some(new)) = (old, new) else {
        return;
    };
    println!("  memory delta (new - old):");
    print_metric_delta("rss_start_mb", old.rss_start_mb, new.rss_start_mb);
    print_metric_delta("rss_peak_mb", old.rss_peak_mb, new.rss_peak_mb);
    print_metric_delta("rss_growth_mb", old.rss_growth_mb, new.rss_growth_mb);
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

fn throughput(accepted: u64, total_wall: &DurationJson) -> f64 {
    let secs = total_wall.as_secs_f64();
    if secs == 0.0 {
        return 0.0;
    }
    accepted as f64 / secs
}

fn pct(new_value: f64, old_value: f64) -> f64 {
    if old_value == 0.0 {
        0.0
    } else {
        ((new_value / old_value) - 1.0) * 100.0
    }
}

fn fmt_opt(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.3}"),
        None => "n/a".to_string(),
    }
}
