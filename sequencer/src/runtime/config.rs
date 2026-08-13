// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! CLI configuration for the three subcommands (`setup`, `run`,
//! `flush-mempool`).
//!
//! The phase split: `setup` is L1-read-only and establishes the
//! timeless deployment identity + initial sync + genesis snapshot; it takes
//! the batch-submitter **address** but never the signing key. `run` boots
//! from an already-set-up DB, reads identity from the DB (so chain id / app
//! address are not CLI args here), and keeps the signing **key** because it
//! submits. `flush-mempool` is a keyed operator tool that settles the wallet
//! nonce on demand.
//!
//! Shared, drift-prone arg groups (`TimingArgs`, `KeyArgs`) are flattened into
//! the per-subcommand configs so defaults and env-var names stay consistent.

use alloy_primitives::Address;
use clap::{ArgGroup, Args};
use sequencer_core::protocol::{ProtocolTiming, ProtocolTimingError};

const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:3000";
const DEFAULT_DATA_DIR: &str = "sequencer-data";
const DB_FILENAME: &str = "sequencer.db";

/// Shared L1 / InputBox configuration used by both the input reader and the batch submitter.
///
/// Built once at startup from the pinned deployment identity plus the runtime
/// `RunConfig`, so RPC URL, InputBox address, and app address are defined in a
/// single place and not duplicated across component configs.
#[derive(Debug, Clone)]
pub struct L1Config {
    pub eth_rpc_url: String,
    pub input_box_address: Address,
    pub app_address: Address,
    pub batch_submitter_private_key: String,
    pub batch_submitter_address: Address,
    /// The pinned deployment chain id. Carried here so keyed-write paths (e.g.
    /// the preemptive-recovery flush) can re-confirm the RPC's chain id right
    /// before signing via [`crate::l1::provider::create_verified_signer_provider`].
    pub chain_id: u64,
    /// Opt into plaintext (`http://`) RPC against a non-loopback host — a
    /// trusted private network (Docker/K8s service, private-VPC IP). Off by
    /// default: the provider layer refuses remote plaintext otherwise. See
    /// [`crate::l1::provider`].
    pub allow_insecure_rpc: bool,
}

/// Full path to the SQLite database file inside `data_dir`.
pub fn db_path_in(data_dir: &str) -> String {
    std::path::Path::new(data_dir)
        .join(DB_FILENAME)
        .to_string_lossy()
        .into_owned()
}

/// Protocol-timing tuning knobs shared by `setup` and `run`. `setup` needs a
/// valid `ProtocolTiming` for the initial-sync writes (and validating it here
/// fails fast at setup, not just at run).
#[derive(Debug, Clone, Args)]
pub struct TimingArgs {
    /// Blocks before MAX_WAIT_BLOCKS to trigger preemptive recovery.
    /// The danger threshold is MAX_WAIT_BLOCKS minus this margin.
    /// Must be less than MAX_WAIT_BLOCKS (validated at startup).
    ///
    /// Default 300 (~1h at 12s/block) is sized to give operators meaningful
    /// runway to investigate before the system gives up on the current
    /// batches — see `docs/recovery/README.md` "Step 1: Danger threshold".
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_PREEMPTIVE_MARGIN_BLOCKS",
        default_value = "300"
    )]
    pub preemptive_margin_blocks: u64,

    /// Blocks of safe-head age after which the L1 read view is considered too
    /// stale to trust. Independent of the preemptive margin. Must be strictly
    /// less than the danger threshold (validated at startup).
    ///
    /// Default 600 (~2h at 12s/block).
    #[arg(long, env = "CARTESI_SEQUENCER_L1_READ_STALE_AFTER_BLOCKS", default_value = "600", value_parser = clap::value_parser!(u64).range(1..))]
    pub l1_read_stale_after_blocks: u64,

    /// Assumed L1 block time in seconds. Used to estimate block progression
    /// from wall-clock time when the L1 provider is unreachable.
    #[arg(long, env = "CARTESI_SEQUENCER_SECONDS_PER_BLOCK", default_value = "12", value_parser = clap::value_parser!(u64).range(1..))]
    pub seconds_per_block: u64,
}

impl TimingArgs {
    /// Build a validated [`ProtocolTiming`]. Pure derivation — no I/O.
    /// `max_wait_blocks` is the shared scheduler constant; the rest are the
    /// operator-tunable CLI args.
    pub fn protocol_timing(&self) -> Result<ProtocolTiming, ProtocolTimingError> {
        ProtocolTiming::try_new(
            sequencer_core::MAX_WAIT_BLOCKS,
            self.preemptive_margin_blocks,
            self.l1_read_stale_after_blocks,
            self.seconds_per_block,
        )
    }
}

/// L1 fee-oracle source selection. A fixed logarithmic price is intended for
/// local development; public chains use a validated Uniswap V3 TWAP source.
#[derive(Debug, Clone, Args)]
pub struct FeeOracleArgs {
    /// Fixed logarithmic gas price for local/Anvil deployments.
    #[arg(long, env = "CARTESI_SEQUENCER_FEE_ORACLE_FIXED_LOG_GAS_PRICE")]
    pub fixed_log_gas_price: Option<u16>,
    #[arg(long, env = "CARTESI_SEQUENCER_FEE_TOKEN_ADDRESS", value_parser = parse_address)]
    pub fee_token_address: Option<Address>,
    #[arg(long, env = "CARTESI_SEQUENCER_WETH_ADDRESS", value_parser = parse_address)]
    pub weth_address: Option<Address>,
    #[arg(long, env = "CARTESI_SEQUENCER_UNISWAP_V3_POOL", value_parser = parse_address)]
    pub uniswap_v3_pool: Option<Address>,
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_FEE_ORACLE_TWAP_WINDOW_SECS",
        default_value_t = crate::l1::fee_oracle::UniswapConfig::DEFAULT_TWAP_WINDOW_SECS
    )]
    pub twap_window_secs: u32,
}

/// Runtime-only fee-oracle tuning. The source itself is setup-pinned.
#[derive(Debug, Clone, Args)]
pub struct FeeOraclePollArgs {
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_FEE_ORACLE_POLL_INTERVAL_MS",
        default_value = "12000",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub poll_interval_ms: u64,
}

/// Resolved fee-oracle source with all chain-specific defaults filled in.
#[derive(Debug, Clone)]
pub enum FeeOracleMode {
    Fixed { log_gas_price: u16 },
    Uniswap(crate::l1::fee_oracle::UniswapConfig),
}

impl FeeOracleArgs {
    /// Resolve explicit overrides, public-chain presets, or the zero-price
    /// local-development default. This is pure so invalid combinations fail
    /// before the inclusion lane can sample a fee.
    pub fn resolve(&self, chain_id: u64) -> Result<FeeOracleMode, String> {
        let has_uniswap_override = self.fee_token_address.is_some()
            || self.weth_address.is_some()
            || self.uniswap_v3_pool.is_some();
        if let Some(log_gas_price) = self.fixed_log_gas_price {
            if has_uniswap_override {
                return Err(
                    "fixed fee oracle cannot be combined with Uniswap address overrides"
                        .to_string(),
                );
            }
            return Ok(FeeOracleMode::Fixed { log_gas_price });
        }
        if has_uniswap_override {
            let (fee_token, weth, pool) = match (
                self.fee_token_address,
                self.weth_address,
                self.uniswap_v3_pool,
            ) {
                (Some(fee_token), Some(weth), Some(pool)) => (fee_token, weth, pool),
                _ => {
                    return Err(
                        "Uniswap fee-oracle overrides require fee token address, WETH address, and Uniswap V3 pool"
                            .to_string(),
                    );
                }
            };
            return Ok(FeeOracleMode::Uniswap(
                crate::l1::fee_oracle::UniswapConfig {
                    chain_id,
                    weth,
                    fee_token,
                    pool,
                    twap_window_secs: self.twap_window_secs,
                },
            ));
        }
        let preset = match chain_id {
            1 => Some((
                crate::l1::fee_oracle::uniswap::MAINNET_WETH,
                crate::l1::fee_oracle::uniswap::MAINNET_USDC,
                crate::l1::fee_oracle::uniswap::MAINNET_USDC_WETH_005_POOL,
            )),
            11_155_111 => Some((
                crate::l1::fee_oracle::uniswap::SEPOLIA_WETH,
                crate::l1::fee_oracle::uniswap::SEPOLIA_USDC,
                crate::l1::fee_oracle::uniswap::SEPOLIA_USDC_WETH_005_POOL,
            )),
            _ => None,
        };
        Ok(match preset {
            Some((weth, fee_token, pool)) => {
                FeeOracleMode::Uniswap(crate::l1::fee_oracle::UniswapConfig {
                    chain_id,
                    weth,
                    fee_token,
                    pool,
                    twap_window_secs: self.twap_window_secs,
                })
            }
            None => {
                return Err(
                    "unknown chain requires --fixed-log-gas-price or complete Uniswap overrides"
                        .to_string(),
                );
            }
        })
    }
}

/// Batch-submitter signing-key source, shared by `run` and `flush-mempool`
/// (both sign L1 transactions). `setup` does NOT use this — it takes the
/// submitter address directly and never signs.
#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("batch_submitter_key_source")
        .args(["batch_submitter_private_key", "batch_submitter_private_key_file"])
        .required(true)
        .multiple(false)
))]
pub struct KeyArgs {
    /// Hex-encoded private key for the batch submitter.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_AUTH_PRIVATE_KEY",
        hide_env_values = true,
        group = "batch_submitter_key_source"
    )]
    batch_submitter_private_key: Option<String>,
    /// Path to a file whose first line contains the batch submitter private key.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE",
        hide_env_values = true,
        group = "batch_submitter_key_source"
    )]
    batch_submitter_private_key_file: Option<String>,
}

impl KeyArgs {
    /// Resolve the batch submitter private key from either the inline value or a key file.
    pub fn resolve(&self) -> Result<String, std::io::Error> {
        resolve_key_source(
            &self.batch_submitter_private_key,
            &self.batch_submitter_private_key_file,
        )
        .map(|opt| opt.expect("batch submitter private key is required by CLI arg group"))
    }
}

/// Resolve a batch-submitter key from an inline value or a key file (first line).
/// Returns `Ok(None)` when neither source is set — `KeyArgs` makes that
/// impossible via its required arg group, but `setup --recovery` validates the
/// "exactly one source" rule in code (the group can't be conditionally required
/// at the clap level), so it needs the `None` case.
fn resolve_key_source(
    inline: &Option<String>,
    file: &Option<String>,
) -> Result<Option<String>, std::io::Error> {
    if let Some(file) = file {
        let contents = std::fs::read_to_string(file)?;
        Ok(Some(
            contents.lines().next().unwrap_or("").trim().to_string(),
        ))
    } else {
        Ok(inline.clone())
    }
}

/// Batch-submitter signing-key source for a context where the key is
/// *conditionally* required — `setup --recovery` needs it (to flush the wallet
/// nonce), plain `setup` must not carry it. clap can't express a
/// conditionally-required arg group, so unlike [`KeyArgs`] neither source is
/// required at the clap level; the owner validates presence + exclusivity via
/// [`is_present`](Self::is_present) / [`both_set`](Self::both_set).
#[derive(Debug, Clone, Args)]
pub struct OptionalKeyArgs {
    /// Hex-encoded batch-submitter private key.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_AUTH_PRIVATE_KEY",
        hide_env_values = true
    )]
    batch_submitter_private_key: Option<String>,
    /// Path to a file whose first line is the batch-submitter private key.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE",
        hide_env_values = true
    )]
    batch_submitter_private_key_file: Option<String>,
}

impl OptionalKeyArgs {
    /// At least one source is set.
    fn is_present(&self) -> bool {
        self.batch_submitter_private_key.is_some()
            || self.batch_submitter_private_key_file.is_some()
    }

    /// Both sources are set — the owner rejects this (at most one is allowed).
    fn both_set(&self) -> bool {
        self.batch_submitter_private_key.is_some()
            && self.batch_submitter_private_key_file.is_some()
    }

    /// Resolve the key if either source is set; `Ok(None)` when neither is.
    fn resolve_if_present(&self) -> Result<Option<String>, std::io::Error> {
        resolve_key_source(
            &self.batch_submitter_private_key,
            &self.batch_submitter_private_key_file,
        )
    }
}

/// `setup` — establish the deployment's timeless state: pin identity, do the
/// initial L1 sync, register the genesis finalized snapshot, write the
/// setup-complete marker. L1-read-only: takes the batch-submitter address, not
/// the signing key.
#[derive(Debug, Clone, Args)]
pub struct SetupConfig {
    #[arg(long, env = "CARTESI_SEQUENCER_DATA_DIR", default_value = DEFAULT_DATA_DIR, value_parser = parse_non_empty_string)]
    pub data_dir: String,
    #[arg(long, env = "CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT", hide_env_values = true, value_parser = parse_non_empty_string)]
    pub eth_rpc_url: String,
    /// Allow plaintext (`http://`) RPC to a non-loopback host — a trusted
    /// private network (Docker/K8s service name, `host.docker.internal`, a
    /// private-VPC IP) where anvil-style plaintext is normal. Off by default:
    /// the provider refuses remote plaintext otherwise, guarding against
    /// accidentally sending L1 traffic over the public internet in the clear.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_ALLOW_INSECURE_RPC",
        default_value_t = false
    )]
    pub allow_insecure_rpc: bool,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    #[arg(long, env = "CARTESI_SEQUENCER_LONG_BLOCK_RANGE_ERROR_CODES", value_delimiter = ',', default_values = crate::l1::partition::DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES)]
    pub long_block_range_error_codes: Vec<String>,
    /// Expected chain ID. Validated against the RPC at setup and pinned.
    #[arg(long, env = "CARTESI_SEQUENCER_BLOCKCHAIN_ID")]
    pub chain_id: u64,
    /// Application (EIP-712 verifying contract) address.
    #[arg(long, env = "CARTESI_SEQUENCER_APP_ADDRESS", value_parser = parse_address)]
    pub app_address: Address,
    /// Address that submits batches. Pinned into the deployment identity.
    /// `setup` never signs, so it takes the address (derivable from the key
    /// without the secret) rather than the key itself.
    #[arg(long, env = "CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS", value_parser = parse_address)]
    pub batch_submitter_address: Address,
    /// L1 inclusion block `B` of the trusted checkpoint machine this setup
    /// boots from. Default `0` = genesis bootstrap. Used only as the lower
    /// bound of `setup`'s read-only detection scan: if a previous instance
    /// left any batch-submitter tx past `B` (or its wallet nonce is unsettled),
    /// `setup` refuses and points the operator at recovery. PR3 does not yet
    /// load a non-genesis checkpoint machine — that is `setup --recovery` (PR5);
    /// here `B` only scopes detection, so `B > 0` against a genesis-style setup
    /// merely narrows the scan.
    #[arg(long, env = "CARTESI_SEQUENCER_CHECKPOINT_BLOCK", default_value_t = 0)]
    pub checkpoint_block: u64,
    /// Recovery mode (cockroach recovery): rebuild this freshly-wiped DB from a
    /// trusted checkpoint at `--checkpoint-block` instead of genesis bootstrap.
    /// Requires `--checkpoint-dump-dir`, `--checkpoint-block > 0`, and the
    /// batch-submitter signing key (recovery flushes the wallet nonce, which
    /// signs L1 no-ops). Plain `setup` stays L1-read-only and key-less.
    #[arg(long, env = "CARTESI_SEQUENCER_RECOVERY", default_value_t = false)]
    pub recovery: bool,
    /// Directory of the trusted checkpoint dump (a finalized `dumps/<id>/` with
    /// `state/` + `info.toml`) that `setup --recovery` boots the machine `S`
    /// from. Required with `--recovery`; rejected without it. This is a
    /// **sequencer** dump from `$CARTESI_SEQUENCER_DATA_DIR/dumps/`, not a
    /// watchdog CM checkpoint under `.../checkpoints/<block>/`.
    #[arg(long, env = "CARTESI_SEQUENCER_CHECKPOINT_DUMP_DIR", value_parser = parse_non_empty_string)]
    pub checkpoint_dump_dir: Option<String>,
    /// Batch-submitter signing key, recovery-only (for the wallet-nonce flush).
    /// Conditionally required: present iff `--recovery`. The two sources are
    /// mutually exclusive and rejected on a non-recovery `setup` — see
    /// [`SetupConfig::validate`].
    #[command(flatten)]
    key: OptionalKeyArgs,
    #[command(flatten)]
    pub timing: TimingArgs,
    #[command(flatten)]
    pub fee_oracle: FeeOracleArgs,
}

impl SetupConfig {
    pub fn db_path(&self) -> String {
        db_path_in(&self.data_dir)
    }

    /// Validate the cross-field recovery constraints clap can't express (a
    /// conditionally-required arg group). Returns the first violation as a
    /// human-readable message; the caller maps it to a terminal bootstrap error.
    ///
    /// Recovery requires a real checkpoint (`--checkpoint-block > 0` + a dump
    /// dir) and the signing key; a plain `setup` must carry none of the
    /// recovery-only inputs (key, dump dir).
    pub fn validate(&self) -> Result<(), String> {
        let key_present = self.key.is_present();
        if self.key.both_set() {
            return Err(
                "--batch-submitter-private-key and --batch-submitter-private-key-file \
                        are mutually exclusive"
                    .to_string(),
            );
        }
        if self.recovery {
            if self.checkpoint_dump_dir.is_none() {
                return Err("--recovery requires --checkpoint-dump-dir".to_string());
            }
            if self.checkpoint_block == 0 {
                return Err("--recovery requires --checkpoint-block > 0 \
                            (recovery boots a non-genesis checkpoint)"
                    .to_string());
            }
            if !key_present {
                return Err("--recovery requires the batch-submitter signing key \
                            (--batch-submitter-private-key[-file]) to flush the wallet nonce"
                    .to_string());
            }
        } else {
            if self.checkpoint_dump_dir.is_some() {
                return Err("--checkpoint-dump-dir is only valid with --recovery".to_string());
            }
            if key_present {
                return Err("plain `setup` is L1-read-only and takes no signing key; \
                            pass the batch-submitter address, or use --recovery"
                    .to_string());
            }
        }
        Ok(())
    }

    /// Resolve the recovery signing key. Precondition: [`SetupConfig::validate`]
    /// passed with `recovery == true` (so exactly one source is set).
    pub fn resolve_recovery_key(&self) -> Result<String, std::io::Error> {
        self.key
            .resolve_if_present()
            .map(|opt| opt.expect("recovery key presence is validated before resolve"))
    }
}

/// `run` — boot the sequencer from an already-set-up DB. Identity (chain id,
/// app address, InputBox address, app deployment block, submitter address) is read
/// from the DB, so those are NOT CLI args here. Keeps the signing key (run
/// submits) and the runtime tuning knobs.
#[derive(Debug, Clone, Args)]
pub struct RunConfig {
    #[arg(long, env = "CARTESI_SEQUENCER_HTTP_ADDR", default_value = DEFAULT_HTTP_ADDR, value_parser = parse_non_empty_string)]
    pub http_addr: String,
    #[arg(long, env = "CARTESI_SEQUENCER_DATA_DIR", default_value = DEFAULT_DATA_DIR, value_parser = parse_non_empty_string)]
    pub data_dir: String,
    #[arg(long, env = "CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT", hide_env_values = true, value_parser = parse_non_empty_string)]
    pub eth_rpc_url: String,
    /// Allow plaintext (`http://`) RPC to a non-loopback host — a trusted
    /// private network (Docker/K8s service name, `host.docker.internal`, a
    /// private-VPC IP) where anvil-style plaintext is normal. Off by default:
    /// the provider refuses remote plaintext otherwise, guarding against
    /// accidentally sending L1 traffic over the public internet in the clear.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_ALLOW_INSECURE_RPC",
        default_value_t = false
    )]
    pub allow_insecure_rpc: bool,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    #[arg(long, env = "CARTESI_SEQUENCER_LONG_BLOCK_RANGE_ERROR_CODES", value_delimiter = ',', default_values = crate::l1::partition::DEFAULT_LONG_BLOCK_RANGE_ERROR_CODES)]
    pub long_block_range_error_codes: Vec<String>,
    #[command(flatten)]
    key: KeyArgs,

    /// How often the batch submitter polls for new work when idle.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS",
        default_value = "5000"
    )]
    pub batch_submitter_idle_poll_interval_ms: u64,

    /// Additional confirmations to wait for after a batch-submission tx is included on L1.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_BATCH_SUBMITTER_CONFIRMATION_DEPTH",
        default_value = "2"
    )]
    pub batch_submitter_confirmation_depth: u64,

    /// Force the inclusion lane to seal the open batch after this many seconds,
    /// regardless of size — the liveness bound that caps batch-posting latency
    /// for a low-traffic deployment. Default 7200 (2h); operators trade latency
    /// against L1 cost, and tests set it small to seal batches promptly.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_MAX_BATCH_OPEN_SECONDS",
        default_value = "7200",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub max_batch_open_seconds: u64,

    #[command(flatten)]
    pub timing: TimingArgs,
    #[command(flatten)]
    pub fee_oracle: FeeOraclePollArgs,
}

impl RunConfig {
    /// Full path to the SQLite database file inside `data_dir`.
    pub fn db_path(&self) -> String {
        db_path_in(&self.data_dir)
    }

    /// Force-close-an-open-batch deadline as a [`Duration`] (the liveness bound
    /// the inclusion lane applies — see [`CARTESI_SEQUENCER_MAX_BATCH_OPEN_SECONDS`]).
    pub fn max_batch_open(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.max_batch_open_seconds)
    }

    /// Build a validated [`ProtocolTiming`] from this config's tuning fields.
    pub fn protocol_timing(&self) -> Result<ProtocolTiming, ProtocolTimingError> {
        self.timing.protocol_timing()
    }

    /// Resolve the batch submitter private key from either the inline value or a key file.
    pub fn resolve_private_key(&self) -> Result<String, std::io::Error> {
        self.key.resolve()
    }
}

/// `flush-mempool` — settle the batch-submitter wallet nonce on demand
///. Reads the submitter address + watermark from the DB; signs
/// no-op transactions, so it needs the key.
#[derive(Debug, Clone, Args)]
pub struct FlushConfig {
    #[arg(long, env = "CARTESI_SEQUENCER_DATA_DIR", default_value = DEFAULT_DATA_DIR, value_parser = parse_non_empty_string)]
    pub data_dir: String,
    #[arg(long, env = "CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT", hide_env_values = true, value_parser = parse_non_empty_string)]
    pub eth_rpc_url: String,
    /// Allow plaintext (`http://`) RPC to a non-loopback host — a trusted
    /// private network (Docker/K8s service name, `host.docker.internal`, a
    /// private-VPC IP) where anvil-style plaintext is normal. Off by default:
    /// the provider refuses remote plaintext otherwise, guarding against
    /// accidentally sending L1 traffic over the public internet in the clear.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_ALLOW_INSECURE_RPC",
        default_value_t = false
    )]
    pub allow_insecure_rpc: bool,
    #[command(flatten)]
    key: KeyArgs,
    /// Assumed L1 block time in seconds; sets the flusher's confirmation /
    /// safe-poll cadence.
    #[arg(long, env = "CARTESI_SEQUENCER_SECONDS_PER_BLOCK", default_value = "12", value_parser = clap::value_parser!(u64).range(1..))]
    pub seconds_per_block: u64,
}

impl FlushConfig {
    pub fn db_path(&self) -> String {
        db_path_in(&self.data_dir)
    }

    pub fn resolve_private_key(&self) -> Result<String, std::io::Error> {
        self.key.resolve()
    }
}

fn parse_non_empty_string(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("value cannot be empty".to_string());
    }
    Ok(value.to_string())
}

fn parse_address(raw: &str) -> Result<Address, String> {
    if !raw.starts_with("0x") {
        return Err("address must be 0x-prefixed".to_string());
    }

    let bytes =
        alloy_primitives::hex::decode(raw).map_err(|err| format!("invalid address hex: {err}"))?;
    if bytes.len() != 20 {
        return Err("address must be 20 bytes".to_string());
    }
    Ok(Address::from_slice(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Cli;
    use clap::Parser;

    // Parse a full subcommand line through the top-level `Cli`, since the
    // per-subcommand configs derive `Args` (embeddable) not `Parser`.
    const SETUP_ARGS: [&str; 10] = [
        "sequencer",
        "setup",
        "--eth-rpc-url",
        "http://127.0.0.1:8545",
        "--chain-id",
        "31337",
        "--app-address",
        "0x1111111111111111111111111111111111111111",
        "--batch-submitter-address",
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    ];

    const RUN_ARGS: [&str; 5] = [
        "sequencer",
        "run",
        "--eth-rpc-url",
        "http://127.0.0.1:8545",
        "--batch-submitter-private-key",
    ];

    const TEST_KEY: &str = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn run_config_from(extra: &[&str]) -> RunConfig {
        let mut args: Vec<&str> = RUN_ARGS.to_vec();
        args.push(TEST_KEY);
        args.extend_from_slice(extra);
        match Cli::try_parse_from(args).expect("parse run").command {
            crate::harness::Command::Run(c) => *c,
            other => panic!("expected run subcommand, got {other:?}"),
        }
    }

    fn setup_config() -> SetupConfig {
        setup_config_from(&[])
    }

    fn setup_config_from(extra: &[&str]) -> SetupConfig {
        let mut args: Vec<&str> = SETUP_ARGS.to_vec();
        args.extend_from_slice(extra);
        match Cli::try_parse_from(args).expect("parse setup").command {
            crate::harness::Command::Setup(c) => *c,
            other => panic!("expected setup subcommand, got {other:?}"),
        }
    }

    #[test]
    fn setup_requires_address_not_key() {
        let cfg = setup_config();
        assert_eq!(cfg.chain_id, 31337);
        assert_eq!(
            cfg.batch_submitter_address,
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
                .parse::<Address>()
                .unwrap()
        );
        // The default db path lives under the default data dir.
        assert!(cfg.db_path().ends_with("sequencer.db"));
    }

    #[test]
    fn setup_checkpoint_block_defaults_to_genesis_and_parses() {
        // Default = 0 (genesis bootstrap) when the flag is absent.
        assert_eq!(setup_config().checkpoint_block, 0);
        // Explicit value parses.
        let mut args: Vec<&str> = SETUP_ARGS.to_vec();
        args.push("--checkpoint-block");
        args.push("4200");
        let cfg = match Cli::try_parse_from(args).expect("parse setup").command {
            crate::harness::Command::Setup(c) => *c,
            other => panic!("expected setup subcommand, got {other:?}"),
        };
        assert_eq!(cfg.checkpoint_block, 4200);
    }

    #[test]
    fn plain_setup_passes_validation_and_is_not_recovery() {
        let cfg = setup_config();
        assert!(!cfg.recovery);
        assert!(cfg.checkpoint_dump_dir.is_none());
        cfg.validate().expect("plain setup is a valid config");
    }

    #[test]
    fn setup_rejects_a_signing_key_without_recovery() {
        // The key now parses (recovery needs it), but a non-recovery `setup`
        // carrying one is rejected by cross-field validation, not at parse time.
        let cfg = setup_config_from(&["--batch-submitter-private-key", TEST_KEY]);
        let err = cfg.validate().expect_err("plain setup must reject a key");
        assert!(
            err.contains("key-less") || err.contains("L1-read-only"),
            "got: {err}"
        );
    }

    #[test]
    fn setup_rejects_dump_dir_without_recovery() {
        let cfg = setup_config_from(&["--checkpoint-dump-dir", "/tmp/ckpt"]);
        let err = cfg
            .validate()
            .expect_err("dump dir without --recovery must fail");
        assert!(err.contains("--checkpoint-dump-dir"), "got: {err}");
    }

    #[test]
    fn recovery_requires_dump_dir_block_and_key() {
        // --recovery alone (no dump dir / block / key) is invalid; each missing
        // input is reported.
        let bare = setup_config_from(&["--recovery"]);
        assert!(
            bare.validate()
                .unwrap_err()
                .contains("--checkpoint-dump-dir")
        );

        let no_block = setup_config_from(&["--recovery", "--checkpoint-dump-dir", "/tmp/ckpt"]);
        assert!(
            no_block
                .validate()
                .unwrap_err()
                .contains("--checkpoint-block > 0")
        );

        let no_key = setup_config_from(&[
            "--recovery",
            "--checkpoint-dump-dir",
            "/tmp/ckpt",
            "--checkpoint-block",
            "1200",
        ]);
        assert!(no_key.validate().unwrap_err().contains("signing key"));
    }

    #[test]
    fn recovery_full_config_validates_and_resolves_key() {
        let cfg = setup_config_from(&[
            "--recovery",
            "--checkpoint-dump-dir",
            "/tmp/ckpt",
            "--checkpoint-block",
            "1200",
            "--batch-submitter-private-key",
            TEST_KEY,
        ]);
        cfg.validate().expect("full recovery config is valid");
        assert!(cfg.recovery);
        assert_eq!(cfg.checkpoint_block, 1200);
        assert_eq!(cfg.checkpoint_dump_dir.as_deref(), Some("/tmp/ckpt"));
        assert_eq!(cfg.resolve_recovery_key().expect("resolve key"), TEST_KEY);
    }

    #[test]
    fn recovery_rejects_both_key_sources() {
        let cfg = setup_config_from(&[
            "--recovery",
            "--checkpoint-dump-dir",
            "/tmp/ckpt",
            "--checkpoint-block",
            "1200",
            "--batch-submitter-private-key",
            TEST_KEY,
            "--batch-submitter-private-key-file",
            "/tmp/key",
        ]);
        assert!(cfg.validate().unwrap_err().contains("mutually exclusive"));
    }

    #[test]
    fn run_requires_a_key_and_drops_identity_args() {
        // Key required.
        let mut args: Vec<&str> = vec!["sequencer", "run", "--eth-rpc-url", "http://x"];
        assert!(
            Cli::try_parse_from(args.clone()).is_err(),
            "run requires a batch-submitter key"
        );
        // chain-id / app-address are no longer run args.
        args.push("--batch-submitter-private-key");
        args.push(TEST_KEY);
        args.push("--chain-id");
        args.push("31337");
        assert!(
            Cli::try_parse_from(args).is_err(),
            "run must reject --chain-id (read from DB now)"
        );
    }

    #[test]
    fn run_uses_default_block_range_retry_codes() {
        let config = run_config_from(&[]);
        assert_eq!(
            config.long_block_range_error_codes,
            vec![
                "-32005".to_string(),
                "-32012".to_string(),
                "-32600".to_string(),
                "-32602".to_string(),
                "-32616".to_string()
            ]
        );
    }

    #[test]
    fn run_defaults_batch_submitter_confirmation_depth_to_two() {
        assert_eq!(run_config_from(&[]).batch_submitter_confirmation_depth, 2);
    }

    #[test]
    fn fee_oracle_fixed_mode_rejects_uniswap_override() {
        let config = setup_config_from(&[
            "--fixed-log-gas-price",
            "7",
            "--fee-token-address",
            "0x1111111111111111111111111111111111111111",
        ]);
        assert!(
            config
                .fee_oracle
                .resolve(31_337)
                .expect_err("fixed and Uniswap config must not mix")
                .contains("cannot be combined")
        );
    }

    #[test]
    fn fee_oracle_partial_uniswap_override_is_rejected() {
        let config = setup_config_from(&[
            "--fee-token-address",
            "0x1111111111111111111111111111111111111111",
        ]);
        assert!(
            config
                .fee_oracle
                .resolve(31_337)
                .expect_err("all Uniswap fields are required together")
                .contains("require")
        );
    }

    #[test]
    fn fee_oracle_uses_public_chain_presets() {
        let config = setup_config_from(&["--twap-window-secs", "42"]);
        let FeeOracleMode::Uniswap(mainnet) = config.fee_oracle.resolve(1).unwrap() else {
            panic!("mainnet must use Uniswap preset");
        };
        assert_eq!(
            mainnet.pool,
            crate::l1::fee_oracle::uniswap::MAINNET_USDC_WETH_005_POOL
        );
        assert_eq!(mainnet.twap_window_secs, 42);

        let FeeOracleMode::Uniswap(sepolia) = config.fee_oracle.resolve(11_155_111).unwrap() else {
            panic!("Sepolia must use Uniswap preset");
        };
        assert_eq!(
            sepolia.pool,
            crate::l1::fee_oracle::uniswap::SEPOLIA_USDC_WETH_005_POOL
        );
    }

    #[test]
    fn fee_oracle_rejects_unknown_chain_without_explicit_source() {
        assert!(
            setup_config()
                .fee_oracle
                .resolve(31_337)
                .expect_err("unknown chain must require an explicit source")
                .contains("unknown chain")
        );
    }

    #[test]
    fn fee_oracle_full_uniswap_override_is_accepted() {
        let fee_token = "0x1111111111111111111111111111111111111111";
        let weth = "0x2222222222222222222222222222222222222222";
        let pool = "0x3333333333333333333333333333333333333333";
        let config = setup_config_from(&[
            "--fee-token-address",
            fee_token,
            "--weth-address",
            weth,
            "--uniswap-v3-pool",
            pool,
            "--twap-window-secs",
            "900",
        ]);
        let FeeOracleMode::Uniswap(resolved) = config.fee_oracle.resolve(31_337).unwrap() else {
            panic!("custom overrides must select Uniswap mode");
        };
        assert_eq!(resolved.chain_id, 31_337);
        assert_eq!(resolved.fee_token.to_string().to_lowercase(), fee_token);
        assert_eq!(resolved.weth.to_string().to_lowercase(), weth);
        assert_eq!(resolved.pool.to_string().to_lowercase(), pool);
        assert_eq!(resolved.twap_window_secs, 900);
    }

    #[test]
    fn timing_args_reject_zero_seconds_per_block() {
        let err = Cli::try_parse_from([
            "sequencer",
            "run",
            "--eth-rpc-url",
            "http://x",
            "--batch-submitter-private-key",
            TEST_KEY,
            "--seconds-per-block",
            "0",
        ])
        .expect_err("seconds_per_block=0 must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("--seconds-per-block") || message.contains("seconds_per_block"),
            "error must name the offending field, got: {message}"
        );
    }

    #[test]
    fn run_default_seconds_per_block_is_12() {
        assert_eq!(run_config_from(&[]).timing.seconds_per_block, 12);
    }

    #[test]
    fn run_default_l1_read_stale_after_blocks_is_600() {
        assert_eq!(run_config_from(&[]).timing.l1_read_stale_after_blocks, 600);
    }

    #[test]
    fn timing_args_reject_zero_l1_read_stale_after_blocks() {
        let err = Cli::try_parse_from([
            "sequencer",
            "run",
            "--eth-rpc-url",
            "http://x",
            "--batch-submitter-private-key",
            TEST_KEY,
            "--l1-read-stale-after-blocks",
            "0",
        ])
        .expect_err("l1_read_stale_after_blocks=0 must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("--l1-read-stale-after-blocks")
                || message.contains("l1_read_stale_after_blocks"),
            "error must name the offending field, got: {message}"
        );
    }

    /// Render `--help` for `subcmd` inside a child process whose env carries
    /// the given secret values. Mutating this process's env is unsound under
    /// parallel `cargo test` (and neighbouring Clap parses race on the same
    /// vars), so the probe runs via `Command::env` on a re-exec of this
    /// test binary.
    fn help_text_with_secret_env(subcmd: &str, secrets: &[(&str, &str)]) -> String {
        let out_path = std::env::temp_dir().join(format!(
            "sequencer-help-leak-{}-{}-{}.txt",
            subcmd,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        let exe = std::env::current_exe().expect("current_exe");
        let mut cmd = std::process::Command::new(&exe);
        cmd.args([
            "--exact",
            "runtime::config::tests::help_does_not_leak_private_key_value",
            "--quiet",
        ])
        .env("SEQUENCER_HELP_LEAK_OUT", &out_path)
        .env("SEQUENCER_HELP_LEAK_SUBCMD", subcmd);
        for (key, value) in secrets {
            cmd.env(key, value);
        }

        let status = cmd.status().expect("spawn help-leak child");
        assert!(
            status.success(),
            "help-leak child failed for subcommand {subcmd}: {status}"
        );
        let help = std::fs::read_to_string(&out_path)
            .unwrap_or_else(|err| panic!("read help output {}: {err}", out_path.display()));
        let _ = std::fs::remove_file(&out_path);
        help
    }

    #[test]
    fn help_does_not_leak_private_key_value() {
        // Child arm: render help under the caller's env and write it out.
        // Exits the process so we never nest another spawn.
        if let Ok(out_path) = std::env::var("SEQUENCER_HELP_LEAK_OUT") {
            let subcmd = std::env::var("SEQUENCER_HELP_LEAK_SUBCMD")
                .expect("SEQUENCER_HELP_LEAK_SUBCMD must be set in the child");
            let help = Cli::try_parse_from(["sequencer", subcmd.as_str(), "--help"])
                .expect_err("--help must short-circuit as a clap error")
                .to_string();
            std::fs::write(&out_path, help).expect("write help output");
            std::process::exit(0);
        }

        let sentinel_key = "0xSENTINEL_PRIVATE_KEY_DEADBEEF1234567890abcdef";
        let sentinel_file = "/secret/path/key.pem";
        let sentinel_rpc = "https://eth-mainnet.g.alchemy.com/v2/SECRET_TOKEN_XYZ";
        let secrets = [
            ("CARTESI_SEQUENCER_AUTH_PRIVATE_KEY", sentinel_key),
            ("CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE", sentinel_file),
            ("CARTESI_SEQUENCER_BLOCKCHAIN_HTTP_ENDPOINT", sentinel_rpc),
        ];

        for subcmd in ["run", "setup", "flush-mempool"] {
            let help = help_text_with_secret_env(subcmd, &secrets);

            assert!(
                !help.contains(sentinel_key),
                "{subcmd} --help must not print the private key value"
            );
            assert!(
                !help.contains(sentinel_file),
                "{subcmd} --help must not print the key-file path"
            );
            assert!(
                !help.contains("SECRET_TOKEN_XYZ"),
                "{subcmd} --help must not print the RPC URL (may contain API tokens)"
            );
            assert!(
                help.contains("CARTESI_SEQUENCER_AUTH_PRIVATE_KEY"),
                "{subcmd} --help should still mention the env var name"
            );
        }
    }

    #[test]
    fn domain_uses_fixed_name_and_version() {
        use alloy_primitives::U256;
        use sequencer_core::{DOMAIN_NAME, DOMAIN_VERSION};
        let domain = sequencer_core::build_input_domain(31337, Address::from_slice(&[0x11; 20]));
        assert_eq!(domain.name.as_deref(), Some(DOMAIN_NAME));
        assert_eq!(domain.version.as_deref(), Some(DOMAIN_VERSION));
        assert_eq!(domain.chain_id, Some(U256::from(31337_u64)));
        assert_eq!(
            domain.verifying_contract,
            Some(Address::from_slice(&[0x11; 20]))
        );
    }
}
