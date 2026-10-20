// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Wallet foreclosure accounts, published before every accepted guest yield.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};

use app_core::application::WalletApp;
use sequencer_core::application::AppError;
use sequencer_core::scheduler::Scheduler;

pub const START: u64 = 0x0090_0000_0000_0000;
pub const LOG2_SIZE: u32 = 26;
pub const ACCOUNT_BYTES: usize = 64;

pub struct ExitDrive {
    file: File,
    previous: Vec<[u8; ACCOUNT_BYTES]>,
}

impl ExitDrive {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        assert_eq!(
            file.seek(SeekFrom::End(0))?,
            1 << LOG2_SIZE,
            "exit drive size mismatch"
        );
        Ok(Self {
            file,
            previous: Vec::new(),
        })
    }

    pub fn publish(&mut self, scheduler: &Scheduler<WalletApp>) -> Result<(), AppError> {
        let accounts = scheduler
            .application()
            .exit_accounts(scheduler.pending_direct_payloads())?;
        assert!(
            accounts.len() <= (1 << LOG2_SIZE) / ACCOUNT_BYTES,
            "wallet exit capacity exceeded"
        );
        let records: Vec<_> = accounts
            .into_iter()
            .map(|(owner, balance)| {
                let mut record = [0; ACCOUNT_BYTES];
                record[12..32].copy_from_slice(owner.as_slice());
                record[32..].copy_from_slice(&balance.to_be_bytes::<32>());
                record
            })
            .collect();
        for index in 0..records.len().max(self.previous.len()) {
            let record = records.get(index).copied().unwrap_or([0; ACCOUNT_BYTES]);
            if self.previous.get(index) != Some(&record) {
                self.file
                    .seek(SeekFrom::Start((index * ACCOUNT_BYTES) as u64))?;
                self.file.write_all(&record)?;
            }
        }
        self.file.sync_all()?;
        self.previous = records;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use app_core::application::WalletConfig;
    use sequencer_core::batch::{Batch, Frame};
    use sequencer_core::scheduler::{SchedulerConfig, SchedulerInput};
    use ssz::Encode;

    #[test]
    fn queued_and_credited_deposits_have_the_same_full_width_exit() {
        let config = WalletConfig::devnet();
        let owner = Address::repeat_byte(1);
        let amount: U256 = (U256::from(1) << 128) + U256::from(37);
        let mut payload = config.supported_erc20_token.as_slice().to_vec();
        payload.extend(owner.as_slice());
        payload.extend(amount.to_be_bytes::<32>());
        let mut scheduler = Scheduler::new(
            WalletApp::new(config),
            SchedulerConfig::new(config.sequencer_address),
        );
        let domain = sequencer_core::build_input_domain(31337, Address::ZERO);
        scheduler
            .process_input(SchedulerInput {
                sender: config.erc20_portal_address,
                inclusion_block: 10,
                domain: domain.clone(),
                payload,
            })
            .unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(1 << LOG2_SIZE).unwrap();
        let mut drive = ExitDrive::open(file.path().to_str().unwrap()).unwrap();
        drive.publish(&scheduler).unwrap();
        let pending = drive.previous.clone();
        assert_eq!(pending.len(), 1);
        assert_eq!(&pending[0][12..32], owner.as_slice());
        assert_eq!(&pending[0][32..], &amount.to_be_bytes::<32>());
        assert_eq!(
            scheduler.application().current_user_balance(owner),
            U256::ZERO
        );
        scheduler
            .process_input(SchedulerInput {
                sender: config.sequencer_address,
                inclusion_block: 11,
                domain,
                payload: Batch {
                    nonce: 0,
                    frames: vec![Frame {
                        safe_block: 10,
                        fee_price: 0,
                        user_ops: vec![],
                    }],
                }
                .as_ssz_bytes(),
            })
            .unwrap();
        drive.publish(&scheduler).unwrap();
        assert_eq!(
            drive.previous, pending,
            "draining must neither drop nor double count custody"
        );
        assert_eq!(scheduler.queued_direct_len(), 0);
        assert_eq!(scheduler.application().current_user_balance(owner), amount);
    }
}
