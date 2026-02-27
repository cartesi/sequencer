// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_sol_types::Eip712Domain;
use app_core::user_op::SignedUserOp;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use tracing_subscriber::EnvFilter;

use sequencer::api::AppState;
use sequencer::application::{WalletApp, WalletConfig};
use sequencer::inclusion_lane::{
    InclusionLane, InclusionLaneConfig, InclusionLaneError, InclusionLaneInput,
};
use sequencer::l2_tx_broadcaster::{L2TxBroadcaster, L2TxBroadcasterConfig};
use sequencer::storage;

const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:3000";
const DEFAULT_DB_PATH: &str = "sequencer.db";
const DEFAULT_QUEUE_CAP: usize = 8192;
const DEFAULT_QUEUE_TIMEOUT_MS: u64 = 100;
const DEFAULT_MAX_USER_OPS_PER_CHUNK: usize = 1024;
const DEFAULT_SAFE_DIRECT_BUFFER_CAPACITY: usize = 2048;
const DEFAULT_MAX_BATCH_OPEN_DURATION: Duration = Duration::from_secs(2 * 60 * 60);
const DEFAULT_MAX_BATCH_USER_OP_BYTES: usize = 1_048_576; // 1 MiB
const DEFAULT_INCLUSION_LANE_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(2);
const DEFAULT_BROADCASTER_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const DEFAULT_BROADCASTER_PAGE_SIZE: usize = 256;
const DEFAULT_BROADCASTER_SUBSCRIBER_BUFFER_CAPACITY: usize = 32_768;
const DEFAULT_OVERLOAD_QUEUE_DEPTH_THRESHOLD_NUMERATOR: usize = 9;
const DEFAULT_OVERLOAD_QUEUE_DEPTH_THRESHOLD_DENOMINATOR: usize = 10;
const DEFAULT_OVERLOAD_MAX_INFLIGHT_MULTIPLIER: usize = 2;
const DEFAULT_RUNTIME_METRICS_ENABLED: bool = false;
const DEFAULT_RUNTIME_METRICS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_MAX_BODY_BYTES: usize = 128 * 1024;
const DEFAULT_DOMAIN_NAME: &str = "CartesiAppSequencer";
const DEFAULT_DOMAIN_VERSION: &str = "1";
const DEFAULT_DOMAIN_CHAIN_ID: u64 = 1;
const DEFAULT_DOMAIN_VERIFYING_CONTRACT: &str = "0x0000000000000000000000000000000000000000";

fn default_overload_queue_depth_threshold(queue_capacity: usize) -> usize {
    let threshold = queue_capacity.saturating_mul(DEFAULT_OVERLOAD_QUEUE_DEPTH_THRESHOLD_NUMERATOR)
        / DEFAULT_OVERLOAD_QUEUE_DEPTH_THRESHOLD_DENOMINATOR;
    threshold.max(1)
}

fn default_overload_max_inflight_submissions(queue_capacity: usize) -> usize {
    queue_capacity
        .saturating_mul(DEFAULT_OVERLOAD_MAX_INFLIGHT_MULTIPLIER)
        .max(1)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        return handle_config_command(command);
    }

    run(cli.run).await
}

async fn run(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_run_args(args)
        .map_err(|reason| std::io::Error::other(format!("invalid configuration: {reason}")))?;
    log_effective_config(&config);
    let domain = config.build_domain()?;

    let mut storage = storage::Storage::open(&config.db_path, config.sqlite_synchronous.pragma())?;
    force_zero_frame_fee_for_now(&mut storage)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<InclusionLaneInput>(config.queue_capacity);

    let inclusion_lane = InclusionLane::new(
        rx,
        WalletApp::new(WalletConfig),
        storage,
        InclusionLaneConfig {
            max_user_ops_per_chunk: config.max_user_ops_per_chunk,
            safe_direct_buffer_capacity: config.safe_direct_buffer_capacity,
            max_batch_open: config.max_batch_open,
            max_batch_user_op_bytes: config.max_batch_user_op_bytes,
            idle_poll_interval: config.inclusion_lane_idle_poll_interval,
            metrics_enabled: config.runtime_metrics_enabled,
            metrics_log_interval: config.runtime_metrics_log_interval,
        },
    );
    let (mut inclusion_lane_handle, inclusion_lane_stop) = inclusion_lane.spawn();
    let broadcaster = L2TxBroadcaster::start(
        config.db_path.clone(),
        L2TxBroadcasterConfig {
            idle_poll_interval: config.broadcaster_idle_poll_interval,
            page_size: config.broadcaster_page_size,
            subscriber_buffer_capacity: config.broadcaster_subscriber_buffer_capacity,
            metrics_enabled: config.runtime_metrics_enabled,
            metrics_log_interval: config.runtime_metrics_log_interval,
        },
    )
    .map_err(|reason| format!("failed to start l2 tx broadcaster: {reason}"))?;
    let broadcaster_shutdown = broadcaster.clone();

    let state = Arc::new(AppState {
        tx_sender: tx,
        domain,
        queue_capacity: config.queue_capacity,
        overload_queue_depth_threshold: config.overload_queue_depth_threshold,
        overload_max_inflight_submissions: config.overload_max_inflight_submissions,
        inflight_submissions: Arc::new(AtomicUsize::new(0)),
        queue_timeout: config.queue_timeout,
        broadcaster,
    });

    let app = sequencer::api::router(state, config.max_body_bytes);
    let listener = tokio::net::TcpListener::bind(&config.http_addr).await?;

    tracing::info!(address = %config.http_addr, "listening");
    tokio::select! {
        server_result = axum::serve(listener, app) => {
            broadcaster_shutdown.request_shutdown();
            inclusion_lane_stop.request_shutdown();
            let lane_result = inclusion_lane_handle.await;
            match lane_result {
                Ok(InclusionLaneError::ShutdownRequested) => {}
                Ok(err) => return Err(format!("inclusion lane exited during shutdown: {err}").into()),
                Err(join_err) => {
                    return Err(format!("inclusion lane join error during shutdown: {join_err}").into())
                }
            }
            server_result?;
        }
        lane_result = &mut inclusion_lane_handle => {
            broadcaster_shutdown.request_shutdown();
            match lane_result {
                Ok(err) => return Err(format!("inclusion lane exited: {err}").into()),
                Err(join_err) => {
                    return Err(format!("inclusion lane join error: {join_err}").into())
                }
            }
        }
    }

    Ok(())
}

fn handle_config_command(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Config {
            command: ConfigCommand::Print(args),
        } => {
            let config = Config::from_run_args(args).map_err(std::io::Error::other)?;
            println!("{}", serde_json::to_string_pretty(&config.effective())?);
            Ok(())
        }
        Command::Config {
            command: ConfigCommand::Validate(args),
        } => {
            let config = Config::from_run_args(args).map_err(std::io::Error::other)?;
            println!("configuration is valid");
            println!("{}", serde_json::to_string_pretty(&config.effective())?);
            Ok(())
        }
    }
}

fn log_effective_config(config: &Config) {
    match serde_json::to_string(&config.effective()) {
        Ok(json) => tracing::info!(effective_config = %json, "resolved sequencer config"),
        Err(err) => tracing::warn!(%err, "failed to serialize effective sequencer config"),
    }
}

struct Config {
    profile: Profile,
    http_addr: String,
    db_path: String,
    queue_capacity: usize,
    queue_timeout: Duration,
    overload_queue_depth_threshold: usize,
    overload_max_inflight_submissions: usize,
    max_user_ops_per_chunk: usize,
    safe_direct_buffer_capacity: usize,
    max_batch_open: Duration,
    max_batch_user_op_bytes: usize,
    inclusion_lane_idle_poll_interval: Duration,
    broadcaster_idle_poll_interval: Duration,
    broadcaster_page_size: usize,
    broadcaster_subscriber_buffer_capacity: usize,
    runtime_metrics_enabled: bool,
    runtime_metrics_log_interval: Duration,
    max_body_bytes: usize,
    sqlite_synchronous: SqliteSynchronous,
    domain_name: String,
    domain_version: String,
    domain_chain_id: u64,
    domain_verifying_contract: String,
}

#[derive(Debug, Parser)]
#[command(
    name = "sequencer",
    about = "Deterministic sequencer prototype with low-latency soft confirmations",
    version,
    after_help = "Examples:\n  sequencer --profile dev\n  sequencer --profile bench --max-user-ops-per-chunk 4096 --max-batch-open 30m\n  sequencer config print --profile safe\n  sequencer config validate --sqlite-synchronous FULL"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Print(RunArgs),
    Validate(RunArgs),
}

#[derive(Debug, Clone, Args)]
struct RunArgs {
    #[arg(long, env = "SEQ_PROFILE", value_enum, default_value_t = Profile::Dev)]
    profile: Profile,
    #[arg(long, env = "SEQ_HTTP_ADDR")]
    http_addr: Option<String>,
    #[arg(long, env = "SEQ_DB_PATH")]
    db_path: Option<String>,
    #[arg(long, env = "SEQ_QUEUE_CAP")]
    queue_capacity: Option<usize>,
    #[arg(
        long,
        env = "SEQ_QUEUE_TIMEOUT_MS",
        value_name = "DURATION",
        value_parser = parse_duration_ms_or_unit
    )]
    queue_timeout: Option<Duration>,
    #[arg(long, env = "SEQ_OVERLOAD_QUEUE_DEPTH_THRESHOLD")]
    overload_queue_depth_threshold: Option<usize>,
    #[arg(long, env = "SEQ_OVERLOAD_MAX_INFLIGHT_SUBMISSIONS")]
    overload_max_inflight_submissions: Option<usize>,
    #[arg(long, env = "SEQ_MAX_USER_OPS_PER_CHUNK")]
    max_user_ops_per_chunk: Option<usize>,
    #[arg(long, env = "SEQ_MAX_BATCH", hide = true)]
    legacy_max_batch: Option<usize>,
    #[arg(long, env = "SEQ_SAFE_DIRECT_BUFFER_CAPACITY")]
    safe_direct_buffer_capacity: Option<usize>,
    #[arg(
        long,
        env = "SEQ_MAX_BATCH_OPEN_MS",
        value_name = "DURATION",
        value_parser = parse_duration_ms_or_unit
    )]
    max_batch_open: Option<Duration>,
    #[arg(long, env = "SEQ_MAX_BATCH_USER_OP_BYTES")]
    max_batch_user_op_bytes: Option<usize>,
    #[arg(
        long,
        env = "SEQ_INCLUSION_LANE_IDLE_POLL_INTERVAL_MS",
        value_name = "DURATION",
        value_parser = parse_duration_ms_or_unit
    )]
    inclusion_lane_idle_poll_interval: Option<Duration>,
    #[arg(
        long,
        env = "SEQ_INCLUSION_LANE_TICK_INTERVAL_MS",
        hide = true,
        value_parser = parse_duration_ms_or_unit
    )]
    legacy_inclusion_lane_tick_interval: Option<Duration>,
    #[arg(
        long,
        env = "SEQ_COMMIT_LANE_TICK_INTERVAL_MS",
        hide = true,
        value_parser = parse_duration_ms_or_unit
    )]
    legacy_commit_lane_tick_interval: Option<Duration>,
    #[arg(
        long,
        env = "SEQ_BROADCASTER_IDLE_POLL_INTERVAL_MS",
        value_name = "DURATION",
        value_parser = parse_duration_ms_or_unit
    )]
    broadcaster_idle_poll_interval: Option<Duration>,
    #[arg(long, env = "SEQ_BROADCASTER_PAGE_SIZE")]
    broadcaster_page_size: Option<usize>,
    #[arg(long, env = "SEQ_BROADCASTER_SUBSCRIBER_BUFFER_CAPACITY")]
    broadcaster_subscriber_buffer_capacity: Option<usize>,
    #[arg(long, env = "SEQ_RUNTIME_METRICS_ENABLED")]
    runtime_metrics_enabled: Option<bool>,
    #[arg(
        long,
        env = "SEQ_RUNTIME_METRICS_LOG_INTERVAL_MS",
        value_name = "DURATION",
        value_parser = parse_duration_ms_or_unit
    )]
    runtime_metrics_log_interval: Option<Duration>,
    #[arg(long, env = "SEQ_MAX_BODY_BYTES")]
    max_body_bytes: Option<usize>,
    #[arg(long, env = "SEQ_SQLITE_SYNCHRONOUS", value_enum)]
    sqlite_synchronous: Option<SqliteSynchronous>,
    #[arg(long, env = "SEQ_DOMAIN_NAME")]
    domain_name: Option<String>,
    #[arg(long, env = "SEQ_DOMAIN_VERSION")]
    domain_version: Option<String>,
    #[arg(long, env = "SEQ_DOMAIN_CHAIN_ID")]
    domain_chain_id: Option<u64>,
    #[arg(long, env = "SEQ_DOMAIN_VERIFYING_CONTRACT")]
    domain_verifying_contract: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum Profile {
    Dev,
    Bench,
    Safe,
}

#[derive(Debug, Clone, Copy, Serialize, ValueEnum)]
#[serde(rename_all = "UPPERCASE")]
enum SqliteSynchronous {
    #[value(name = "OFF", alias = "off")]
    Off,
    #[value(name = "NORMAL", alias = "normal")]
    Normal,
    #[value(name = "FULL", alias = "full")]
    Full,
    #[value(name = "EXTRA", alias = "extra")]
    Extra,
}

impl SqliteSynchronous {
    fn pragma(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
            Self::Extra => "EXTRA",
        }
    }
}

struct ProfileDefaults {
    queue_capacity: usize,
    queue_timeout: Duration,
    max_user_ops_per_chunk: usize,
    safe_direct_buffer_capacity: usize,
    max_batch_open: Duration,
    max_batch_user_op_bytes: usize,
    inclusion_lane_idle_poll_interval: Duration,
    broadcaster_idle_poll_interval: Duration,
    broadcaster_page_size: usize,
    broadcaster_subscriber_buffer_capacity: usize,
    runtime_metrics_enabled: bool,
    runtime_metrics_log_interval: Duration,
    max_body_bytes: usize,
    sqlite_synchronous: SqliteSynchronous,
}

impl Profile {
    fn defaults(self) -> ProfileDefaults {
        match self {
            Self::Dev => ProfileDefaults {
                queue_capacity: DEFAULT_QUEUE_CAP,
                queue_timeout: Duration::from_millis(DEFAULT_QUEUE_TIMEOUT_MS),
                max_user_ops_per_chunk: DEFAULT_MAX_USER_OPS_PER_CHUNK,
                safe_direct_buffer_capacity: DEFAULT_SAFE_DIRECT_BUFFER_CAPACITY,
                max_batch_open: DEFAULT_MAX_BATCH_OPEN_DURATION,
                max_batch_user_op_bytes: DEFAULT_MAX_BATCH_USER_OP_BYTES,
                inclusion_lane_idle_poll_interval: DEFAULT_INCLUSION_LANE_IDLE_POLL_INTERVAL,
                broadcaster_idle_poll_interval: DEFAULT_BROADCASTER_IDLE_POLL_INTERVAL,
                broadcaster_page_size: DEFAULT_BROADCASTER_PAGE_SIZE,
                broadcaster_subscriber_buffer_capacity:
                    DEFAULT_BROADCASTER_SUBSCRIBER_BUFFER_CAPACITY,
                runtime_metrics_enabled: DEFAULT_RUNTIME_METRICS_ENABLED,
                runtime_metrics_log_interval: DEFAULT_RUNTIME_METRICS_LOG_INTERVAL,
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
                sqlite_synchronous: SqliteSynchronous::Normal,
            },
            Self::Bench => ProfileDefaults {
                queue_capacity: 32_768,
                queue_timeout: Duration::from_millis(100),
                max_user_ops_per_chunk: 4_096,
                safe_direct_buffer_capacity: 8_192,
                max_batch_open: DEFAULT_MAX_BATCH_OPEN_DURATION,
                max_batch_user_op_bytes: 1_572_864, // 1.5 MiB
                inclusion_lane_idle_poll_interval: Duration::from_millis(1),
                broadcaster_idle_poll_interval: Duration::from_millis(5),
                broadcaster_page_size: 1_024,
                broadcaster_subscriber_buffer_capacity: 131_072,
                runtime_metrics_enabled: DEFAULT_RUNTIME_METRICS_ENABLED,
                runtime_metrics_log_interval: DEFAULT_RUNTIME_METRICS_LOG_INTERVAL,
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
                sqlite_synchronous: SqliteSynchronous::Normal,
            },
            Self::Safe => ProfileDefaults {
                queue_capacity: DEFAULT_QUEUE_CAP,
                queue_timeout: Duration::from_millis(DEFAULT_QUEUE_TIMEOUT_MS),
                max_user_ops_per_chunk: DEFAULT_MAX_USER_OPS_PER_CHUNK,
                safe_direct_buffer_capacity: DEFAULT_SAFE_DIRECT_BUFFER_CAPACITY,
                max_batch_open: Duration::from_secs(60 * 60),
                max_batch_user_op_bytes: DEFAULT_MAX_BATCH_USER_OP_BYTES,
                inclusion_lane_idle_poll_interval: Duration::from_millis(5),
                broadcaster_idle_poll_interval: Duration::from_millis(25),
                broadcaster_page_size: DEFAULT_BROADCASTER_PAGE_SIZE,
                broadcaster_subscriber_buffer_capacity:
                    DEFAULT_BROADCASTER_SUBSCRIBER_BUFFER_CAPACITY,
                runtime_metrics_enabled: DEFAULT_RUNTIME_METRICS_ENABLED,
                runtime_metrics_log_interval: DEFAULT_RUNTIME_METRICS_LOG_INTERVAL,
                max_body_bytes: DEFAULT_MAX_BODY_BYTES,
                sqlite_synchronous: SqliteSynchronous::Full,
            },
        }
    }
}

impl Config {
    fn from_run_args(args: RunArgs) -> Result<Self, String> {
        let defaults = args.profile.defaults();
        let queue_capacity = args.queue_capacity.unwrap_or(defaults.queue_capacity);
        let overload_queue_depth_threshold = args
            .overload_queue_depth_threshold
            .unwrap_or_else(|| default_overload_queue_depth_threshold(queue_capacity));
        let overload_max_inflight_submissions = args
            .overload_max_inflight_submissions
            .unwrap_or_else(|| default_overload_max_inflight_submissions(queue_capacity));

        let config = Self {
            profile: args.profile,
            http_addr: args
                .http_addr
                .unwrap_or_else(|| DEFAULT_HTTP_ADDR.to_string()),
            db_path: args.db_path.unwrap_or_else(|| DEFAULT_DB_PATH.to_string()),
            queue_capacity,
            queue_timeout: args.queue_timeout.unwrap_or(defaults.queue_timeout),
            overload_queue_depth_threshold,
            overload_max_inflight_submissions,
            max_user_ops_per_chunk: args
                .max_user_ops_per_chunk
                .or(args.legacy_max_batch)
                .unwrap_or(defaults.max_user_ops_per_chunk),
            safe_direct_buffer_capacity: args
                .safe_direct_buffer_capacity
                .unwrap_or(defaults.safe_direct_buffer_capacity),
            max_batch_open: args.max_batch_open.unwrap_or(defaults.max_batch_open),
            max_batch_user_op_bytes: args
                .max_batch_user_op_bytes
                .unwrap_or(defaults.max_batch_user_op_bytes),
            inclusion_lane_idle_poll_interval: args
                .inclusion_lane_idle_poll_interval
                .or(args.legacy_inclusion_lane_tick_interval)
                .or(args.legacy_commit_lane_tick_interval)
                .unwrap_or(defaults.inclusion_lane_idle_poll_interval),
            broadcaster_idle_poll_interval: args
                .broadcaster_idle_poll_interval
                .unwrap_or(defaults.broadcaster_idle_poll_interval),
            broadcaster_page_size: args
                .broadcaster_page_size
                .unwrap_or(defaults.broadcaster_page_size),
            broadcaster_subscriber_buffer_capacity: args
                .broadcaster_subscriber_buffer_capacity
                .unwrap_or(defaults.broadcaster_subscriber_buffer_capacity),
            runtime_metrics_enabled: args
                .runtime_metrics_enabled
                .unwrap_or(defaults.runtime_metrics_enabled),
            runtime_metrics_log_interval: args
                .runtime_metrics_log_interval
                .unwrap_or(defaults.runtime_metrics_log_interval),
            max_body_bytes: args.max_body_bytes.unwrap_or(defaults.max_body_bytes),
            sqlite_synchronous: args
                .sqlite_synchronous
                .unwrap_or(defaults.sqlite_synchronous),
            domain_name: args
                .domain_name
                .unwrap_or_else(|| DEFAULT_DOMAIN_NAME.to_string()),
            domain_version: args
                .domain_version
                .unwrap_or_else(|| DEFAULT_DOMAIN_VERSION.to_string()),
            domain_chain_id: args.domain_chain_id.unwrap_or(DEFAULT_DOMAIN_CHAIN_ID),
            domain_verifying_contract: args
                .domain_verifying_contract
                .unwrap_or_else(|| DEFAULT_DOMAIN_VERIFYING_CONTRACT.to_string()),
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.http_addr.trim().is_empty() {
            return Err("http_addr cannot be empty".to_string());
        }
        if self.db_path.trim().is_empty() {
            return Err("db_path cannot be empty".to_string());
        }
        if self.queue_capacity == 0 {
            return Err("queue_capacity must be > 0".to_string());
        }
        if self.queue_timeout.is_zero() {
            return Err("queue_timeout must be > 0".to_string());
        }
        if self.overload_queue_depth_threshold == 0 {
            return Err("overload_queue_depth_threshold must be > 0".to_string());
        }
        if self.overload_queue_depth_threshold > self.queue_capacity {
            return Err(format!(
                "overload_queue_depth_threshold must be <= queue_capacity ({} > {})",
                self.overload_queue_depth_threshold, self.queue_capacity
            ));
        }
        if self.overload_max_inflight_submissions == 0 {
            return Err("overload_max_inflight_submissions must be > 0".to_string());
        }
        if self.max_user_ops_per_chunk == 0 {
            return Err("max_user_ops_per_chunk must be > 0".to_string());
        }
        if self.safe_direct_buffer_capacity == 0 {
            return Err("safe_direct_buffer_capacity must be > 0".to_string());
        }
        if self.max_batch_open.is_zero() {
            return Err("max_batch_open must be > 0".to_string());
        }
        if self.max_batch_user_op_bytes == 0 {
            return Err("max_batch_user_op_bytes must be > 0".to_string());
        }
        if self.max_batch_user_op_bytes < SignedUserOp::max_batch_bytes_upper_bound() {
            return Err(format!(
                "max_batch_user_op_bytes must be >= {} (one max-sized user op)",
                SignedUserOp::max_batch_bytes_upper_bound()
            ));
        }
        if self.inclusion_lane_idle_poll_interval.is_zero() {
            return Err("inclusion_lane_idle_poll_interval must be > 0".to_string());
        }
        if self.broadcaster_idle_poll_interval.is_zero() {
            return Err("broadcaster_idle_poll_interval must be > 0".to_string());
        }
        if self.broadcaster_page_size == 0 {
            return Err("broadcaster_page_size must be > 0".to_string());
        }
        if self.broadcaster_subscriber_buffer_capacity == 0 {
            return Err("broadcaster_subscriber_buffer_capacity must be > 0".to_string());
        }
        if self.runtime_metrics_log_interval.is_zero() {
            return Err("runtime_metrics_log_interval must be > 0".to_string());
        }
        if self.max_body_bytes == 0 {
            return Err("max_body_bytes must be > 0".to_string());
        }
        if self.domain_name.trim().is_empty() {
            return Err("domain_name cannot be empty".to_string());
        }
        if self.domain_version.trim().is_empty() {
            return Err("domain_version cannot be empty".to_string());
        }
        Ok(())
    }

    fn build_domain(&self) -> Result<Eip712Domain, String> {
        let verifying_contract = parse_address(&self.domain_verifying_contract)?;
        Ok(Eip712Domain {
            name: Some(self.domain_name.clone().into()),
            version: Some(self.domain_version.clone().into()),
            chain_id: Some(U256::from(self.domain_chain_id)),
            verifying_contract: Some(verifying_contract),
            salt: None,
        })
    }

    fn effective(&self) -> EffectiveConfig {
        EffectiveConfig {
            profile: self.profile,
            http_addr: self.http_addr.clone(),
            db_path: self.db_path.clone(),
            queue_capacity: self.queue_capacity,
            queue_timeout: DurationValue::from(self.queue_timeout),
            overload_queue_depth_threshold: self.overload_queue_depth_threshold,
            overload_max_inflight_submissions: self.overload_max_inflight_submissions,
            max_user_ops_per_chunk: self.max_user_ops_per_chunk,
            safe_direct_buffer_capacity: self.safe_direct_buffer_capacity,
            max_batch_open: DurationValue::from(self.max_batch_open),
            max_batch_user_op_bytes: self.max_batch_user_op_bytes,
            inclusion_lane_idle_poll_interval: DurationValue::from(
                self.inclusion_lane_idle_poll_interval,
            ),
            broadcaster_idle_poll_interval: DurationValue::from(
                self.broadcaster_idle_poll_interval,
            ),
            broadcaster_page_size: self.broadcaster_page_size,
            broadcaster_subscriber_buffer_capacity: self.broadcaster_subscriber_buffer_capacity,
            runtime_metrics_enabled: self.runtime_metrics_enabled,
            runtime_metrics_log_interval: DurationValue::from(self.runtime_metrics_log_interval),
            max_body_bytes: self.max_body_bytes,
            sqlite_synchronous: self.sqlite_synchronous,
            domain_name: self.domain_name.clone(),
            domain_version: self.domain_version.clone(),
            domain_chain_id: self.domain_chain_id,
            domain_verifying_contract: self.domain_verifying_contract.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct EffectiveConfig {
    profile: Profile,
    http_addr: String,
    db_path: String,
    queue_capacity: usize,
    queue_timeout: DurationValue,
    overload_queue_depth_threshold: usize,
    overload_max_inflight_submissions: usize,
    max_user_ops_per_chunk: usize,
    safe_direct_buffer_capacity: usize,
    max_batch_open: DurationValue,
    max_batch_user_op_bytes: usize,
    inclusion_lane_idle_poll_interval: DurationValue,
    broadcaster_idle_poll_interval: DurationValue,
    broadcaster_page_size: usize,
    broadcaster_subscriber_buffer_capacity: usize,
    runtime_metrics_enabled: bool,
    runtime_metrics_log_interval: DurationValue,
    max_body_bytes: usize,
    sqlite_synchronous: SqliteSynchronous,
    domain_name: String,
    domain_version: String,
    domain_chain_id: u64,
    domain_verifying_contract: String,
}

#[derive(Debug, Clone, Serialize)]
struct DurationValue {
    ms: u64,
    human: String,
}

impl From<Duration> for DurationValue {
    fn from(value: Duration) -> Self {
        Self {
            ms: duration_millis_u64(value),
            human: format_duration(value),
        }
    }
}

fn parse_address(value: &str) -> Result<Address, String> {
    if !value.starts_with("0x") {
        return Err("verifying contract must be 0x-prefixed".to_string());
    }
    let bytes = alloy_primitives::hex::decode(value)
        .map_err(|err| format!("invalid verifying contract hex: {err}"))?;
    if bytes.len() != 20 {
        return Err("verifying contract must be 20 bytes".to_string());
    }
    Ok(Address::from_slice(&bytes))
}

fn parse_duration_ms_or_unit(raw: &str) -> Result<Duration, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("duration cannot be empty".to_string());
    }

    if let Ok(ms) = value.parse::<u64>() {
        return Ok(Duration::from_millis(ms));
    }

    if let Some(ms) = value.strip_suffix("ms") {
        let value = ms
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("invalid milliseconds duration: {raw}"))?;
        return Ok(Duration::from_millis(value));
    }
    if let Some(seconds) = value.strip_suffix('s') {
        let value = seconds
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("invalid seconds duration: {raw}"))?;
        return Ok(Duration::from_secs(value));
    }
    if let Some(minutes) = value.strip_suffix('m') {
        let value = minutes
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("invalid minutes duration: {raw}"))?;
        let seconds = value
            .checked_mul(60)
            .ok_or_else(|| format!("duration overflow: {raw}"))?;
        return Ok(Duration::from_secs(seconds));
    }
    if let Some(hours) = value.strip_suffix('h') {
        let value = hours
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("invalid hours duration: {raw}"))?;
        let seconds = value
            .checked_mul(60 * 60)
            .ok_or_else(|| format!("duration overflow: {raw}"))?;
        return Ok(Duration::from_secs(seconds));
    }

    Err(format!(
        "invalid duration '{raw}'. Use plain milliseconds (e.g. 250) or suffix: ms, s, m, h"
    ))
}

fn format_duration(value: Duration) -> String {
    let ms = duration_millis_u64(value);
    if ms.is_multiple_of(60 * 60 * 1000) {
        return format!("{}h", ms / (60 * 60 * 1000));
    }
    if ms.is_multiple_of(60 * 1000) {
        return format!("{}m", ms / (60 * 1000));
    }
    if ms.is_multiple_of(1000) {
        return format!("{}s", ms / 1000);
    }
    format!("{ms}ms")
}

fn duration_millis_u64(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}

fn force_zero_frame_fee_for_now(
    storage: &mut storage::Storage,
) -> Result<(), Box<dyn std::error::Error>> {
    // Temporary prototype policy: keep sequencer frame fee at zero until fee estimation lands.
    storage.set_recommended_fee(0)?;
    let mut head = storage.load_open_state()?;
    if head.frame_fee != 0 {
        storage.close_frame_only(&mut head, 0, 0)?;
    }
    Ok(())
}
