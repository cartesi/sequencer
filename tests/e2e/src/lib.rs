// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

pub mod test_cases;

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
    rollups_harness::teardown_sqlite_artifacts(std::path::Path::new(
        rollups_harness::DEFAULT_TEST_LOGS_DIR,
    ))
    .map_err(failed)?;

    let outcome = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(failed)?
        .block_on(scenario());

    let teardown_result = rollups_harness::teardown_sqlite_artifacts(std::path::Path::new(
        rollups_harness::DEFAULT_TEST_LOGS_DIR,
    ));

    match (outcome, teardown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(err), _) => Err(libtest_mimic::Failed::from(format!(
            "{scenario_name}: {err}"
        ))),
        (Ok(()), Err(err)) => Err(libtest_mimic::Failed::from(format!(
            "{scenario_name} teardown failed: {err}"
        ))),
    }
}

fn failed(err: impl std::fmt::Display) -> libtest_mimic::Failed {
    libtest_mimic::Failed::from(err.to_string())
}
