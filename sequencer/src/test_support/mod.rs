// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shared `#[cfg(test)]` fixtures for the sequencer crate.

use std::time::Duration;

use alloy_primitives::Address;
use tempfile::TempDir;

use crate::ingress::inclusion_lane::InclusionLaneError;
use crate::runtime::shutdown::RuntimeScope;
use crate::storage::{DeploymentIdentity, FeeOracleIdentity, Storage};

pub(crate) mod server;
mod stub_app;

pub(crate) use stub_app::{ReplayEvent, StubApp, create_structured_dump};

pub(crate) struct TestDb {
    pub _dir: TempDir,
    pub path: String,
}

pub(crate) fn temp_db(name: &str) -> TestDb {
    let dir = tempfile::Builder::new()
        .prefix(format!("sequencer-{name}-").as_str())
        .tempdir()
        .expect("create temporary test directory");
    let path = dir.path().join("sequencer.sqlite");
    TestDb {
        _dir: dir,
        path: path.to_string_lossy().into_owned(),
    }
}

/// Pin the trusted deployment identity a production setup establishes before
/// any fresh-Tip attribution can classify safe inputs.
pub(crate) fn pin_test_deployment_identity(
    storage: &mut Storage,
    batch_submitter_address: Address,
) {
    storage
        .load_or_insert_deployment_identity(DeploymentIdentity {
            chain_id: 1,
            app_address: Address::repeat_byte(0x11),
            input_box_address: Address::repeat_byte(0x22),
            app_deployment_block: 0,
            batch_submitter_address,
            fee_oracle: FeeOracleIdentity::Fixed { log_gas_price: 0 },
        })
        .expect("pin test deployment identity");
}

/// Poll `predicate` every 5 ms until it holds or `timeout` passes; returns its
/// final value.
pub(crate) async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let started = tokio::time::Instant::now();
    while started.elapsed() < timeout {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    predicate()
}

pub(crate) async fn shutdown_lane(
    shutdown: &RuntimeScope,
    handle: tokio::task::JoinHandle<Result<(), InclusionLaneError>>,
) {
    shutdown.request_shutdown();
    let joined = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("wait for lane shutdown");
    let result = joined.expect("join lane task");
    assert!(result.is_ok(), "lane should shut down cleanly: {result:?}");
}
