//! Integration-style tests kept inside the crate so raw worker launch APIs
//! can remain crate-private. Production consumers enter through `run_main`.

mod batch_submitter;
mod chain_id_validation;
mod common;
mod e2e_sequencer;
mod snapshot_endpoints;
mod ws_broadcaster;
