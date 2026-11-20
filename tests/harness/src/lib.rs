// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

mod anvil_accounts;
pub mod paths;
pub mod proxy;
pub mod replay;
pub mod rollups;
pub mod sequencer;
mod util;
pub mod wallet;
pub mod ws;

pub type HarnessResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub use anvil_accounts::default_private_keys;
pub use proxy::TcpProxy;
pub use replay::ReplayWalletApp;
pub use rollups::{DEVNET_CHAIN_ID, DEVNET_SECONDS_PER_BLOCK, DevnetRollupsStack};
pub use sequencer::{
    BatchCounts, BatchNonceFrontier, DEFAULT_DEVNET_SEQUENCER_BIN, DEFAULT_TEST_LOGS_DIR,
    EXIT_TERMINAL, LandingHealth, ManagedSequencer, ManagedSequencerConfig, PublishedEra,
    RebuiltHistoryRows, RunStart, SetupOutcome, StackChildExit, UnacceptedBatch,
    batch_submitter_signer, default_c_wallet_sequencer_config, default_devnet_sequencer_config,
    devnet_sequencer_config_no_faketime, encode_batch, is_terminal_abort,
};
pub use wallet::{
    TestSigner, WalletL1Client, WalletL2Client, address_from_signing_key, sign_user_op_hex,
};
pub use ws::WsClient;
