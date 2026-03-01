// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod db;
mod sql;

use std::time::SystemTime;
use thiserror::Error;

pub use db::Storage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedDirectInput {
    pub index: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum StorageOpenError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Migration(#[from] rusqlite_migration::Error),
}

#[derive(Debug, Clone, Copy)]
pub struct WriteHead {
    pub batch_index: u64,
    pub batch_created_at: SystemTime,
    // Sequencer-chosen fee committed for this open frame.
    pub frame_fee: u64,
    pub batch_user_op_count: u64,
    pub frame_in_batch: u32,
}

impl WriteHead {
    pub fn increment_batch_user_op_count(&mut self, count: usize) {
        self.batch_user_op_count = self.batch_user_op_count.saturating_add(count as u64);
    }

    pub fn advance_frame(&mut self, frame_fee: u64) {
        self.frame_in_batch = self.frame_in_batch.saturating_add(1);
        self.frame_fee = frame_fee;
    }

    pub fn move_to_next_batch(
        &mut self,
        batch_index: u64,
        batch_created_at: SystemTime,
        frame_fee: u64,
    ) {
        self.batch_index = batch_index;
        self.batch_created_at = batch_created_at;
        self.frame_fee = frame_fee;
        self.batch_user_op_count = 0;
        self.frame_in_batch = 0;
    }
}
