// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

pub mod exit_drive;
pub mod scheduler;

pub use scheduler::{SchedulerConfig, run_scheduler_forever};

pub fn run_wallet_scheduler_forever<R: trolley::Rollup>(
    rollup: R,
    app: app_core::application::WalletApp,
    config: SchedulerConfig,
) -> ! {
    let mut drive = exit_drive::ExitDrive::open("/dev/pmem1").expect("open wallet exit drive");
    scheduler::run_scheduler_observed(rollup, app, config, move |scheduler| {
        drive.publish(scheduler)
    })
}
