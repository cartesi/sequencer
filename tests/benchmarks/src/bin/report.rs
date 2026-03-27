// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use benchmarks::{
    BenchResult, BenchmarkJsonOutput, RoundTripRunReport, RtSweepRunReport, SweepRunReport,
    runtime::DEFAULT_RESULTS_DIR, trailing_number,
};
use clap::Parser;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Parser)]
#[command(name = "report")]
#[command(about = "Print a summary report from the latest benchmark results")]
struct Cli {
    #[arg(long, default_value = DEFAULT_RESULTS_DIR)]
    results_dir: PathBuf,
}

fn main() -> BenchResult<()> {
    let cli = Cli::parse();
    let dir = &cli.results_dir;

    let baseline_rt = load_latest::<RoundTripRunReport>(dir, "round-trip-latency-", ".json");
    let rt_sweep = load_latest::<RtSweepRunReport>(dir, "rt-sweep-", ".json");

    // Load sweep data: prefer capacity-sweep-self.json (full multi-level sweep),
    // then fall back to ack-sweep-*.json.
    let capacity_sweep = load_latest::<SweepRunReport>(dir, "capacity-sweep-self", ".json")
        .or_else(|| load_latest_multi_row_sweep(dir));

    // For baseline ACK latency, extract c=1 row from the latest ack-sweep file.
    // This is what bench-ack-self produces (sweep --concurrency-list 1).
    let baseline_ack_sweep = load_latest::<SweepRunReport>(dir, "ack-sweep-", ".json");

    println!();
    println!("══════════════════════════════════════════════════════════");
    println!("  SEQUENCER BENCHMARK REPORT");
    println!("══════════════════════════════════════════════════════════");
    print_environment();
    println!();

    // --- Baseline Latency ---
    println!("  BASELINE LATENCY (concurrency=1)");
    println!("  ────────────────────────────────────────────");
    // Try to get c=1 data from the baseline ack sweep, or from the capacity sweep.
    let ack_c1_row = baseline_ack_sweep
        .as_ref()
        .and_then(|r| r.report.rows.iter().find(|row| row.concurrency == 1))
        .or_else(|| {
            capacity_sweep
                .as_ref()
                .and_then(|r| r.report.rows.iter().find(|row| row.concurrency == 1))
        });
    match ack_c1_row {
        Some(row) => {
            println!(
                "  ACK latency:         p50={:.1}ms   p99={:.1}ms   ({} accepted)",
                row.p50_ms, row.p99_ms, row.accepted_count
            );
        }
        None => println!("  ACK latency:         n/a"),
    }
    match &baseline_rt {
        Some(r) => {
            let ack = &r.report.ack_latency_accepted_ms;
            let rt = &r.report.round_trip_latency_accepted_ms;
            println!(
                "  Round-trip latency:  p50={:.1}ms   p99={:.1}ms   ({} accepted)",
                rt.p50_ms, rt.p99_ms, r.report.accepted
            );
            println!(
                "    (ACK portion:      p50={:.1}ms   p99={:.1}ms)",
                ack.p50_ms, ack.p99_ms
            );
        }
        None => println!("  Round-trip latency:  n/a"),
    }
    println!();

    // --- Round-trip latency vs concurrency ---
    println!("  ROUND-TRIP LATENCY vs CONCURRENCY");
    println!("  ────────────────────────────────────────────");
    if let Some(r) = &rt_sweep {
        for row in &r.report.rows {
            let rej = if row.rejected_count > 0 {
                format!("  rej={}", row.rejected_count)
            } else {
                String::new()
            };
            println!(
                "  c={:<3} {:>7} tx/s  ack p50={:.0}ms p99={:.0}ms  rt p50={:.0}ms p99={:.0}ms{}",
                row.concurrency,
                format_tps(row.accepted_tps),
                row.ack_p50_ms,
                row.ack_p99_ms,
                row.rt_p50_ms,
                row.rt_p99_ms,
                rej,
            );
        }
    } else {
        println!("  n/a (no round-trip sweep data)");
    }
    println!();

    // --- Sweep curve (throughput + latency) ---
    if let Some(r) = &capacity_sweep {
        let rows = &r.report.rows;
        let max_tps = r.report.summary.max_sustainable_tps_at_0_rejections;

        println!("  THROUGHPUT & LATENCY vs CONCURRENCY");
        println!("  ────────────────────────────────────────────");
        println!(
            "  Peak: {} tx/s (0% rejection)",
            max_tps.map_or("n/a".to_string(), format_tps)
        );
        println!();

        let chart_max = rows
            .iter()
            .map(|row| row.accepted_tps)
            .fold(0.0_f64, f64::max);
        let bar_width = 30;
        for row in rows {
            let filled = ((row.accepted_tps / chart_max) * bar_width as f64).round() as usize;
            let bar = "█".repeat(filled) + &"░".repeat(bar_width - filled);
            let rej = if row.rejected_count > 0 {
                format!("  rej={}", row.rejected_count)
            } else {
                String::new()
            };
            println!(
                "  c={:<3} {} {:>7} tx/s  p50={:.0}ms p99={:.0}ms{}",
                row.concurrency,
                bar,
                format_tps(row.accepted_tps),
                row.p50_ms,
                row.p99_ms,
                rej,
            );
        }
        println!();
    } else {
        println!("  THROUGHPUT & LATENCY vs CONCURRENCY");
        println!("  ────────────────────────────────────────────");
        println!("  n/a (no sweep data)");
        println!();
    }

    // --- Memory ---
    println!("  MEMORY");
    println!("  ────────────────────────────────────────────");
    let memory_source = capacity_sweep
        .as_ref()
        .and_then(|r| r.report.memory.as_ref());
    match memory_source {
        Some(mem) => {
            match mem.rss_growth_per_1k_accepted_tx_mb {
                Some(v) => println!("  RSS growth per 1K txs: {:.2} MB", v),
                None => println!("  RSS growth per 1K txs: n/a (insufficient sample)"),
            }
            match mem.rss_peak_mb {
                Some(v) => println!("  RSS peak:              {:.1} MB", v),
                None => println!("  RSS peak:              n/a"),
            }
        }
        None => {
            println!("  n/a (no memory data available)");
        }
    }

    println!();
    println!("  Methodology: all benchmarks run on localhost (same-host).");
    println!("  Client uses HTTP/1.1 with keep-alive (reqwest connection");
    println!("  pooling). Workload: signed EIP-712 transfer operations,");
    println!("  nonce-validated, fee-checked, state-mutating.");
    println!();
    println!("══════════════════════════════════════════════════════════");
    println!();

    Ok(())
}

fn print_environment() {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let cpu = if os == "macos" {
        cmd_output("sysctl", &["-n", "machdep.cpu.brand_string"])
    } else {
        // Linux: extract model name from /proc/cpuinfo
        cmd_output(
            "sh",
            &["-c", "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2"],
        )
    }
    .map(|s| s.trim().to_string());

    let cores = if os == "macos" {
        cmd_output("sysctl", &["-n", "hw.ncpu"])
    } else {
        cmd_output("nproc", &[])
    }
    .map(|s| s.trim().to_string());

    let mem_gb = if os == "macos" {
        cmd_output("sysctl", &["-n", "hw.memsize"])
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|b| format!("{} GB", b / (1024 * 1024 * 1024)))
    } else {
        cmd_output(
            "sh",
            &["-c", "grep MemTotal /proc/meminfo | awk '{print $2}'"],
        )
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| format!("{} GB", kb / (1024 * 1024)))
    };

    println!();
    print!("  env: {}/{}", os, arch);
    if let Some(c) = &cores {
        print!(", {} cores", c);
    }
    if let Some(m) = &mem_gb {
        print!(", {}", m);
    }
    println!();
    if let Some(cpu) = &cpu {
        println!("       {}", cpu);
    }
}

fn cmd_output(cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
}

fn format_tps(tps: f64) -> String {
    if tps >= 1000.0 {
        let thousands = tps / 1000.0;
        if thousands >= 10.0 {
            format!("{:.0}K", thousands)
        } else {
            format!("{:.1}K", thousands)
        }
    } else {
        format!("{:.0}", tps)
    }
}

fn load_latest<R: serde::de::DeserializeOwned>(
    dir: &Path,
    prefix: &str,
    suffix: &str,
) -> Option<BenchmarkJsonOutput<R, Value>> {
    let path = latest_file(dir, prefix, suffix)?;
    let raw = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Load the latest ack-sweep JSON that has more than 1 row (i.e., a real sweep,
/// not a single-concurrency baseline run).
fn load_latest_multi_row_sweep(dir: &Path) -> Option<BenchmarkJsonOutput<SweepRunReport, Value>> {
    let mut candidates: Vec<(u64, PathBuf)> = Vec::new();
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = path.file_name()?.to_str()?;
        if file_name.starts_with("ack-sweep-") && file_name.ends_with(".json") {
            let ts = trailing_number(file_name).unwrap_or(0);
            candidates.push((ts, path));
        }
    }
    // Sort by timestamp descending.
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    // Return the first one that has more than 1 row.
    for (_, path) in candidates {
        let raw = fs::read_to_string(&path).ok()?;
        if let Ok(parsed) = serde_json::from_str::<BenchmarkJsonOutput<SweepRunReport, Value>>(&raw)
            && parsed.report.rows.len() > 1
        {
            return Some(parsed);
        }
    }
    None
}

fn latest_file(dir: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;

    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = path.file_name()?.to_str()?;
        if file_name.starts_with(prefix) && file_name.ends_with(suffix) {
            let ts = trailing_number(file_name).unwrap_or(0);
            if best.as_ref().is_none_or(|(best_ts, _)| ts > *best_ts) {
                best = Some((ts, path));
            }
        }
    }

    best.map(|(_, path)| path)
}
