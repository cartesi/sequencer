// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use benchmarks::{
    BenchResult, BenchmarkJsonOutput, SweepRunReport, load_sweep_results,
    runtime::{DEFAULT_RESULTS_DIR, DRAIN_DEADLINE},
};
use clap::Parser;
use std::path::PathBuf;
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

    let ack_runs = load_sweep_results(dir, "ack-sweep")?;
    let rt_runs = load_sweep_results(dir, "rt-sweep")?;
    let baseline_ack = latest(&ack_runs, false);
    let capacity_sweep = latest(&ack_runs, true);
    let baseline_rt = latest(&rt_runs, false);
    let rt_sweep = latest(&rt_runs, true);

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
    let ack_c1_row = baseline_ack
        .and_then(|r| r.rows.iter().find(|row| row.concurrency == 1))
        .or_else(|| capacity_sweep.and_then(|r| r.rows.iter().find(|row| row.concurrency == 1)));
    match ack_c1_row {
        Some(row) => {
            println!(
                "  ACK latency:         p50={:.1}ms   p99={:.1}ms   ({} accepted)",
                row.ack_ms.p50_ms, row.ack_ms.p99_ms, row.accepted_count
            );
        }
        None => println!("  ACK latency:         n/a"),
    }
    let rt_row = baseline_rt
        .and_then(|r| r.rows.first())
        .and_then(|row| row.round_trip_ms.as_ref().map(|rt| (row, rt)));
    match rt_row {
        Some((row, rt)) => {
            println!(
                "  Round-trip latency:  p50={:.1}ms   p99={:.1}ms   ({} accepted)",
                rt.p50_ms, rt.p99_ms, row.accepted_count
            );
            println!(
                "    (ACK portion:      p50={:.1}ms   p99={:.1}ms)",
                row.ack_ms.p50_ms, row.ack_ms.p99_ms
            );
        }
        None => println!("  Round-trip latency:  n/a"),
    }
    println!();

    // --- Round-trip latency vs concurrency ---
    println!("  ROUND-TRIP LATENCY vs CONCURRENCY");
    println!("  ────────────────────────────────────────────");
    if let Some(r) = rt_sweep {
        for row in &r.rows {
            let Some(rt) = &row.round_trip_ms else {
                continue;
            };
            let rej = if row.rejected_count > 0 {
                format!("  rej={}", row.rejected_count)
            } else {
                String::new()
            };
            println!(
                "  c={:<3} {:>7} tx/s  ack p50={:.0}ms p99={:.0}ms  rt p50={:.0}ms p99={:.0}ms{}",
                row.concurrency,
                format_tps(row.accepted_tps),
                row.ack_ms.p50_ms,
                row.ack_ms.p99_ms,
                rt.p50_ms,
                rt.p99_ms,
                rej,
            );
        }
    } else {
        println!("  n/a (no round-trip sweep data)");
    }
    println!();

    // --- Sweep curve (throughput + latency) ---
    if let Some(r) = capacity_sweep {
        let rows = &r.rows;
        let max_tps = r.summary.max_sustainable_tps_at_0_rejections;

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
                row.ack_ms.p50_ms,
                row.ack_ms.p99_ms,
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

    // --- Memory & storage ---
    println!("  MEMORY & STORAGE");
    println!("  ────────────────────────────────────────────");
    let memory_source = capacity_sweep.and_then(|r| r.memory.as_ref());
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
    match capacity_sweep.and_then(|r| r.storage.as_ref()) {
        Some(storage) => println!(
            "  DB size:               {:.1} MB (+ WAL {:.1} MB)",
            mb(storage.db_bytes),
            mb(storage.wal_bytes)
        ),
        None => println!("  DB size:               n/a"),
    }
    match capacity_sweep.and_then(|r| r.drain.as_ref()) {
        Some(drain) => match drain.drain_secs {
            Some(secs) => println!(
                "  Batch drain:           {} batches in {:.1} s",
                drain.batches_pending, secs
            ),
            None => println!(
                "  Batch drain:           {} batches, not drained within {} s",
                drain.batches_pending,
                DRAIN_DEADLINE.as_secs()
            ),
        },
        None => println!("  Batch drain:           n/a"),
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

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
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

/// The latest run whose row count says it is a sweep (`multi_row`) or a
/// single-concurrency baseline.
fn latest(
    runs: &[(PathBuf, BenchmarkJsonOutput<SweepRunReport>)],
    multi_row: bool,
) -> Option<&SweepRunReport> {
    runs.iter()
        .rev()
        .map(|(_, run)| &run.report)
        .find(|report| (report.rows.len() > 1) == multi_row)
}
