// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The generic host binary for an application supplying its engine as an archive, which needs no
//! code of its own. Built without one it has no engine to run and says so; nothing in the host is
//! reachable from that arm, which is what lets the workspace build it with no symbols to resolve.

use std::process::ExitCode;

#[cfg(external_engine)]
#[tokio::main]
async fn main() -> ExitCode {
    c_app_sequencer::run().await
}

#[cfg(not(external_engine))]
fn main() -> ExitCode {
    eprintln!(
        "built without an engine, so there is no application to run. Set \
         APPLICATION_ENGINE_LIB, APPLICATION_ENGINE_HEADER and \
         APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT, then build again."
    );
    ExitCode::FAILURE
}
