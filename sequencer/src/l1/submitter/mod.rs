// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter: posts closed batches to L1 with at-least-once semantics.
//!
//! Each valid closed batch has a structural nonce (`batches.nonce`, set at
//! creation time as `parent.nonce + 1`). The scheduler checks that nonces are
//! strictly increasing and skips otherwise, so duplicates are deduplicated at
//! the scheduler level. See `worker` for the tick loop.

mod config;
mod poster;
mod worker;

pub use config::BatchSubmitterConfig;
pub use poster::{BatchPoster, BatchPosterConfig, BatchPosterError, EthereumBatchPoster, TxHash};
pub use worker::{BatchSubmitter, BatchSubmitterError, SubmitterExit};
