// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod core;

pub use core::{DEVNET_SEQUENCER_ADDRESS, SEPOLIA_SEQUENCER_ADDRESS, SchedulerConfig};

use sequencer_core::application::AppOutput;
use sequencer_core::application::Application;
use trolley::{Rollup, RollupRequest};
use types::{Notice, Voucher};

pub fn run_scheduler_forever<R: Rollup, A: Application>(
    mut rollup: R,
    app: A,
    scheduler_config: SchedulerConfig,
) -> ! {
    let mut scheduler = core::Scheduler::new(app, scheduler_config);

    loop {
        match rollup.next_input() {
            Ok(RollupRequest::Advance { metadata, payload }) => {
                let inclusion_block = core::block_to_u64(metadata.block_number);
                let domain = core::input_domain(
                    core::chain_id_to_u64(metadata.chain_id),
                    metadata.app_contract,
                );

                let input = core::SchedulerInput {
                    sender: metadata.msg_sender,
                    inclusion_block,
                    domain,
                    payload,
                };

                let result = scheduler.process_input(input);
                for output in &result.outputs {
                    emit_app_output(&mut rollup, output)
                        .unwrap_or_else(|err| panic!("scheduler failed to emit app output: {err}"));
                }
                if matches!(result.outcome, core::ProcessOutcome::BatchRejected(_)) {
                    rollup
                        .emit_report(b"scheduler dropped invalid batch")
                        .unwrap_or_else(|err| {
                            panic!("scheduler failed to emit invalid-batch report: {err}")
                        });
                }
            }
            Ok(RollupRequest::Inspect { .. }) => {
                rollup
                    .emit_report(b"scheduler inspect endpoint not implemented")
                    .unwrap_or_else(|err| {
                        panic!("scheduler failed to emit inspect report: {err}")
                    });
            }
            Err(err) => panic!("scheduler failed while reading next input: {err}"),
        }
    }
}

fn emit_app_output<R: Rollup>(rollup: &mut R, output: &AppOutput) -> trolley::RollupResult<()> {
    match output {
        AppOutput::Notice(payload) => {
            let notice = Notice {
                payload: payload.clone().into(),
            };
            rollup.emit_notice(&notice)
        }
        AppOutput::Voucher {
            destination,
            value,
            payload,
        } => {
            let voucher = Voucher {
                destination: *destination,
                value: *value,
                payload: payload.clone().into(),
            };
            rollup.emit_voucher(&voucher)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{Address, U256};
    use app_core::application::{WalletApp, WalletConfig};
    use trolley::{InputMetadata, RollupError};

    use super::*;

    struct MockRollup {
        inputs: VecDeque<Result<RollupRequest, RollupError>>,
        reports: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl MockRollup {
        fn with_inputs(
            inputs: Vec<Result<RollupRequest, RollupError>>,
        ) -> (Self, Arc<Mutex<Vec<Vec<u8>>>>) {
            let reports = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    inputs: VecDeque::from(inputs),
                    reports: Arc::clone(&reports),
                },
                reports,
            )
        }
    }

    impl Rollup for MockRollup {
        fn next_input(&mut self) -> trolley::RollupResult<RollupRequest> {
            self.inputs
                .pop_front()
                .unwrap_or(Err(RollupError::CmtCallFailed {
                    operation: "next_input",
                    code: -1,
                }))
        }

        fn revert(&mut self) -> ! {
            panic!("mock rollup revert is not used in this test");
        }

        fn gio(&mut self, _domain: u16, _id: &[u8]) -> trolley::RollupResult<(u16, Vec<u8>)> {
            unimplemented!("mock rollup gio is not used in this test");
        }

        fn emit_voucher(&mut self, _voucher: &types::Voucher) -> trolley::RollupResult<()> {
            unimplemented!("mock rollup emit_voucher is not used in this test");
        }

        fn emit_notice(&mut self, _notice: &types::Notice) -> trolley::RollupResult<()> {
            unimplemented!("mock rollup emit_notice is not used in this test");
        }

        fn emit_report(&mut self, report: &[u8]) -> trolley::RollupResult<()> {
            self.reports
                .lock()
                .expect("poisoned reports mutex")
                .push(report.to_vec());
            Ok(())
        }
    }

    fn metadata(sender: Address, block: u64) -> InputMetadata {
        InputMetadata {
            chain_id: U256::from(1_u64),
            app_contract: Address::ZERO,
            msg_sender: sender,
            block_number: U256::from(block),
            block_timestamp: U256::ZERO,
            prev_randao: U256::ZERO,
            index: U256::ZERO,
        }
    }

    #[test]
    fn run_scheduler_emits_report_for_invalid_batch_before_rollup_error() {
        let sequencer = SchedulerConfig::default().sequencer_address;
        let invalid_batch_input = RollupRequest::Advance {
            metadata: metadata(sequencer, 10),
            payload: vec![0xFF, 0xEE, 0xDD],
        };
        let terminal_err = Err(RollupError::CmtCallFailed {
            operation: "next_input",
            code: -22,
        });
        let (rollup, reports) =
            MockRollup::with_inputs(vec![Ok(invalid_batch_input), terminal_err]);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_scheduler_forever(
                rollup,
                WalletApp::new(WalletConfig::default()),
                SchedulerConfig::default(),
            )
        }));

        assert!(
            result.is_err(),
            "scheduler loop should panic on rollup error"
        );
        let reports = reports.lock().expect("poisoned reports mutex");
        assert!(
            reports
                .iter()
                .any(|report| report.as_slice() == b"scheduler dropped invalid batch"),
            "missing invalid batch report, got: {reports:?}"
        );
    }
}
