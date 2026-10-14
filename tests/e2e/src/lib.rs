// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod cold_replica;
pub mod test_cases;
mod watchdog_compare;

// Link the reference engine's C symbols for the EngineApp replica scenarios.
use c_wallet_engine as _;

use std::future::Future;
use std::pin::Pin;

use rollups_harness::{
    ManagedSequencer, ManagedSequencerConfig, default_c_wallet_sequencer_config,
    default_devnet_sequencer_config,
};

pub type MimicResult = Result<(), libtest_mimic::Failed>;
pub type ScenarioResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub type ScenarioFuture<'a> = Pin<Box<dyn Future<Output = ScenarioResult<()>> + Send + 'a>>;
pub type ScenarioFn = for<'a> fn(&'a mut ManagedSequencer) -> ScenarioFuture<'a>;

/// Which sequencer binary a scenario runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Rust,
    C,
}

#[derive(Clone, Copy)]
pub struct Scenario {
    pub name: &'static str,
    pub host: Host,
    pub run: ScenarioFn,
    /// Overrides applied to the host's default spawn config.
    pub configure: fn(&mut ManagedSequencerConfig),
}

impl Scenario {
    pub fn with(self, configure: fn(&mut ManagedSequencerConfig)) -> Self {
        Self { configure, ..self }
    }

    pub fn spawn_config(&self) -> ManagedSequencerConfig {
        let log_prefix = format!("rollups-e2e-{}", self.name);
        let mut config = match self.host {
            Host::Rust => default_devnet_sequencer_config(log_prefix),
            Host::C => default_c_wallet_sequencer_config(log_prefix),
        };
        (self.configure)(&mut config);
        config
    }
}

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
