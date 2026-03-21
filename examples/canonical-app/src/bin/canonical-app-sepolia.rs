// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use app_core::application::{WalletApp, WalletConfig};
use canonical_app::{SchedulerConfig, run_scheduler_forever};
use trolley::cmt::RollupCmt;

fn main() {
    let rollup = RollupCmt::try_new().expect("failed to initialize rollup");
    let app = WalletApp::new(WalletConfig::sepolia());
    run_scheduler_forever(rollup, app, SchedulerConfig::sepolia());
}
