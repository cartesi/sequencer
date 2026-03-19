// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::{BenchResult, support::err};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub count: usize,
    pub min: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub p999: Duration,
}

pub fn summarize(samples: &[Duration]) -> BenchResult<Stats> {
    if samples.is_empty() {
        return Err(err("cannot summarize empty sample set"));
    }

    let mut nanos: Vec<u128> = samples.iter().map(Duration::as_nanos).collect();
    nanos.sort_unstable();
    let sum: u128 = nanos.iter().copied().sum();
    let count = nanos.len();

    Ok(Stats {
        count,
        min: duration_from_nanos(nanos[0]),
        max: duration_from_nanos(nanos[count - 1]),
        mean: duration_from_nanos(sum / count as u128),
        p50: duration_from_nanos(percentile(&nanos, 0.50)),
        p95: duration_from_nanos(percentile(&nanos, 0.95)),
        p99: duration_from_nanos(percentile(&nanos, 0.99)),
        p999: duration_from_nanos(percentile(&nanos, 0.999)),
    })
}

pub fn print_stats(name: &str, stats: &Stats) {
    println!("{name}:");
    println!("  count: {}", stats.count);
    println!("  min:   {}", format_ms(stats.min));
    println!("  p50:   {}", format_ms(stats.p50));
    println!("  p95:   {}", format_ms(stats.p95));
    println!("  p99:   {}", format_ms(stats.p99));
    println!("  p99.9: {}", format_ms(stats.p999));
    println!("  max:   {}", format_ms(stats.max));
    println!("  mean:  {}", format_ms(stats.mean));
}

pub fn throughput_tx_per_s(accepted_count: usize, total_wall: Duration) -> f64 {
    if total_wall.is_zero() {
        0.0
    } else {
        accepted_count as f64 / total_wall.as_secs_f64()
    }
}

pub fn rejection_rate(accepted: u64, rejected: u64) -> f64 {
    let total = accepted.saturating_add(rejected);
    if total == 0 {
        0.0
    } else {
        (rejected as f64 / total as f64) * 100.0
    }
}

pub(crate) fn format_optional_f64(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.3}"),
        None => "n/a".to_string(),
    }
}

fn percentile(sorted_nanos: &[u128], p: f64) -> u128 {
    let last = sorted_nanos.len() - 1;
    let rank = (p * last as f64).ceil() as usize;
    sorted_nanos[rank.min(last)]
}

fn duration_from_nanos(value: u128) -> Duration {
    let nanos = u64::try_from(value).unwrap_or(u64::MAX);
    Duration::from_nanos(nanos)
}

fn format_ms(value: Duration) -> String {
    format!("{:.3} ms", value.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::summarize;
    use std::time::Duration;

    #[test]
    fn summarize_includes_p999() {
        let samples: Vec<Duration> = (1_u64..=10_000).map(Duration::from_micros).collect();
        let stats = summarize(samples.as_slice()).expect("summarize");
        assert_eq!(stats.count, 10_000);
        assert!(stats.p999 >= stats.p99);
        assert!(stats.p999 <= stats.max);
    }
}
