// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter: posts closed batches to L1 at least once. Duplicates are safe
//! because the scheduler accepts a batch only at its exact expected nonce and
//! rejects others without consuming it (`docs/protocol/scheduler-semantics.md`).
//! The tick loop is in `worker`.

mod config;
mod poster;
mod worker;

pub use config::BatchSubmitterConfig;
pub(crate) use poster::BatchPoster;
pub use poster::{
    BatchPosterConfig, BatchPosterError, EthereumBatchPoster, SubmitBatchesOutcome, TxHash,
};
pub(crate) use worker::BatchSubmitter;
pub use worker::BatchSubmitterError;
