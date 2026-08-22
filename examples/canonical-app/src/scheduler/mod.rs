// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Canonical-app harness around the scheduler library.
//!
//! The pure scheduler fold lives in [`sequencer_core::scheduler`]; this module
//! is the I/O shell that drives it inside the RISC-V app. It pulls inputs off
//! the `trolley` rollup, coerces host `U256` metadata into the scheduler's
//! `u64` domain, feeds them through [`Scheduler::process_input`], and emits the
//! app's notices/vouchers/reports back to the rollup.

use alloy_primitives::U256;
use sequencer_core::application::AppOutput;
use sequencer_core::application::Application;
use sequencer_core::scheduler::{
    InspectError, ProcessOutcome, Scheduler, SchedulerInput, input_domain,
};
use trolley::{Rollup, RollupRequest};
use types::{Notice, Voucher};

pub use sequencer_core::scheduler::{STATE_INSPECT_QUERY, SchedulerConfig};

pub fn run_scheduler_forever<R: Rollup, A: Application>(
    mut rollup: R,
    app: A,
    scheduler_config: SchedulerConfig,
) -> ! {
    let mut scheduler = Scheduler::new(app, scheduler_config);

    loop {
        match rollup.next_input() {
            Ok(RollupRequest::Advance { metadata, payload }) => {
                let inclusion_block = block_to_u64(metadata.block_number);
                let domain =
                    input_domain(chain_id_to_u64(metadata.chain_id), metadata.app_contract);

                let input = SchedulerInput {
                    sender: metadata.msg_sender,
                    inclusion_block,
                    domain,
                    payload,
                };

                let result = scheduler
                    .process_input(input)
                    .unwrap_or_else(|err| panic!("canonical application execution failed: {err}"));
                for output in &result.outputs {
                    emit_app_output(&mut rollup, output)
                        .unwrap_or_else(|err| panic!("scheduler failed to emit app output: {err}"));
                }
                if matches!(result.outcome, ProcessOutcome::BatchRejected(_)) {
                    rollup
                        .emit_report(b"scheduler dropped invalid batch")
                        .unwrap_or_else(|err| {
                            panic!("scheduler failed to emit invalid-batch report: {err}")
                        });
                }
            }
            Ok(RollupRequest::Inspect { payload }) => {
                // Inspect is a public, read-only query endpoint: an unknown query
                // (or a state-encode error) must not halt the guest. Emit a
                // structured error report and keep serving, as it did before.
                let report = match scheduler.inspect_state(&payload) {
                    Ok(bytes) => bytes,
                    Err(InspectError::UnsupportedQuery) => b"unsupported inspect query".to_vec(),
                    Err(InspectError::Application(reason)) => {
                        format!("inspect failed: {reason}").into_bytes()
                    }
                };
                rollup
                    .emit_report(&report)
                    .unwrap_or_else(|err| panic!("scheduler failed to emit inspect report: {err}"));
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

/// Coerce a host `U256` block number to the scheduler's `u64` domain. Solidity
/// exposes block numbers as `uint256`; a value that does not fit `u64` is a
/// malformed host input for this prototype. Host-side, so it stays in the
/// harness rather than the pure scheduler library.
fn block_to_u64(block: U256) -> u64 {
    u64::try_from(block).expect("block number does not fit u64")
}

/// Coerce a host `U256` chain id to `u64` (same rationale as [`block_to_u64`]).
fn chain_id_to_u64(chain_id: U256) -> u64 {
    u64::try_from(chain_id).expect("chain id does not fit u64")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{Address, U256};
    use app_core::application::{SEPOLIA_SEQUENCER_ADDRESS, WalletApp, WalletConfig};
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
    fn run_scheduler_emits_exported_state_for_state_inspect() {
        let inspect = RollupRequest::Inspect {
            payload: STATE_INSPECT_QUERY.to_vec(),
        };
        let terminal_err = Err(RollupError::CmtCallFailed {
            operation: "next_input",
            code: -22,
        });
        let (rollup, reports) = MockRollup::with_inputs(vec![Ok(inspect), terminal_err]);
        let expected = app_core::wallet_snapshot::encode(&WalletApp::new(WalletConfig::default()));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_scheduler_forever(
                rollup,
                WalletApp::new(WalletConfig::default()),
                SchedulerConfig::new(SEPOLIA_SEQUENCER_ADDRESS),
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
                .any(|report| report.as_slice() == expected.as_slice()),
            "missing state inspect report, got: {reports:?}"
        );
    }

    #[test]
    fn run_scheduler_reports_unsupported_inspect_query_without_panicking() {
        // A non-"state" inspect payload must produce a graceful report, not a
        // guest panic. The loop should survive the inspect and only panic when
        // it later hits the terminal rollup error.
        let inspect = RollupRequest::Inspect {
            payload: b"balances".to_vec(),
        };
        let terminal_err = Err(RollupError::CmtCallFailed {
            operation: "next_input",
            code: -22,
        });
        let (rollup, reports) = MockRollup::with_inputs(vec![Ok(inspect), terminal_err]);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_scheduler_forever(
                rollup,
                WalletApp::new(WalletConfig::default()),
                SchedulerConfig::new(SEPOLIA_SEQUENCER_ADDRESS),
            )
        }));

        assert!(
            result.is_err(),
            "scheduler loop should panic only on the terminal rollup error"
        );
        let reports = reports.lock().expect("poisoned reports mutex");
        assert!(
            reports
                .iter()
                .any(|report| report.as_slice() == b"unsupported inspect query"),
            "expected a graceful unsupported-query report, got: {reports:?}"
        );
    }

    #[test]
    fn run_scheduler_emits_report_for_invalid_batch_before_rollup_error() {
        let sequencer = SEPOLIA_SEQUENCER_ADDRESS;
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
                SchedulerConfig::new(SEPOLIA_SEQUENCER_ADDRESS),
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
