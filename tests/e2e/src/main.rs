// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use libtest_mimic::{Arguments, Trial};
use rollups_e2e::run_trial;
use rollups_harness::{ManagedSequencer, default_devnet_sequencer_config};

fn main() {
    let mut args = Arguments::from_args();
    args.test_threads = Some(1);

    let trials: Vec<Trial> = rollups_e2e::test_cases::test_cases()
        .into_iter()
        .map(|(name, scenario)| {
            Trial::test(name, move || {
                let log_prefix = format!("rollups-e2e-{name}");
                run_trial(name, || async move {
                    let mut runtime =
                        ManagedSequencer::spawn(default_devnet_sequencer_config(log_prefix))
                            .await?;
                    let scenario_result = scenario(&mut runtime).await;
                    let shutdown_result = runtime.shutdown().await;
                    shutdown_result?;
                    scenario_result
                })
            })
        })
        .collect();

    libtest_mimic::run(&args, trials).exit();
}
