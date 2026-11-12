// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Step 1 of setup: the base it rebuilds from. A stored machine is read
//! through its export, and the sequencer's application is built from the
//! machine's state bytes; genesis is the empty scheduler and the host's
//! genesis application.

use std::path::{Path, PathBuf};

use sequencer_core::application::{AppError, Application};
use sequencer_core::history::L2TxCount;
use sequencer_core::scheduler::{Scheduler, SchedulerConfig, SchedulerExport};

use crate::commands::error::{CommandError, SetupError};
use crate::machine::MachineExportError;

/// How setup reads a stored machine: [`crate::machine::read_machine_export`]
/// in production. A test hands in an export and state bytes through it, so
/// setup's own code and tests never need an emulator.
pub(crate) type ReadMachineExport =
    Box<dyn FnOnce(&Path, &Path) -> Result<SchedulerExport, MachineExportError> + Send>;

pub(crate) fn read_stored_machine() -> ReadMachineExport {
    Box::new(crate::machine::read_machine_export)
}

/// The point setup rebuilds from: the machine's export, and the application
/// at the export's L2 tx count.
pub(crate) struct Base<A> {
    pub(crate) export: SchedulerExport,
    pub(crate) app: A,
}

/// Read the base: the machine at `machine` through `read_export`, its state
/// bytes written under `scratch`, or genesis without one. The genesis factory
/// runs only for a genesis base.
pub(crate) async fn load_base<A, F>(
    machine: Option<&Path>,
    genesis_app: F,
    read_export: ReadMachineExport,
    scheduler_config: SchedulerConfig,
    scratch: &Path,
) -> Result<Base<A>, CommandError>
where
    A: Application + 'static,
    F: FnOnce() -> Result<A, AppError>,
{
    let Some(machine) = machine else {
        let app = genesis_app()?;
        if app.l2_tx_count() != L2TxCount::ZERO {
            return Err(CommandError::AppBootstrap(AppError::Internal {
                reason: format!(
                    "a genesis application must start at l2_tx_count = 0, got {}",
                    app.l2_tx_count().get()
                ),
            }));
        }
        let export = SchedulerExport::new(
            Scheduler::genesis(scheduler_config).state(),
            L2TxCount::ZERO,
        );
        return Ok(Base { export, app });
    };

    // The emulator blocks and its machine handle is not `Send`.
    let machine_dir = machine.to_path_buf();
    let state_file = scratch.join("base-state");
    let export = {
        let (dir, file) = (machine_dir.clone(), state_file.clone());
        tokio::task::spawn_blocking(move || read_export(&dir, &file))
            .await
            .map_err(|join| {
                if join.is_panic() {
                    std::panic::resume_unwind(join.into_panic());
                }
                CommandError::Io(std::io::Error::other(format!(
                    "reading the base machine was cancelled: {join}"
                )))
            })?
            .map_err(|source| SetupError::BaseMachine {
                path: machine_dir.clone(),
                source,
            })?
    };
    let app = application_from_state::<A>(&machine_dir, &state_file, &export)?;
    Ok(Base { export, app })
}

/// The application at the export's count, held to it: an instance at another
/// count would number the rebuilt history wrongly.
fn application_from_state<A: Application>(
    machine_dir: &Path,
    state_file: &Path,
    export: &SchedulerExport,
) -> Result<A, SetupError> {
    let count = L2TxCount::new(export.l2_tx_count);
    let base_application = |source| SetupError::BaseApplication {
        path: PathBuf::from(machine_dir),
        source,
    };
    let app = A::from_canonical_state(state_file, count).map_err(base_application)?;
    if app.l2_tx_count() != count {
        return Err(base_application(AppError::Internal {
            reason: format!(
                "the application built from the machine's state is at L2 tx count {}, \
                 but the machine reports {}",
                app.l2_tx_count().get(),
                export.l2_tx_count
            ),
        }));
    }
    Ok(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use app_core::application::WalletApp;
    use sequencer_core::application::{Admission, AppOutputs, CanonicalState, SchedulerLimits};
    use sequencer_core::l2_tx::{Direct, UserOpTx};
    use sequencer_core::user_op::UserOp;

    /// Builds itself from the count its state bytes hold, ignoring the one it
    /// is handed: setup must hold it to the export's count itself.
    struct IgnoresCount(L2TxCount);

    impl Application for IgnoresCount {
        fn user_nonce(&self, _sender: Address) -> Result<u32, AppError> {
            unreachable!("setup's base never executes")
        }
        fn apply_user_op(&mut self, _op: &UserOpTx) -> Result<AppOutputs, AppError> {
            unreachable!("setup's base never executes")
        }
        fn apply_direct(&mut self, _direct: &Direct) -> Result<AppOutputs, AppError> {
            unreachable!("setup's base never executes")
        }
        fn l2_tx_count(&self) -> L2TxCount {
            self.0
        }
        fn max_method_payload_bytes() -> usize {
            0
        }
        fn scheduler_limits() -> SchedulerLimits {
            app_core::application::WALLET_SCHEDULER_LIMITS
        }
        fn admit_user_op(
            &self,
            _sender: Address,
            _op: &UserOp,
            _fee: u16,
        ) -> Result<Admission, AppError> {
            unreachable!("setup never admits")
        }
        fn from_dump(_prefix: &Path) -> Result<Self, AppError> {
            unreachable!("setup's base is no dump")
        }
        fn create_dump(&mut self, _prefix: &Path) -> Result<(), AppError> {
            unreachable!("setup's base writes no dump")
        }
        fn state_file_in_dump(prefix: &Path) -> PathBuf {
            prefix.to_path_buf()
        }
        fn from_canonical_state(state_file: &Path, _count: L2TxCount) -> Result<Self, AppError> {
            let bytes: [u8; 8] =
                std::fs::read(state_file)?
                    .try_into()
                    .map_err(|_| AppError::Internal {
                        reason: "eight bytes".to_owned(),
                    })?;
            Ok(Self(L2TxCount::new(u64::from_le_bytes(bytes))))
        }
    }

    fn export_at(l2_tx_count: u64) -> SchedulerExport {
        SchedulerExport {
            l2_tx_count,
            ..SchedulerExport::new(
                Scheduler::genesis(SchedulerConfig::for_application::<WalletApp>(Address::ZERO))
                    .state(),
                L2TxCount::ZERO,
            )
        }
    }

    /// A machine reader that writes `state` and answers `export`.
    fn machine(state: Vec<u8>, export: SchedulerExport) -> ReadMachineExport {
        Box::new(move |_, state_file| {
            std::fs::write(state_file, &state).unwrap();
            Ok(export)
        })
    }

    async fn refusal<A: Application + 'static>(read_export: ReadMachineExport) -> SetupError {
        let scratch = tempfile::tempdir().unwrap();
        let error = load_base::<A, _>(
            Some(Path::new("/machines/base")),
            || -> Result<A, AppError> { unreachable!("a machine base needs no genesis") },
            read_export,
            SchedulerConfig::for_application::<WalletApp>(Address::ZERO),
            scratch.path(),
        )
        .await
        .err()
        .expect("the base is refused");
        match error {
            CommandError::Bootstrap(crate::commands::BootstrapError::Setup(error)) => *error,
            other => panic!("expected a setup refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failed_machine_read_names_the_machine() {
        let error = refusal::<WalletApp>(Box::new(|_, state_file| {
            Err(MachineExportError::Io {
                path: state_file.to_path_buf(),
                source: std::io::Error::other("disk full"),
            })
        }))
        .await;
        assert!(
            matches!(
                &error,
                SetupError::BaseMachine { path, source: MachineExportError::Io { .. } }
                    if path == Path::new("/machines/base")
            ),
            "{error:?}"
        );
    }

    /// The count the built application reports must be the export's, whether
    /// or not the application checks it itself.
    #[tokio::test]
    async fn an_application_at_another_count_than_the_export_is_refused() {
        let error =
            refusal::<IgnoresCount>(machine(7_u64.to_le_bytes().to_vec(), export_at(8))).await;
        assert!(
            matches!(
                &error,
                SetupError::BaseApplication { path, source: AppError::Internal { reason } }
                    if path == Path::new("/machines/base") && reason.contains("machine reports 8")
            ),
            "{error:?}"
        );

        let state = WalletApp::default().canonical_snapshot_bytes().unwrap();
        let error = refusal::<WalletApp>(machine(state, export_at(1))).await;
        assert!(
            matches!(error, SetupError::BaseApplication { .. }),
            "{error:?}"
        );
    }
}
