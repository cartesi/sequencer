// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_sol_types::Eip712Domain;
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
const DEFAULT_QUEUE_CAP: usize = 1024;
const DEFAULT_QUEUE_TIMEOUT_MS: u64 = 100;
const DEFAULT_MAX_USER_OPS_PER_CHUNK: usize = 64;
const DEFAULT_SAFE_DIRECT_BUFFER_CAPACITY: usize = 256;
const DEFAULT_MAX_BATCH_OPEN_DURATION: Duration = Duration::from_secs(2 * 60 * 60);
const DEFAULT_MAX_BATCH_USER_OP_BYTES: usize = 1_048_576; // 1 MiB
const DEFAULT_INCLUSION_LANE_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(2);
const DEFAULT_BROADCASTER_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const DEFAULT_BROADCASTER_PAGE_SIZE: usize = 256;
const DEFAULT_BROADCASTER_SUBSCRIBER_BUFFER_CAPACITY: usize = 1024;
const DEFAULT_MAX_BODY_BYTES: usize = 128 * 1024;
const DEFAULT_SQLITE_SYNCHRONOUS: &str = "NORMAL";
const DEFAULT_DOMAIN_NAME: &str = "CartesiAppSequencer";
const DEFAULT_DOMAIN_VERSION: &str = "1";
const DEFAULT_DOMAIN_CHAIN_ID: u64 = 1;
const DEFAULT_DOMAIN_VERIFYING_CONTRACT: &str = "0x0000000000000000000000000000000000000000";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();
    let domain = config.build_domain()?;

    let storage = storage::Storage::open(&config.db_path, &config.sqlite_synchronous)?;
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
        },
    );
    let (mut inclusion_lane_handle, inclusion_lane_stop) = inclusion_lane.spawn();
    let broadcaster = L2TxBroadcaster::start(
        config.db_path.clone(),
        L2TxBroadcasterConfig {
            idle_poll_interval: config.broadcaster_idle_poll_interval,
            page_size: config.broadcaster_page_size,
            subscriber_buffer_capacity: config.broadcaster_subscriber_buffer_capacity,
        },
    )
    .map_err(|reason| format!("failed to start l2 tx broadcaster: {reason}"))?;

    let state = Arc::new(AppState {
        tx_sender: tx,
        domain,
        queue_timeout: std::time::Duration::from_millis(config.queue_timeout_ms),
        broadcaster,
    });

    let app = sequencer::api::router(state, config.max_body_bytes);
    let listener = tokio::net::TcpListener::bind(&config.http_addr).await?;

    tracing::info!(address = %config.http_addr, "listening");
    tokio::select! {
        server_result = axum::serve(listener, app) => {
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

struct Config {
    http_addr: String,
    db_path: String,
    queue_capacity: usize,
    queue_timeout_ms: u64,
    max_user_ops_per_chunk: usize,
    safe_direct_buffer_capacity: usize,
    max_batch_open: Duration,
    max_batch_user_op_bytes: usize,
    inclusion_lane_idle_poll_interval: Duration,
    broadcaster_idle_poll_interval: Duration,
    broadcaster_page_size: usize,
    broadcaster_subscriber_buffer_capacity: usize,
    max_body_bytes: usize,
    sqlite_synchronous: String,
    domain_name: String,
    domain_version: String,
    domain_chain_id: u64,
    domain_verifying_contract: String,
}

impl Config {
    fn from_env() -> Self {
        Self {
            http_addr: env_string("SEQ_HTTP_ADDR", DEFAULT_HTTP_ADDR),
            db_path: env_string("SEQ_DB_PATH", DEFAULT_DB_PATH),
            queue_capacity: env_usize("SEQ_QUEUE_CAP", DEFAULT_QUEUE_CAP).max(1),
            queue_timeout_ms: env_u64("SEQ_QUEUE_TIMEOUT_MS", DEFAULT_QUEUE_TIMEOUT_MS),
            max_user_ops_per_chunk: env_usize(
                "SEQ_MAX_USER_OPS_PER_CHUNK",
                env_usize("SEQ_MAX_BATCH", DEFAULT_MAX_USER_OPS_PER_CHUNK),
            )
            .max(1),
            safe_direct_buffer_capacity: env_usize(
                "SEQ_SAFE_DIRECT_BUFFER_CAPACITY",
                DEFAULT_SAFE_DIRECT_BUFFER_CAPACITY,
            )
            .max(1),
            max_batch_open: Duration::from_millis(
                env_u64(
                    "SEQ_MAX_BATCH_OPEN_MS",
                    DEFAULT_MAX_BATCH_OPEN_DURATION.as_millis() as u64,
                )
                .max(1),
            ),
            max_batch_user_op_bytes: env_usize(
                "SEQ_MAX_BATCH_USER_OP_BYTES",
                DEFAULT_MAX_BATCH_USER_OP_BYTES,
            )
            .max(1),
            inclusion_lane_idle_poll_interval: Duration::from_millis(
                env_u64(
                    "SEQ_INCLUSION_LANE_IDLE_POLL_INTERVAL_MS",
                    env_u64(
                        "SEQ_INCLUSION_LANE_TICK_INTERVAL_MS",
                        env_u64(
                            "SEQ_COMMIT_LANE_TICK_INTERVAL_MS",
                            DEFAULT_INCLUSION_LANE_IDLE_POLL_INTERVAL.as_millis() as u64,
                        ),
                    ),
                )
                .max(1),
            ),
            broadcaster_idle_poll_interval: Duration::from_millis(
                env_u64(
                    "SEQ_BROADCASTER_IDLE_POLL_INTERVAL_MS",
                    DEFAULT_BROADCASTER_IDLE_POLL_INTERVAL.as_millis() as u64,
                )
                .max(1),
            ),
            broadcaster_page_size: env_usize(
                "SEQ_BROADCASTER_PAGE_SIZE",
                DEFAULT_BROADCASTER_PAGE_SIZE,
            )
            .max(1),
            broadcaster_subscriber_buffer_capacity: env_usize(
                "SEQ_BROADCASTER_SUBSCRIBER_BUFFER_CAPACITY",
                DEFAULT_BROADCASTER_SUBSCRIBER_BUFFER_CAPACITY,
            )
            .max(1),
            max_body_bytes: env_usize("SEQ_MAX_BODY_BYTES", DEFAULT_MAX_BODY_BYTES),
            sqlite_synchronous: env_string("SEQ_SQLITE_SYNCHRONOUS", DEFAULT_SQLITE_SYNCHRONOUS),
            domain_name: env_string("SEQ_DOMAIN_NAME", DEFAULT_DOMAIN_NAME),
            domain_version: env_string("SEQ_DOMAIN_VERSION", DEFAULT_DOMAIN_VERSION),
            domain_chain_id: env_u64("SEQ_DOMAIN_CHAIN_ID", DEFAULT_DOMAIN_CHAIN_ID),
            domain_verifying_contract: env_string(
                "SEQ_DOMAIN_VERIFYING_CONTRACT",
                DEFAULT_DOMAIN_VERIFYING_CONTRACT,
            ),
        }
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
}

fn env_string(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
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
