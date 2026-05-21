// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

pub mod test_cases;
mod watchdog_compare;

use std::future::Future;
use std::pin::Pin;

use rollups_harness::ManagedSequencer;

pub type MimicResult = Result<(), libtest_mimic::Failed>;
pub type ScenarioResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub type ScenarioFuture<'a> = Pin<Box<dyn Future<Output = ScenarioResult<()>> + Send + 'a>>;
pub type ScenarioFn = for<'a> fn(&'a mut ManagedSequencer) -> ScenarioFuture<'a>;

pub fn run_trial<F, Fut>(scenario_name: &str, scenario: F) -> MimicResult
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ScenarioResult<()>>,
{
    let outcome = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(failed)?
        .block_on(scenario());

    outcome.map_err(|err| libtest_mimic::Failed::from(format!("{scenario_name}: {err}")))
}

fn failed(err: impl std::fmt::Display) -> libtest_mimic::Failed {
    libtest_mimic::Failed::from(err.to_string())
}
