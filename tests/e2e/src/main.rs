// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use libtest_mimic::{Arguments, Trial};
use rollups_e2e::run_trial;
use rollups_harness::{
    ManagedSequencer, default_devnet_sequencer_config, devnet_sequencer_config_no_faketime,
};

fn main() {
    let mut args = Arguments::from_args();
    args.test_threads = Some(1);

    let trials: Vec<Trial> = rollups_e2e::test_cases::test_cases()
        .into_iter()
        .map(|(name, scenario)| {
            Trial::test(name, move || {
                let log_prefix = format!("rollups-e2e-{name}");
                let spawn_config = if name == "watchdog_genesis_compare_test"
                    || name == "deposit_transfer_withdrawal_test"
                    || name == "watchdog_non_genesis_divergence_test"
                {
                    devnet_sequencer_config_no_faketime(log_prefix)
                } else if name == "fixed_fee_oracle_sets_frame_fee_test" {
                    let mut config = default_devnet_sequencer_config(log_prefix);
                    // 100 → recommended fee 1456, under the wallet client's
                    // DEFAULT_MAX_FEE (2500) so transfers still admit.
                    config.fee_oracle_fixed_log_gas_price = Some(100);
                    config
                } else {
                    default_devnet_sequencer_config(log_prefix)
                };
                run_trial(name, || async move {
                    let mut runtime = ManagedSequencer::spawn(spawn_config).await?;
                    let scenario_result = scenario(&mut runtime).await;
                    // Post-test schema invariants: assert the DB's structural
                    // invariants only if the scenario succeeded — otherwise
                    // we'd mask the original failure with downstream
                    // weirdness. Checks the partial unique index, nonce
                    // contiguity, and FK validity directly against the DB
                    // file.
                    let invariant_result = if scenario_result.is_ok() {
                        runtime.assert_schema_invariants()
                    } else {
                        Ok(())
                    };
                    let shutdown_result = runtime.shutdown().await;
                    shutdown_result?;
                    invariant_result?;
                    scenario_result
                })
            })
        })
        .collect();

    libtest_mimic::run(&args, trials).exit();
}
