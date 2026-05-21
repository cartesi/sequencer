// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Safe app-state reconstruction for read-only egress endpoints.

use alloy_primitives::Address;
use thiserror::Error;

use crate::storage::Storage;
use sequencer_core::application::{AppError, Application};
use sequencer_core::l2_tx::SequencedL2Tx;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    pub safe_block: u64,
    pub state: String,
}

#[derive(Debug, Error)]
pub enum StateSnapshotError {
    #[error("safe L1 head has not been observed yet")]
    SafeHeadUnavailable,
    #[error("storage error: {source}")]
    Storage {
        #[from]
        source: rusqlite::Error,
    },
    #[error("storage open error: {source}")]
    OpenStorage {
        #[from]
        source: crate::storage::StorageOpenError,
    },
    #[error("application error: {source}")]
    Application {
        #[from]
        source: AppError,
    },
}

#[derive(Clone)]
pub struct StateSnapshotter<A> {
    db_path: String,
    genesis_app: A,
    batch_submitter_address: Address,
}

pub trait StateSnapshotProvider: Send + Sync {
    fn snapshot(&self) -> Result<StateSnapshot, StateSnapshotError>;
}

impl<A> StateSnapshotter<A>
where
    A: Application + Clone,
{
    pub fn new(db_path: String, genesis_app: A, batch_submitter_address: Address) -> Self {
        Self {
            db_path,
            genesis_app,
            batch_submitter_address,
        }
    }

    fn build_snapshot(&self) -> Result<StateSnapshot, StateSnapshotError> {
        let mut storage = Storage::open_read_only(self.db_path.as_str())?;
        let safe_block = storage
            .current_safe_block()?
            .ok_or(StateSnapshotError::SafeHeadUnavailable)?;
        let txs = storage.safe_accepted_l2_txs(self.batch_submitter_address)?;

        let mut app = self.genesis_app.clone();
        replay_txs(&mut app, txs)?;
        let state = app.export_state()?;

        Ok(StateSnapshot { safe_block, state })
    }
}

impl<A> StateSnapshotProvider for StateSnapshotter<A>
where
    A: Application + Clone + Sync,
{
    fn snapshot(&self) -> Result<StateSnapshot, StateSnapshotError> {
        self.build_snapshot()
    }
}

fn replay_txs<A>(app: &mut A, txs: Vec<SequencedL2Tx>) -> Result<(), AppError>
where
    A: Application,
{
    for tx in txs {
        match tx {
            SequencedL2Tx::UserOp(user_op) => {
                app.execute_valid_user_op(&user_op)?;
            }
            SequencedL2Tx::Direct(input) => {
                app.execute_direct_input(&input)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::SystemTime;

    use crate::ingress::inclusion_lane::PendingUserOp;
    use crate::storage::test_helpers::{default_protocol_timing, temp_db};
    use crate::storage::{SafeInputRange, Storage, StoredSafeInput};
    use alloy_primitives::Signature;
    use sequencer_core::application::{AppOutputs, InvalidReason};
    use sequencer_core::batch::Batch;
    use sequencer_core::l2_tx::{DirectInput, ValidUserOp};
    use sequencer_core::user_op::{SignedUserOp, UserOp};
    use tokio::sync::oneshot;

    #[derive(Clone, Default)]
    struct RecordingApp {
        events: Vec<String>,
    }

    impl Application for RecordingApp {
        const MAX_METHOD_PAYLOAD_BYTES: usize = 1024;

        fn current_user_nonce(&self, _sender: Address) -> u32 {
            0
        }

        fn current_user_balance(&self, _sender: Address) -> alloy_primitives::U256 {
            alloy_primitives::U256::ZERO
        }

        fn validate_user_op(
            &self,
            _sender: Address,
            _user_op: &UserOp,
            _current_fee: u16,
        ) -> Result<(), InvalidReason> {
            Ok(())
        }

        fn execute_valid_user_op(&mut self, user_op: &ValidUserOp) -> Result<AppOutputs, AppError> {
            self.events.push(format!(
                "user:{}:{}:{}",
                user_op.sender,
                user_op.fee,
                alloy_primitives::hex::encode(&user_op.data)
            ));
            Ok(Vec::new())
        }

        fn execute_direct_input(&mut self, input: &DirectInput) -> Result<AppOutputs, AppError> {
            self.events.push(format!(
                "direct:{}:{}:{}",
                input.sender,
                input.block_number,
                alloy_primitives::hex::encode(&input.payload)
            ));
            Ok(Vec::new())
        }

        fn export_state(&self) -> Result<String, AppError> {
            Ok(self.events.join("|"))
        }
    }

    fn empty_batch_payload(nonce: u64) -> Vec<u8> {
        ssz::Encode::as_ssz_bytes(&Batch {
            nonce,
            frames: Vec::new(),
        })
    }

    #[test]
    fn snapshot_replays_only_scheduler_accepted_batch_prefix() {
        let db = temp_db("state-snapshot-accepted-prefix");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let batch_submitter = Address::repeat_byte(0xfe);
        let direct_sender = Address::repeat_byte(0x11);

        storage
            .append_safe_inputs(
                10,
                &[
                    StoredSafeInput {
                        sender: direct_sender,
                        payload: vec![0xaa],
                        block_number: 10,
                    },
                    StoredSafeInput {
                        sender: batch_submitter,
                        payload: empty_batch_payload(0),
                        block_number: 10,
                    },
                ],
                batch_submitter,
                &default_protocol_timing(),
            )
            .expect("append safe inputs");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::new(0, 2))
            .expect("initialize open state");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close accepted batch");

        let snapshotter =
            StateSnapshotter::new(db.path.clone(), RecordingApp::default(), batch_submitter);
        let snapshot = snapshotter.snapshot().expect("snapshot");

        assert_eq!(snapshot.safe_block, 10);
        assert_eq!(snapshot.state, format!("direct:{direct_sender}:10:aa"));
    }

    #[test]
    fn snapshot_excludes_open_tip_user_op() {
        let db = temp_db("state-snapshot-excludes-tip");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let batch_submitter = Address::repeat_byte(0xfe);
        let sender = Address::repeat_byte(0x22);

        storage
            .append_safe_inputs(10, &[], batch_submitter, &default_protocol_timing())
            .expect("append safe head");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::new(0, 0))
            .expect("initialize open state");
        let (respond_to, _recv) = oneshot::channel();
        storage
            .append_user_ops_chunk(
                &mut head,
                &[PendingUserOp {
                    signed: SignedUserOp {
                        sender,
                        signature: Signature::test_signature(),
                        user_op: UserOp {
                            nonce: 0,
                            max_fee: u16::MAX,
                            data: vec![0x01].into(),
                        },
                    },
                    respond_to,
                    received_at: SystemTime::now(),
                }],
            )
            .expect("append tip user op");

        let snapshotter =
            StateSnapshotter::new(db.path.clone(), RecordingApp::default(), batch_submitter);
        let snapshot = snapshotter.snapshot().expect("snapshot");

        assert_eq!(snapshot.safe_block, 10);
        assert_eq!(snapshot.state, "");
    }

    #[test]
    fn snapshot_excludes_unaccepted_soft_batch() {
        let db = temp_db("state-snapshot-excludes-soft");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let batch_submitter = Address::repeat_byte(0xfe);
        let direct_sender = Address::repeat_byte(0x11);

        storage
            .append_safe_inputs(
                10,
                &[StoredSafeInput {
                    sender: direct_sender,
                    payload: vec![0xaa],
                    block_number: 10,
                }],
                batch_submitter,
                &default_protocol_timing(),
            )
            .expect("append safe input");
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::new(0, 1))
            .expect("initialize open state");
        storage
            .close_frame_and_batch(&mut head, 10)
            .expect("close unaccepted batch");

        let snapshotter =
            StateSnapshotter::new(db.path.clone(), RecordingApp::default(), batch_submitter);
        let snapshot = snapshotter.snapshot().expect("snapshot");

        assert_eq!(snapshot.safe_block, 10);
        assert_eq!(snapshot.state, "");
    }
}
