// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! CLI configuration for `setup`, `run` and `flush-mempool`: the owner of
//! env-var names, defaults and validation. Shared arg groups (`L1RpcArgs`,
//! `TimingArgs`, `KeyArgs`) are flattened into each command so names and defaults
//! cannot drift. The setup/run phase split is in README "Running".

use alloy_primitives::Address;
use clap::{ArgGroup, Args};

use crate::commands::error::BootstrapError;
use crate::l1::SubmitterKey;

const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:3000";
const DEFAULT_INTERNAL_HTTP_ADDR: &str = "127.0.0.1:3001";
const DEFAULT_DATA_DIR: &str = "sequencer-data";
const DB_FILENAME: &str = "sequencer.db";

/// Blocks a time-sealed batch needs after its open window to become
/// safe-accepted inside the staleness window: its first frame trails the safe
/// head by under
/// one frame-clock interval (5), Ethereum's safe head trails inclusion by up to
/// two epochs (64), and the rest covers submitter pickup, inclusion and the dump.
const BATCH_SUBMISSION_MARGIN_BLOCKS: u64 = 100;

/// Full path to the SQLite database file inside `data_dir`.
pub fn db_path_in(data_dir: &str) -> String {
    std::path::Path::new(data_dir)
        .join(DB_FILENAME)
        .to_string_lossy()
        .into_owned()
}

/// L1 RPC endpoint, shared by setup, run and flush-mempool.
#[derive(Debug, Clone, Args)]
pub struct L1RpcArgs {
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
}

/// The L1 block-time estimate shared by `setup` and `run`.
#[derive(Debug, Clone, Args)]
pub struct TimingArgs {
    /// Assumed L1 block time in seconds. Sets the flush's and the poster's
    /// confirmation cadence, and converts `--max-batch-open-seconds` into
    /// blocks for the batch-open check.
    #[arg(long, env = "CARTESI_SEQUENCER_SECONDS_PER_BLOCK", default_value = "12", value_parser = clap::value_parser!(u64).range(1..))]
    pub seconds_per_block: u64,
}

/// L1 fee-oracle source selection. A fixed logarithmic price is intended for
/// local development; public chains use a validated Uniswap V3 TWAP source.
#[derive(Debug, Clone, Args)]
pub struct FeeOracleArgs {
    /// Fixed logarithmic gas price for local/Anvil deployments. Negative values
    /// price below one fee-token smallest unit per gas; setup refuses one that
    /// would push the recommended fee below exponent 0.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_FEE_ORACLE_FIXED_LOG_GAS_PRICE",
        allow_negative_numbers = true
    )]
    pub fixed_log_gas_price: Option<i16>,
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
    Fixed { log_gas_price: i16 },
    Uniswap(crate::l1::fee_oracle::UniswapConfig),
}

impl FeeOracleArgs {
    /// Resolve explicit overrides or the public-chain presets; an unknown chain
    /// needs an explicit source. This is pure so invalid combinations fail
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
/// (both sign L1 transactions). `setup` takes the same two sources through
/// [`SetupKeyArgs`], where `--dry-run` may leave both out.
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
        group = "batch_submitter_key_source",
        value_parser = parse_submitter_key
    )]
    batch_submitter_private_key: Option<SubmitterKey>,
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
    pub fn resolve(&self) -> Result<SubmitterKey, BootstrapError> {
        resolve_key_source(
            &self.batch_submitter_private_key,
            &self.batch_submitter_private_key_file,
        )
        .map(|key| key.expect("batch submitter private key is required by CLI arg group"))
    }
}

/// `setup`'s signing-key source: the same arguments as [`KeyArgs`], required
/// unless `--dry-run`, which flushes nothing.
#[derive(Debug, Clone, Args)]
pub struct SetupKeyArgs {
    /// Hex-encoded private key for the batch submitter.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_AUTH_PRIVATE_KEY",
        hide_env_values = true,
        conflicts_with = "batch_submitter_private_key_file",
        required_unless_present_any = ["batch_submitter_private_key_file", "dry_run"],
        value_parser = parse_submitter_key
    )]
    batch_submitter_private_key: Option<SubmitterKey>,
    /// Path to a file whose first line contains the batch submitter private key.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_AUTH_PRIVATE_KEY_FILE",
        hide_env_values = true,
        required_unless_present_any = ["batch_submitter_private_key", "dry_run"]
    )]
    batch_submitter_private_key_file: Option<String>,
}

/// The key from the file when one is named, otherwise the inline value. The
/// error carries the path, never the contents: a key file's bytes must not
/// reach a log line even when the read fails.
fn resolve_key_source(
    inline: &Option<SubmitterKey>,
    file: &Option<String>,
) -> Result<Option<SubmitterKey>, BootstrapError> {
    let Some(file) = file else {
        return Ok(inline.clone());
    };
    let contents = std::fs::read_to_string(file).map_err(|source| BootstrapError::KeyFile {
        path: std::path::PathBuf::from(file),
        source,
    })?;
    Ok(Some(SubmitterKey::new(
        contents.lines().next().unwrap_or("").trim().to_string(),
    )))
}

/// Whether a key-file read failure is deterministic operator
/// misconfiguration — the file is missing, unreadable by this user, not a
/// file, or not text — as opposed to an environmental failure that may clear
/// on restart (a device or memory error). The same kind of split
/// `referenced_artifact_io_is_terminal` draws for snapshot artifacts, over a
/// different set: a whole-file read cannot produce `UnexpectedEof`, and a
/// permission denial on the operator-named key path is the operator's own
/// mount or mode mistake, where a snapshot artifact's mode can be repaired
/// underneath a running process. `NotFound` is deliberately terminal even
/// though a secret populated after the process starts hits it: the
/// orchestrator must mount the key before start, and a typo'd path
/// restart-looping unpaged is the failure this replaces. The genesis dump
/// keeps PermissionDenied operational so native and C engines agree; the key
/// file is read by the host alone and never crosses the engine seam, so its
/// denial stays terminal.
pub(crate) fn key_file_io_is_terminal(source: &std::io::Error) -> bool {
    matches!(
        source.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::InvalidData
            | std::io::ErrorKind::IsADirectory
            | std::io::ErrorKind::NotADirectory
    )
}

/// `setup` — start a new era in an empty data directory: pin identity, flush
/// the submitter's wallet to pin the stop block, sync L1, rebuild the history
/// from the base (genesis, or a stored canonical machine) through the stop
/// block, and publish the era. `docs/recovery/cockroach.md` ("What setup does")
/// has the procedure.
#[derive(Debug, Clone, Args)]
pub struct SetupConfig {
    #[arg(long, env = "CARTESI_SEQUENCER_DATA_DIR", default_value = DEFAULT_DATA_DIR, value_parser = parse_non_empty_string)]
    pub data_dir: String,
    #[command(flatten)]
    pub rpc: L1RpcArgs,
    /// Expected chain ID. Validated against the RPC at setup and pinned.
    #[arg(long, env = "CARTESI_SEQUENCER_BLOCKCHAIN_ID")]
    pub chain_id: u64,
    /// Application (EIP-712 verifying contract) address.
    #[arg(long, env = "CARTESI_SEQUENCER_APP_ADDRESS", value_parser = parse_address)]
    pub app_address: Address,
    /// Address that submits batches, pinned into the deployment identity.
    /// The key must sign for it: setup's flush settles this account.
    #[arg(long, env = "CARTESI_SEQUENCER_BATCH_SUBMITTER_ADDRESS", value_parser = parse_address)]
    pub batch_submitter_address: Address,
    /// A stored canonical machine of the deployment's template, stopped
    /// between inputs, to rebuild from. Without it the base is genesis.
    #[arg(long, value_name = "MACHINE_DIR")]
    pub base: Option<std::path::PathBuf>,
    /// Report the base's counts, the submitter's activity past it and the
    /// rebuild's size, then exit. Needs no key and never touches the data
    /// directory.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    #[command(flatten)]
    key: SetupKeyArgs,
    #[command(flatten)]
    pub timing: TimingArgs,
    #[command(flatten)]
    pub fee_oracle: FeeOracleArgs,
}

impl SetupConfig {
    pub fn db_path(&self) -> String {
        db_path_in(&self.data_dir)
    }

    /// The submitter key. Clap requires one unless `--dry-run`, which never
    /// asks for it.
    pub fn resolve_private_key(&self) -> Result<SubmitterKey, BootstrapError> {
        resolve_key_source(
            &self.key.batch_submitter_private_key,
            &self.key.batch_submitter_private_key_file,
        )
        .map(|key| key.expect("setup requires a batch submitter key unless --dry-run"))
    }
}

/// `run` — boot the sequencer from an already-set-up DB. Identity (chain id,
/// app address, InputBox address, app deployment block, submitter address) is read
/// from the DB, so those are NOT CLI args here. Keeps the signing key (run
/// submits) and the runtime tuning knobs.
#[derive(Debug, Clone, Args)]
pub struct RunConfig {
    /// Public listener: the ingress routes only.
    #[arg(long, env = "CARTESI_SEQUENCER_HTTP_ADDR", default_value = DEFAULT_HTTP_ADDR, value_parser = parse_non_empty_string)]
    pub http_addr: String,
    /// Internal listener: subscription, history, snapshots, health. No
    /// authentication; keep it off the public network.
    #[arg(long, env = "CARTESI_SEQUENCER_INTERNAL_HTTP_ADDR", default_value = DEFAULT_INTERNAL_HTTP_ADDR, value_parser = parse_non_empty_string)]
    pub internal_http_addr: String,
    #[arg(long, env = "CARTESI_SEQUENCER_DATA_DIR", default_value = DEFAULT_DATA_DIR, value_parser = parse_non_empty_string)]
    pub data_dir: String,
    #[command(flatten)]
    pub rpc: L1RpcArgs,
    #[command(flatten)]
    key: KeyArgs,

    /// How often the batch submitter polls for new work when idle.
    #[arg(
        long,
        env = "CARTESI_SEQUENCER_BATCH_SUBMITTER_IDLE_POLL_INTERVAL_MS",
        default_value = "5000",
        value_parser = clap::value_parser!(u64).range(1..)
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
    /// Rounded up to blocks at `--seconds-per-block`, it must leave a
    /// submission margin below the application's staleness window
    /// (validated at startup).
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

    /// Refuse a batch-open window that would seal a low-volume batch too late
    /// to land inside the application's staleness window `max_wait_blocks`.
    pub fn check_batch_open_window(&self, max_wait_blocks: u64) -> Result<(), BootstrapError> {
        let seconds_per_block = self.timing.seconds_per_block;
        // Rounding up counts a partial block against the operator.
        let open_blocks = self.max_batch_open_seconds.div_ceil(seconds_per_block);
        if open_blocks > max_wait_blocks.saturating_sub(BATCH_SUBMISSION_MARGIN_BLOCKS) {
            return Err(BootstrapError::BatchOpenPastWindow {
                max_batch_open_seconds: self.max_batch_open_seconds,
                seconds_per_block,
                open_blocks,
                max_wait_blocks,
                margin_blocks: BATCH_SUBMISSION_MARGIN_BLOCKS,
            });
        }
        Ok(())
    }

    /// Resolve the batch submitter private key from either the inline value or a key file.
    pub fn resolve_private_key(&self) -> Result<SubmitterKey, BootstrapError> {
        self.key.resolve()
    }
}

/// `flush-mempool` — settle the batch-submitter wallet nonce on demand.
/// Reads the submitter address + watermark from the DB and signs no-op
/// transactions, so it needs the key.
#[derive(Debug, Clone, Args)]
pub struct FlushConfig {
    #[arg(long, env = "CARTESI_SEQUENCER_DATA_DIR", default_value = DEFAULT_DATA_DIR, value_parser = parse_non_empty_string)]
    pub data_dir: String,
    #[command(flatten)]
    pub rpc: L1RpcArgs,
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

    pub fn resolve_private_key(&self) -> Result<SubmitterKey, BootstrapError> {
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

/// Wrap the inline key value at the clap edge so the secret is redacted the
/// moment it enters the process (env or flag).
fn parse_submitter_key(raw: &str) -> Result<SubmitterKey, String> {
    Ok(SubmitterKey::new(raw.to_string()))
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
    const SETUP_ARGS_WITHOUT_KEY: [&str; 10] = [
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

    fn setup_args() -> Vec<&'static str> {
        let mut args = SETUP_ARGS_WITHOUT_KEY.to_vec();
        args.extend(["--batch-submitter-private-key", TEST_KEY]);
        args
    }

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
        let mut args = setup_args();
        args.extend_from_slice(extra);
        match Cli::try_parse_from(args).expect("parse setup").command {
            crate::harness::Command::Setup(c) => *c,
            other => panic!("expected setup subcommand, got {other:?}"),
        }
    }

    #[test]
    fn setup_takes_the_address_and_the_key() {
        let cfg = setup_config();
        assert_eq!(cfg.chain_id, 31337);
        assert_eq!(
            cfg.batch_submitter_address,
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(cfg.resolve_private_key().unwrap().expose_secret(), TEST_KEY);
        assert_eq!(cfg.base, None);
        assert!(!cfg.dry_run);
        // The default db path lives under the default data dir.
        assert!(cfg.db_path().ends_with("sequencer.db"));
    }

    /// Setup flushes with the key, so it needs one; the dry run flushes
    /// nothing and needs none. Both sources together are refused.
    #[test]
    fn setup_requires_the_key_unless_dry_run() {
        assert!(
            Cli::try_parse_from(SETUP_ARGS_WITHOUT_KEY).is_err(),
            "setup without a key must not parse"
        );
        let mut dry_run = SETUP_ARGS_WITHOUT_KEY.to_vec();
        dry_run.push("--dry-run");
        let crate::harness::Command::Setup(cfg) = Cli::try_parse_from(dry_run)
            .expect("a dry run needs no key")
            .command
        else {
            panic!("expected setup subcommand");
        };
        assert!(cfg.dry_run);

        let mut both = setup_args();
        both.extend(["--batch-submitter-private-key-file", "/run/secrets/key"]);
        assert!(
            Cli::try_parse_from(both).is_err(),
            "two key sources must not parse"
        );

        let key_file = tempfile::NamedTempFile::new().expect("key file");
        std::fs::write(key_file.path(), format!("{TEST_KEY}\n")).expect("write key");
        let mut from_file = SETUP_ARGS_WITHOUT_KEY.to_vec();
        let path = key_file.path().to_str().expect("utf8 path");
        from_file.extend(["--batch-submitter-private-key-file", path]);
        let crate::harness::Command::Setup(cfg) = Cli::try_parse_from(from_file)
            .expect("a key file parses")
            .command
        else {
            panic!("expected setup subcommand");
        };
        assert_eq!(cfg.resolve_private_key().unwrap().expose_secret(), TEST_KEY);
    }

    #[test]
    fn setup_parses_a_base_machine() {
        let cfg = setup_config_from(&["--base", "/machines/epoch-41", "--dry-run"]);
        assert_eq!(
            cfg.base.as_deref(),
            Some(std::path::Path::new("/machines/epoch-41"))
        );
        assert!(cfg.dry_run);
    }

    /// The checkpoint options of the removed recovery mode are gone: the base
    /// machine names the position.
    #[test]
    fn setup_has_no_checkpoint_options() {
        for extra in [
            &["--recovery"][..],
            &["--checkpoint-dump-dir", "/tmp/dump"][..],
            &["--checkpoint-block", "4200"][..],
        ] {
            let mut args = setup_args();
            args.extend_from_slice(extra);
            Cli::try_parse_from(args).expect_err("a removed option must not parse");
        }
    }

    #[test]
    fn missing_key_file_is_terminal_and_names_the_path_not_the_contents() {
        let args = [
            "sequencer",
            "run",
            "--eth-rpc-url",
            "http://127.0.0.1:8545",
            "--batch-submitter-private-key-file",
            "/nonexistent/dir/submitter.key",
        ];
        let cfg = match Cli::try_parse_from(args).expect("parse run").command {
            crate::harness::Command::Run(c) => *c,
            other => panic!("expected run subcommand, got {other:?}"),
        };
        let err = cfg
            .resolve_private_key()
            .expect_err("a missing key file must not resolve");
        assert!(
            matches!(&err, BootstrapError::KeyFile { path, source }
                if path.to_string_lossy() == "/nonexistent/dir/submitter.key"
                    && source.kind() == std::io::ErrorKind::NotFound),
            "expected a typed key-file error, got {err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("/nonexistent/dir/submitter.key"));
        // Deterministic misconfiguration pages rather than restart-loops.
        assert_eq!(
            crate::commands::error::CommandError::from(err).exit_code(),
            crate::commands::error::EXIT_TERMINAL
        );
    }

    #[test]
    fn run_requires_a_key_and_drops_identity_args() {
        // Key required.
        let mut args: Vec<&str> = vec!["sequencer", "run", "--eth-rpc-url", "http://x"];
        assert!(
            Cli::try_parse_from(args.clone()).is_err(),
            "run requires a batch-submitter key"
        );
        // chain-id / app-address are not run args.
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
    fn fee_oracle_fixed_mode_accepts_a_negative_exponent() {
        let config = setup_config_from(&["--fixed-log-gas-price", "-7"]);
        assert!(matches!(
            config.fee_oracle.resolve(31_337),
            Ok(FeeOracleMode::Fixed { log_gas_price: -7 })
        ));
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
    fn run_rejects_zero_batch_submitter_idle_poll_interval() {
        let mut args: Vec<&str> = RUN_ARGS.to_vec();
        args.extend([TEST_KEY, "--batch-submitter-idle-poll-interval-ms", "0"]);
        let message = Cli::try_parse_from(args)
            .expect_err("a zero idle poll would spin the submitter against the RPC")
            .to_string();
        assert!(
            message.contains("--batch-submitter-idle-poll-interval-ms"),
            "error must name the offending field, got: {message}"
        );
    }

    #[test]
    fn run_default_seconds_per_block_is_12() {
        assert_eq!(run_config_from(&[]).timing.seconds_per_block, 12);
    }

    /// The repair's tuning flags are gone with it.
    #[test]
    fn run_and_setup_have_no_repair_flags() {
        for flag in ["--preemptive-margin-blocks", "--l1-read-stale-after-blocks"] {
            let mut run = RUN_ARGS.to_vec();
            run.extend([TEST_KEY, flag, "300"]);
            Cli::try_parse_from(run).expect_err("a removed run flag must not parse");
            let mut setup = setup_args();
            setup.extend([flag, "300"]);
            Cli::try_parse_from(setup).expect_err("a removed setup flag must not parse");
        }
    }

    /// The staleness window the fixtures below assume.
    const WINDOW: u64 = 1200;

    #[test]
    fn run_bounds_batch_open_window_by_the_window_less_submission_margin() {
        // The window (1200) less the margin (100) leaves 1100 blocks = 13200 s
        // at 12 s.
        run_config_from(&[])
            .check_batch_open_window(WINDOW)
            .expect("default window fits");
        run_config_from(&["--max-batch-open-seconds", "13200"])
            .check_batch_open_window(WINDOW)
            .expect("boundary window fits");
        // 13201 s is 1100.08 blocks; rounding up must refuse it.
        let err = run_config_from(&["--max-batch-open-seconds", "13201"])
            .check_batch_open_window(WINDOW)
            .expect_err("one second past the budget must be refused");
        assert!(
            matches!(
                err,
                BootstrapError::BatchOpenPastWindow {
                    max_batch_open_seconds: 13201,
                    seconds_per_block: 12,
                    open_blocks: 1101,
                    max_wait_blocks: WINDOW,
                    margin_blocks: 100,
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            crate::commands::error::CommandError::from(err).exit_code(),
            crate::commands::error::EXIT_TERMINAL
        );
        let err = run_config_from(&[
            "--max-batch-open-seconds",
            "18446744073709551615",
            "--seconds-per-block",
            "1",
        ])
        .check_batch_open_window(WINDOW)
        .expect_err("an unbounded window must be refused without overflow");
        assert!(
            matches!(err, BootstrapError::BatchOpenPastWindow { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn run_refuses_every_batch_open_window_when_the_window_is_below_the_margin() {
        let err = run_config_from(&["--max-batch-open-seconds", "1"])
            .check_batch_open_window(50)
            .expect_err("no budget remains");
        assert!(
            matches!(
                err,
                BootstrapError::BatchOpenPastWindow {
                    open_blocks: 1,
                    max_wait_blocks: 50,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// At the boundary window a low-volume batch, sealed by time, still has
    /// the submission margin less the frame lag to land before its history
    /// turns void.
    #[test]
    fn boundary_batch_open_window_seals_a_low_volume_batch_inside_the_window() {
        use crate::storage::test_helpers::{SENDER_A, temp_db_with_default_deployment_identity};
        use crate::storage::{SafeInputRange, Storage};
        use sequencer_core::protocol::FRAME_CLOCK_INTERVAL_SAFE_BLOCKS;

        let config = run_config_from(&["--max-batch-open-seconds", "13200"]);
        config
            .check_batch_open_window(WINDOW)
            .expect("boundary window fits");
        let open_blocks = config
            .max_batch_open_seconds
            .div_ceil(config.timing.seconds_per_block);
        // Worst case: the first frame trails the safe head by one block less
        // than the frame-clock interval.
        let frame_lag = FRAME_CLOCK_INTERVAL_SAFE_BLOCKS - 1;
        let first_frame = 10;
        let seal_safe_block = first_frame + frame_lag + open_blocks;

        let db = temp_db_with_default_deployment_identity("batch-open-boundary");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(first_frame, SafeInputRange::empty_at(0))
            .expect("open Tip");
        let void_at = |storage: &mut Storage, safe_block: u64| {
            storage
                .append_safe_inputs(safe_block, &[], SENDER_A)
                .expect("advance safe head");
            storage.terminal_fact().expect("terminal fact").is_some()
        };

        // Low volume: only the deadline seals the batch, and it can still land.
        assert!(!void_at(&mut storage, seal_safe_block));
        storage
            .close_frame_and_batch(&mut head)
            .expect("seal by time");

        // The sealed batch keeps the margin, less the frame lag, to be accepted:
        // the last safe head at which a later block can still accept it is
        // `first_frame + WINDOW - 2`.
        let last_live = first_frame + WINDOW - 2;
        assert_eq!(last_live - seal_safe_block, 100 - frame_lag - 2);
        assert!(!void_at(&mut storage, last_live));
        assert!(void_at(&mut storage, last_live + 1));
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
            "commands::config::tests::help_does_not_leak_private_key_value",
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
            // Every command signs, setup's flush included, so each names the key.
            assert!(
                help.contains("CARTESI_SEQUENCER_AUTH_PRIVATE_KEY"),
                "{subcmd} --help names the key env var"
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
