// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter: posts closed batches to L1 with at-least-once semantics.
//!
//! The batch index is used as the batch nonce (id). The scheduler checks that nonces are
//! strictly increasing and invalidates otherwise, so duplicates are deduplicated at the
//! scheduler level. See `worker` for the wake → read S → compare → submit → sleep loop.

mod batch_poster;
mod config;
mod worker;

pub use batch_poster::{
    BatchPoster, BatchPosterConfig, BatchPosterError, EthereumBatchPoster, TxHash,
};
pub use config::BatchSubmitterConfig;
pub use worker::{BatchSubmitter, BatchSubmitterError, TickOutcome};
