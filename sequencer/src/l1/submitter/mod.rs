// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter: posts closed batches to L1 with at-least-once semantics.
//!
//! Each valid closed batch has a structural nonce (`batches.nonce`, set at
//! creation time as `parent.nonce + 1`). The scheduler accepts a batch only at
//! its exact expected nonce and rejects mismatches *without consuming the
//! nonce* (the staleness "skip" is a separate path) — at-least-once submission
//! is safe precisely because duplicates and replays arrive at already-consumed
//! nonces and are rejected. See `worker` for the tick loop.

mod config;
mod poster;
mod worker;

pub use config::BatchSubmitterConfig;
pub use poster::{
    BatchPoster, BatchPosterConfig, BatchPosterError, EthereumBatchPoster, SubmitBatchesOutcome,
    TxHash,
};
pub use worker::{BatchSubmitter, BatchSubmitterError, SubmitterExit};
