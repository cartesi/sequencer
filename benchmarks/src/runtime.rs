// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::BenchResult;

pub const DEFAULT_SEQUENCER_BIN: &str = "target/release/sequencer";
pub const DEFAULT_SEQUENCER_START_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_SEQUENCER_SHUTDOWN_TIMEOUT_MS: u64 = 3_000;
pub const DEFAULT_MEMORY_SAMPLE_INTERVAL_MS: u64 = 500;
pub const DEFAULT_RUNTIME_METRICS_LOG_INTERVAL_MS: u64 = 5_000;
pub const DEFAULT_SEQUENCER_LOGS_DIR: &str = "benchmarks/results";

#[derive(Debug, Clone)]
pub struct ManagedSequencerConfig {
    pub sequencer_bin: String,
    pub start_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub temp_db: bool,
    pub log_path: Option<PathBuf>,
    pub runtime_metrics_enabled: bool,
    pub runtime_metrics_log_interval: Duration,
    pub rust_log: String,
}

pub struct ManagedSequencer {
    child: Child,
    shutdown_timeout: Duration,
    temp_dir: Option<TempDir>,
    pub endpoint: String,
    pub ws_subscribe_url: String,
    pub db_path: PathBuf,
    log_path: PathBuf,
}

impl ManagedSequencer {
    pub async fn spawn(config: ManagedSequencerConfig) -> BenchResult<Self> {
        let (endpoint, http_addr) = build_local_endpoint()?;
        let ws_subscribe_url = format!("{}/ws/subscribe", endpoint.replacen("http://", "ws://", 1));

        let (temp_dir, db_path) = if config.temp_db {
            let dir = tempfile::tempdir()?;
            let path = dir_path_join(dir.path(), "sequencer.db");
            (Some(dir), path)
        } else {
            (None, PathBuf::from("sequencer.bench.db"))
        };

        let log_path = config
            .log_path
            .unwrap_or_else(|| default_sequencer_log_path("sequencer-self-contained"));
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let stdout_log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(log_path.as_path())?;
        let stderr_log = stdout_log.try_clone()?;

        let mut child = Command::new(config.sequencer_bin.as_str())
            .arg("--http-addr")
            .arg(http_addr)
            .arg("--db-path")
            .arg(path_as_str(db_path.as_path())?)
            .env(
                "SEQ_RUNTIME_METRICS_ENABLED",
                if config.runtime_metrics_enabled {
                    "true"
                } else {
                    "false"
                },
            )
            .env(
                "SEQ_RUNTIME_METRICS_LOG_INTERVAL_MS",
                config.runtime_metrics_log_interval.as_millis().to_string(),
            )
            .env("RUST_LOG", config.rust_log.as_str())
            .stdout(Stdio::from(stdout_log))
            .stderr(Stdio::from(stderr_log))
            .spawn()
            .map_err(|err| {
                io_other(format!(
                    "failed to spawn sequencer binary '{}': {err}",
                    config.sequencer_bin
                ))
            })?;

        wait_for_readiness(endpoint.as_str(), &mut child, config.start_timeout).await?;

        Ok(Self {
            child,
            shutdown_timeout: config.shutdown_timeout,
            temp_dir,
            endpoint,
            ws_subscribe_url,
            db_path,
            log_path,
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn log_path(&self) -> &Path {
        self.log_path.as_path()
    }

    pub async fn shutdown(mut self) -> BenchResult<()> {
        let _ = self.temp_dir.take();
        send_graceful_terminate(&mut self.child).await;
        match tokio::time::timeout(self.shutdown_timeout, self.child.wait()).await {
            Ok(wait_result) => {
                let _ = wait_result?;
                Ok(())
            }
            Err(_) => {
                self.child.start_kill()?;
                let _ = self.child.wait().await;
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MemoryReport {
    pub method: String,
    pub sample_interval_ms: u64,
    pub rss_start_mb: Option<f64>,
    pub rss_peak_mb: Option<f64>,
    pub rss_end_mb: Option<f64>,
    pub rss_growth_mb: Option<f64>,
    pub rss_growth_per_1k_accepted_tx_mb: Option<f64>,
}

pub struct MemorySampler {
    stop_tx: Option<oneshot::Sender<u64>>,
    join: JoinHandle<MemoryReport>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct InclusionLaneProfileReport {
    pub samples: u64,
    pub latest_window_ms: Option<u64>,
    pub latest_user_op_app_execute_phase_ms: Option<u64>,
    pub latest_user_op_persist_phase_ms: Option<u64>,
    pub latest_user_op_app_share_pct_of_app_plus_persist: Option<f64>,
    pub latest_user_op_persist_share_pct_of_app_plus_persist: Option<f64>,
    pub avg_user_op_app_share_pct_of_app_plus_persist: Option<f64>,
    pub avg_user_op_persist_share_pct_of_app_plus_persist: Option<f64>,
}

impl MemorySampler {
    pub fn start(pid: u32, sample_interval: Duration) -> Self {
        let (stop_tx, stop_rx) = oneshot::channel::<u64>();
        let join =
            tokio::spawn(
                async move { sample_memory_until_stop(pid, sample_interval, stop_rx).await },
            );
        Self {
            stop_tx: Some(stop_tx),
            join,
        }
    }

    pub async fn stop(mut self, accepted_count: u64) -> BenchResult<MemoryReport> {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(accepted_count);
        }
        let report = self
            .join
            .await
            .map_err(|err| io_other(format!("memory sampler task join failed: {err}")))?;
        Ok(report)
    }
}

async fn sample_memory_until_stop(
    pid: u32,
    sample_interval: Duration,
    mut stop_rx: oneshot::Receiver<u64>,
) -> MemoryReport {
    let mut start_mb = sample_rss_mb(pid);
    let mut end_mb = start_mb;
    let mut peak_mb = start_mb;
    let mut accepted_count = 0_u64;

    loop {
        tokio::select! {
            maybe_accepted = &mut stop_rx => {
                if let Ok(value) = maybe_accepted {
                    accepted_count = value;
                }
                end_mb = sample_rss_mb(pid).or(end_mb);
                if let Some(end) = end_mb {
                    peak_mb = Some(peak_mb.unwrap_or(end).max(end));
                }
                break;
            }
            _ = tokio::time::sleep(sample_interval) => {
                if let Some(current) = sample_rss_mb(pid) {
                    if start_mb.is_none() {
                        start_mb = Some(current);
                    }
                    end_mb = Some(current);
                    peak_mb = Some(peak_mb.unwrap_or(current).max(current));
                }
            }
        }
    }

    let rss_growth_mb = match (start_mb, end_mb) {
        (Some(start), Some(end)) => Some(end - start),
        _ => None,
    };
    let rss_growth_per_1k_accepted_tx_mb = match (rss_growth_mb, accepted_count) {
        (Some(growth), value) if value > 0 => Some(growth / (value as f64 / 1000.0)),
        _ => None,
    };

    MemoryReport {
        method: "ps-rss-sampling".to_string(),
        sample_interval_ms: u64::try_from(sample_interval.as_millis()).unwrap_or(u64::MAX),
        rss_start_mb: start_mb,
        rss_peak_mb: peak_mb,
        rss_end_mb: end_mb,
        rss_growth_mb,
        rss_growth_per_1k_accepted_tx_mb,
    }
}

fn sample_rss_mb(pid: u32) -> Option<f64> {
    let output = std::process::Command::new("ps")
        .arg("-o")
        .arg("rss=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let rss_kib = text.trim().parse::<f64>().ok()?;
    Some(rss_kib / 1024.0)
}

pub fn parse_inclusion_lane_profile_from_log(
    log_path: &Path,
) -> BenchResult<Option<InclusionLaneProfileReport>> {
    if !log_path.exists() {
        return Ok(None);
    }
    let file = std::fs::File::open(log_path)?;
    let reader = BufReader::new(file);

    let mut report = InclusionLaneProfileReport::default();
    let mut app_share_sum = 0.0_f64;
    let mut persist_share_sum = 0.0_f64;
    let mut app_share_samples = 0_u64;
    let mut persist_share_samples = 0_u64;

    for line_result in reader.lines() {
        let line = line_result?;
        let line = strip_ansi_escapes(line.as_str());
        if !line.contains("inclusion lane metrics") {
            continue;
        }

        report.samples = report.samples.saturating_add(1);
        report.latest_window_ms = parse_u64_field(line.as_str(), "window_ms");
        report.latest_user_op_app_execute_phase_ms =
            parse_u64_field(line.as_str(), "user_op_app_execute_phase_ms");
        report.latest_user_op_persist_phase_ms =
            parse_u64_field(line.as_str(), "user_op_persist_phase_ms");
        report.latest_user_op_app_share_pct_of_app_plus_persist =
            parse_f64_field(line.as_str(), "user_op_app_share_pct_of_app_plus_persist");
        report.latest_user_op_persist_share_pct_of_app_plus_persist = parse_f64_field(
            line.as_str(),
            "user_op_persist_share_pct_of_app_plus_persist",
        );

        if let Some(value) = report.latest_user_op_app_share_pct_of_app_plus_persist {
            app_share_sum += value;
            app_share_samples = app_share_samples.saturating_add(1);
        }
        if let Some(value) = report.latest_user_op_persist_share_pct_of_app_plus_persist {
            persist_share_sum += value;
            persist_share_samples = persist_share_samples.saturating_add(1);
        }
    }

    if report.samples == 0 {
        return Ok(None);
    }
    if app_share_samples > 0 {
        report.avg_user_op_app_share_pct_of_app_plus_persist =
            Some(app_share_sum / app_share_samples as f64);
    }
    if persist_share_samples > 0 {
        report.avg_user_op_persist_share_pct_of_app_plus_persist =
            Some(persist_share_sum / persist_share_samples as f64);
    }

    Ok(Some(report))
}

pub fn default_sequencer_log_path(prefix: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    PathBuf::from(format!("{DEFAULT_SEQUENCER_LOGS_DIR}/{prefix}-{ts}.log"))
}

fn parse_u64_field(line: &str, key: &str) -> Option<u64> {
    let needle = format!("{key}=");
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(needle.as_str()))
        .and_then(clean_token_value)
        .and_then(|value| value.parse::<u64>().ok())
}

fn parse_f64_field(line: &str, key: &str) -> Option<f64> {
    let needle = format!("{key}=");
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(needle.as_str()))
        .and_then(clean_token_value)
        .and_then(|value| value.parse::<f64>().ok())
}

fn clean_token_value(raw: &str) -> Option<String> {
    let value = raw
        .trim()
        .trim_matches(',')
        .trim_matches('"')
        .trim_matches('\'');
    (!value.is_empty()).then(|| value.to_string())
}

fn strip_ansi_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0_usize;

    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() {
                    let b = bytes[i];
                    i += 1;
                    if b.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            continue;
        }

        out.push(bytes[i] as char);
        i += 1;
    }

    out
}

async fn wait_for_readiness(
    endpoint: &str,
    child: &mut Child,
    timeout: Duration,
) -> BenchResult<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(io_other(format!(
                "sequencer exited before readiness: status={status}"
            ))
            .into());
        }
        if http_endpoint_is_ready(endpoint).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io_other(format!(
                "timed out waiting for sequencer readiness at {endpoint}"
            ))
            .into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn http_endpoint_is_ready(endpoint: &str) -> bool {
    let Some(host_port) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let mut stream =
        match tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(host_port)).await
        {
            Ok(Ok(value)) => value,
            _ => return false,
        };

    let request = format!("GET /tx HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = [0_u8; 64];
    match tokio::time::timeout(Duration::from_millis(300), stream.read(&mut head)).await {
        Ok(Ok(read)) if read > 0 => std::str::from_utf8(&head[..read])
            .ok()
            .map(|text| text.starts_with("HTTP/1.1") || text.starts_with("HTTP/1.0"))
            .unwrap_or(false),
        _ => false,
    }
}

async fn send_graceful_terminate(child: &mut Child) {
    let Some(pid) = child.id() else {
        return;
    };

    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }

    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
}

fn build_local_endpoint() -> BenchResult<(String, String)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let http_addr = format!("127.0.0.1:{}", addr.port());
    let endpoint = format!("http://{http_addr}");
    Ok((endpoint, http_addr))
}

fn dir_path_join(base: &Path, file: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    path.push(file);
    path
}

fn path_as_str(path: &Path) -> BenchResult<&str> {
    path.to_str()
        .ok_or_else(|| io_other(format!("path is not valid UTF-8: {}", path.display())).into())
}

fn io_other(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

#[cfg(test)]
mod tests {
    use super::{default_sequencer_log_path, parse_inclusion_lane_profile_from_log};
    use std::fs;

    #[test]
    fn parses_inclusion_lane_profile_summary_from_logs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let log_path = temp.path().join("sequencer.log");
        let content = r#"
2026-03-01T00:00:00Z  INFO x: inclusion lane metrics window_ms=5000 user_op_app_execute_phase_ms=120 user_op_persist_phase_ms=80 user_op_app_share_pct_of_app_plus_persist=60.0 user_op_persist_share_pct_of_app_plus_persist=40.0
2026-03-01T00:00:05Z  INFO x: inclusion lane metrics window_ms=5000 user_op_app_execute_phase_ms=140 user_op_persist_phase_ms=60 user_op_app_share_pct_of_app_plus_persist=70.0 user_op_persist_share_pct_of_app_plus_persist=30.0
"#;
        fs::write(log_path.as_path(), content).expect("write log");

        let report = parse_inclusion_lane_profile_from_log(log_path.as_path())
            .expect("parse result")
            .expect("profile present");

        assert_eq!(report.samples, 2);
        assert_eq!(report.latest_user_op_app_execute_phase_ms, Some(140));
        assert_eq!(report.latest_user_op_persist_phase_ms, Some(60));
        assert_eq!(
            report.avg_user_op_app_share_pct_of_app_plus_persist,
            Some(65.0)
        );
        assert_eq!(
            report.avg_user_op_persist_share_pct_of_app_plus_persist,
            Some(35.0)
        );
    }

    #[test]
    fn default_log_path_uses_results_dir() {
        let value = default_sequencer_log_path("ack-latency");
        assert!(value.to_string_lossy().contains("benchmarks/results/"));
        assert!(value.to_string_lossy().contains("ack-latency"));
    }

    #[test]
    fn parses_ansi_colored_profile_lines() {
        let temp = tempfile::tempdir().expect("tempdir");
        let log_path = temp.path().join("sequencer.log");
        let content = "\u{1b}[2m2026-03-01T00:00:00Z\u{1b}[0m \u{1b}[32mINFO\u{1b}[0m x: inclusion lane metrics \u{1b}[3mwindow_ms\u{1b}[0m\u{1b}[2m=\u{1b}[0m5000 \u{1b}[3muser_op_app_share_pct_of_app_plus_persist\u{1b}[0m\u{1b}[2m=\u{1b}[0m60.5 \u{1b}[3muser_op_persist_share_pct_of_app_plus_persist\u{1b}[0m\u{1b}[2m=\u{1b}[0m39.5\n";
        fs::write(log_path.as_path(), content).expect("write log");

        let report = parse_inclusion_lane_profile_from_log(log_path.as_path())
            .expect("parse result")
            .expect("profile present");
        assert_eq!(report.samples, 1);
        assert_eq!(report.latest_window_ms, Some(5000));
        assert_eq!(
            report.latest_user_op_app_share_pct_of_app_plus_persist,
            Some(60.5)
        );
    }
}
