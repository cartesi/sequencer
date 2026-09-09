// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The same wallet as `wallet-sequencer`, reached over the C seam instead of directly. This is
//! the model for what a C application author builds: depend on an engine, call the host.

use std::process::ExitCode;

// Load-bearing: nothing here calls the engine, but without the import the crate stays off the
// link line and the seam's symbols go unresolved.
use c_wallet_engine as _;

#[tokio::main]
async fn main() -> ExitCode {
    c_app_sequencer::run().await
}
