use crate::types::{InputBuilder, OutputsForInput};

use cartesi_machine::{
    cartesi_machine_sys::CM_HTIF_CONSOLE_CMD_GETCHAR,
    config::{machine, runtime::RuntimeConfig},
    constants::{break_reason, cmio::commands},
    types::cmio,
};
use std::{
    ops::ControlFlow,
    path::PathBuf,
    time::{Duration, Instant},
};
use thiserror::Error;
use types::alloy_primitives::{Address, U256};

type Report = Vec<u8>;

const DEFAULT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(90);
const RUN_MCYCLE_INCREMENT: u64 = 1 << 28;

#[derive(Debug, Error)]
pub enum TestsiMachineError {
    #[error(transparent)]
    Machine(#[from] cartesi_machine::error::MachineError),
    #[error("machine halted unexpectedly at mcycle={mcycle}")]
    Halted { mcycle: u64 },
    #[error("machine reached its per-input mcycle limit (mcycle overflow) at mcycle={mcycle}")]
    McycleOverflow { mcycle: u64 },
    #[error("application raised tx exception: {message}")]
    TxException { message: String },
    #[error(
        "application rejected the input (manual rx_rejected); testsi does not revert \
         rejected inputs, so this machine cannot accept further inputs"
    )]
    InputRejected,
    #[error("machine is not awaiting an input (expected a manual rx_accepted yield): {state}")]
    NotAwaitingInput { state: String },
    #[error("machine execution timed out after {elapsed:?} (timeout {timeout:?})")]
    ExecutionTimeout {
        elapsed: Duration,
        timeout: Duration,
    },
}

type Result<T> = std::result::Result<T, TestsiMachineError>;

pub struct MachineBuilder {
    cartesi_machine_path: PathBuf,
    chain_id: usize,
    dapp_address: Address,
    input_index: usize,
    no_console_putchar: bool,
    execution_timeout: Duration,
}

impl MachineBuilder {
    pub fn load_from<T: Into<PathBuf>>(path: T) -> MachineBuilder {
        Self {
            cartesi_machine_path: path.into(),
            chain_id: 1,
            dapp_address: Address::ZERO,
            input_index: 0,
            no_console_putchar: true,
            execution_timeout: DEFAULT_EXECUTION_TIMEOUT,
        }
    }

    pub fn at_chain(mut self, chain_id: usize) -> MachineBuilder {
        self.chain_id = chain_id;
        self
    }

    pub fn deployed_at(mut self, dapp_address: Address) -> MachineBuilder {
        self.dapp_address = dapp_address;
        self
    }

    pub fn with_input_count(mut self, input_index: usize) -> MachineBuilder {
        self.input_index = input_index;
        self
    }

    pub fn no_console_putchar(mut self, no_console_putchar: bool) -> MachineBuilder {
        self.no_console_putchar = no_console_putchar;
        self
    }

    pub fn with_execution_timeout(mut self, execution_timeout: Duration) -> MachineBuilder {
        self.execution_timeout = execution_timeout;
        self
    }

    pub fn try_build(self) -> Result<Machine> {
        Machine::try_new(self)
    }
}

pub struct Machine {
    cartesi_machine: cartesi_machine::machine::Machine,
    builder: MachineBuilder,
}

impl Machine {
    pub fn try_new(builder: MachineBuilder) -> Result<Self> {
        // v0.21 replaced `htif.no_console_putchar` with a console output
        // destination; the default writes guest console output to stdout.
        let runtime_config = if builder.no_console_putchar {
            RuntimeConfig::quiet_console()
        } else {
            RuntimeConfig::default()
        };

        // Instantiate Machine
        let cartesi_machine = {
            let mut cm =
                cartesi_machine::Machine::load(&builder.cartesi_machine_path, &runtime_config)?;
            let c = cm.initial_config()?;
            sanity_check_cm_config(&c);
            cm
        };

        Ok(Self {
            cartesi_machine,
            builder,
        })
    }

    pub fn advance_state(
        &mut self,
        input: InputBuilder,
    ) -> Result<(OutputsForInput, Vec<Vec<u8>>)> {
        let encoded_input = input.encode(
            self.builder.chain_id,
            current_input_index(self.builder.input_index),
            self.builder.dapp_address,
        );

        // v0.21 accepts an advance-state response only at a manual
        // rx_accepted yield, and requires the root hash of that pre-input
        // state (the state a rejected input reverts to). testsi never
        // reverts: a rejection is reported as `InputRejected`.
        expect_awaiting_input(&mut self.cartesi_machine)?;
        let revert_root_hash = self.cartesi_machine.root_hash()?;
        self.cartesi_machine.send_cmio_response(
            cmio::CmioResponseReason::Advance,
            &encoded_input,
            Some(&revert_root_hash),
        )?;
        advance_input_index(&mut self.builder.input_index);

        let mut outputs = OutputsForInput::default();
        let mut reports = Vec::new();
        let execution_started_at = Instant::now();

        while let ControlFlow::Continue(_) = run_machine_increment(
            &mut self.cartesi_machine,
            &mut outputs,
            &mut reports,
            execution_started_at,
            self.builder.execution_timeout,
        )? {}

        Ok((outputs, reports))
    }

    pub fn inspect(&self) -> Result<()> {
        panic!("testsi::Machine does not support inspect() yet")
    }
}

fn run_machine_increment(
    cartesi_machine: &mut cartesi_machine::machine::Machine,
    outputs: &mut OutputsForInput,
    reports: &mut Vec<Report>,
    execution_started_at: Instant,
    execution_timeout: Duration,
) -> Result<ControlFlow<()>> {
    // `run` takes an absolute mcycle target and rejects one already in the past.
    let mcycle_end = cartesi_machine
        .mcycle()?
        .saturating_add(RUN_MCYCLE_INCREMENT);
    let break_reason = cartesi_machine.run(mcycle_end)?;

    let control_flow = match break_reason {
        break_reason::HALTED => {
            return Err(TestsiMachineError::Halted {
                mcycle: cartesi_machine.mcycle()?,
            });
        }
        // Fixed point: the input exhausted its mcycle budget (imcyclemax).
        break_reason::MCYCLE_OVERFLOW => {
            return Err(TestsiMachineError::McycleOverflow {
                mcycle: cartesi_machine.mcycle()?,
            });
        }
        break_reason::REACHED_TARGET_MCYCLE => {
            on_reached_target_mcycle(execution_started_at, execution_timeout)?
        }

        break_reason::YIELDED_MANUALLY | break_reason::YIELDED_AUTOMATICALLY => {
            handle_yield(cartesi_machine, outputs, reports)?
        }

        break_reason::YIELDED_SOFTLY => {
            panic!("testsi harness does not support softly yielded execution yet")
        }

        _ => {
            panic!("machine returned invalid break reason {break_reason}")
        }
    };

    Ok(control_flow)
}

fn on_reached_target_mcycle(
    execution_started_at: Instant,
    execution_timeout: Duration,
) -> Result<ControlFlow<()>> {
    let elapsed = execution_started_at.elapsed();
    if elapsed >= execution_timeout {
        return Err(TestsiMachineError::ExecutionTimeout {
            elapsed,
            timeout: execution_timeout,
        });
    }
    Ok(ControlFlow::Continue(()))
}

fn handle_yield(
    cartesi_machine: &mut cartesi_machine::machine::Machine,
    outputs: &mut OutputsForInput,
    reports: &mut Vec<Report>,
) -> Result<ControlFlow<()>> {
    let request = cartesi_machine.receive_cmio_request()?;

    Ok(match request {
        // Manual yield
        cmio::CmioRequest::Manual(cmio::ManualReason::RxAccepted {
            output_hashes_root_hash: _,
        }) => ControlFlow::Break(()),

        cmio::CmioRequest::Manual(cmio::ManualReason::RxRejected) => {
            return Err(TestsiMachineError::InputRejected);
        }

        cmio::CmioRequest::Manual(cmio::ManualReason::TxException { message }) => {
            return Err(TestsiMachineError::TxException { message });
        }

        cmio::CmioRequest::Manual(cmio::ManualReason::GIO { .. }) => {
            panic!("unexpected GIO request")
        }

        // Automatic yield
        cmio::CmioRequest::Automatic(cmio::AutomaticReason::Progress { mille_progress: _ }) => {
            ControlFlow::Continue(())
        }
        cmio::CmioRequest::Automatic(cmio::AutomaticReason::TxOutput { data }) => {
            outputs.push_encoded(&data);
            ControlFlow::Continue(())
        }
        cmio::CmioRequest::Automatic(cmio::AutomaticReason::TxReport { data }) => {
            reports.push(data);
            ControlFlow::Continue(())
        }
    })
}

/// Fails unless the machine sits at the manual rx_accepted yield that v0.21
/// requires before an advance-state response.
fn expect_awaiting_input(cartesi_machine: &mut cartesi_machine::machine::Machine) -> Result<()> {
    if !cartesi_machine.iflags_y()? {
        let state = if cartesi_machine.iflags_h()? {
            "machine is halted".to_owned()
        } else {
            "machine is not at a manual yield".to_owned()
        };
        return Err(TestsiMachineError::NotAwaitingInput { state });
    }
    match cartesi_machine.receive_cmio_request()? {
        cmio::CmioRequest::Manual(cmio::ManualReason::RxAccepted { .. }) => Ok(()),
        other => Err(TestsiMachineError::NotAwaitingInput {
            state: format!("{other:?}"),
        }),
    }
}

fn sanity_check_cm_config(config: &machine::MachineConfig) {
    // v0.21 keeps the HTIF feature switches as bitmasks in the iyield and
    // iconsole CSRs, indexed by HTIF command.
    let htif = &config.processor.registers.htif;
    if htif.iyield & (1 << commands::YIELD_MANUAL) == 0 {
        eprintln!("warning: machine config has manual yields disabled (htif.iyield)");
    }
    if htif.iyield & (1 << commands::YIELD_AUTOMATIC) == 0 {
        eprintln!("warning: machine config has automatic yields disabled (htif.iyield)");
    }
    if htif.iconsole & (1 << CM_HTIF_CONSOLE_CMD_GETCHAR) != 0 {
        eprintln!("warning: machine config has console getchar enabled (htif.iconsole)");
    }

    check_cmio_memory_range_config(&config.cmio.tx_buffer, "tx_buffer");
    check_cmio_memory_range_config(&config.cmio.rx_buffer, "rx_buffer");
}

fn check_cmio_memory_range_config(range: &machine::CmioBufferConfig, name: &str) {
    assert!(
        !range.backing_store.shared,
        "cmio range {} cannot be shared",
        name
    );
}

fn current_input_index(input_index: usize) -> U256 {
    U256::from(input_index)
}

fn advance_input_index(input_index: &mut usize) {
    *input_index = input_index
        .checked_add(1)
        .expect("testsi input index overflow");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reached_target_mcycle_continues_before_timeout() {
        let started_at = Instant::now();
        let timeout = Duration::from_secs(5);

        let result = on_reached_target_mcycle(started_at, timeout).expect("must continue");
        assert!(matches!(result, ControlFlow::Continue(())));
    }

    #[test]
    fn reached_target_mcycle_returns_timeout_error_after_deadline() {
        let timeout = Duration::from_millis(1);
        let started_at = Instant::now() - Duration::from_secs(1);

        let err = on_reached_target_mcycle(started_at, timeout).expect_err("must time out");
        match err {
            TestsiMachineError::ExecutionTimeout { elapsed, timeout } => {
                assert!(elapsed >= timeout);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn input_index_helpers_are_monotonic() {
        let mut input_index = 7;

        assert_eq!(current_input_index(input_index), U256::from(7_u64));
        advance_input_index(&mut input_index);
        assert_eq!(current_input_index(input_index), U256::from(8_u64));
    }
}

// fn get_yield(machine: &cartesi_machine::Machine) -> Result<(isize, u32, u64)> {
//     let cmd = machine.read_htif_tohost_cmd()? as isize;
//     let data = machine.read_htif_tohost_data()?;
//
//     let reason = data >> 32;
//     let m16 = (1 << 16) - 1;
//     let reason = reason & m16;
//     let m32 = (1 << 32) - 1;
//     let length = data & m32;
//
//     Ok((cmd, reason as u32, length))
// }
