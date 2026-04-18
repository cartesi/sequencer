// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

pub mod paths;
pub mod proxy;
pub mod replay;
pub mod rollups;
pub mod sequencer;
mod util;
pub mod wallet;
pub mod ws;

pub type HarnessResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub use proxy::TcpProxy;
pub use replay::ReplayWalletApp;
pub use rollups::{DEVNET_CHAIN_ID, DevnetRollupsStack};
pub use sequencer::{
    BatchCounts, DEFAULT_DEVNET_SEQUENCER_BIN, DEFAULT_TEST_LOGS_DIR, ManagedSequencer,
    ManagedSequencerConfig, RespawnAttemptOutcome, RespawnPolicy, default_devnet_sequencer_config,
};
pub use wallet::{
    TestSigner, WalletL1Client, WalletL2Client, address_from_signing_key, sign_user_op_hex,
};
pub use ws::WsClient;
