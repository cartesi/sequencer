// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch poster: broadcasts closed batches to the InputBox at consecutive
//! wallet nonces and watches them to confirmation depth.
//!
//! # Fee policy: re-estimate, never escalate
//!
//! Every tick re-derives the unresolved suffix from L1 and re-broadcasts each
//! pending batch at **this tick's fresh market estimate** ([`estimate_fees`]).
//! The poster keeps no per-nonce fee state and never bumps a price it already
//! offered. This note records why, because the gap that policy leaves is easy
//! to re-find, and the obvious fix is worse than the gap.
//!
//! ## The gap
//!
//! A pending tx at wallet nonce `n` can only be replaced by one that beats it
//! on **both** EIP-1559 components, `max_fee_per_gas` and
//! `max_priority_fee_per_gas`, each strictly and by ≥10% (geth's rule; reth,
//! erigon, Besu and Nethermind ship the same default). A fresh market estimate
//! moves the two components independently: Alloy's default estimator gives
//! `max_fee = 2·base + tip`, where `tip` is a lagging 10-block median of the
//! 20th-percentile reward. So if the base fee more than doubles after a
//! broadcast (our tx is now unmineable) while the tip median has not moved
//! 10%, every re-broadcast is refused — "replacement transaction underpriced",
//! or "already known" when it is byte-identical — and the batch waits until
//! the market moves.
//!
//! The same two replies also appear on an ordinary flat or falling market: a
//! fresh estimate that is not 10% above the pending one is refused either way.
//! There they are benign — the pending tx is at or above the current market
//! and mines. Only the base-doubling case leaves an unmineable tx in the slot.
//!
//! ## Why we accept it
//!
//! The requirement is that a batch land within `MAX_WAIT_BLOCKS` (1200
//! blocks, about four hours at 12 s) or the scheduler skips it as stale — not
//! that any particular tx wins. Well before that, at
//! `MAX_WAIT_BLOCKS − preemptive_margin_blocks` (900 blocks, about three
//! hours at the shipped defaults, counted from the batch's own first frame),
//! the danger detector stops the process and hands the slot to recovery.
//! Measured against those numbers:
//!
//! - The gap needs two conditions that pull against each other. Our cap
//!   carries 2× headroom, so the tx must sit unmined through at least six
//!   consecutive full blocks (base ×1.125 per block; the `+ tip` term makes
//!   six a floor) — *and* the 20th-percentile tip median must stay flat
//!   through exactly those full blocks. Sustained full blocks are when tips
//!   rise; the median follows within one or two minutes, after which the
//!   fresh estimate clears both components and the replacement goes through.
//!   (Inclusion degrades a little before the sixth block, as the effective
//!   tip `max_fee − base` is squeezed, but the estimator bids a
//!   20th-percentile reward, which sits above the marginal included tip.)
//! - It also clears itself when the base falls back below our cap (the
//!   original mines) or when the node drops the tx — eviction, a node
//!   restart, an RPC failover to a peer that never saw it. A *sequencer*
//!   restart does not clear it: with no fee state to lose, the poster
//!   re-derives the same suffix, re-offers the same price, and gets the same
//!   reply. What a restart loses is the in-memory wait clock below, so a
//!   stall that survives one is under-reported, not resolved.
//! - The expected cost is therefore minutes of quiet waiting in a rare
//!   regime, bounded by the danger detector, which is a safe, designed
//!   fallback. And the recovery flusher's one-shot no-op is priced with a
//!   fixed headroom above the current market, so it evicts a pending poster
//!   tx unless the market has fallen more than that headroom since the send
//!   — in which case that tx is over-priced, hence mineable (the residual is
//!   spelled out at the flusher's `submit_noops`).
//!
//! ## Why we do not escalate
//!
//! Any escalation needs memory of what was last offered and a rule for
//! growing it. Four rounds of that were built and adversarially reviewed on
//! cartesi/sequencer#34; each mechanism closed the gap and opened a worse
//! hole:
//!
//! - **A per-nonce floor with asymmetric bumps** (tip ×2, cap ×1.1): the
//!   components compound until `tip > max_fee`, an invalid tx every node
//!   rejects; because a failed send must not raise the floor, the poster
//!   re-sends the same invalid pair forever — a permanent wedge.
//! - **Head-only escalation with suffix skip-and-watch**: a stored hash stops
//!   being ours when the payload↔nonce assignment shifts (RPC read skew, a
//!   nonce consumed by an out-of-gas revert), silently dropping a batch; and
//!   it assumes the node still holds the tx across failover, restart, and
//!   eviction.
//! - **Symmetric floors on every nonce** (the correct memory): the floor
//!   compounds ×1.1 per retry round across the whole suffix, and any tick
//!   that errs after a successful send re-bumps five seconds later. geth
//!   reserves `gas_limit × max_fee` for *every* pending tx from the sender,
//!   so a suffix of compounded caps exhausts an under-funded wallet's balance
//!   check within a single stall — after which every send is refused with
//!   "insufficient funds" and the floor, which a failed send must not raise,
//!   can never come back down.
//! - **A market-relative ceiling on the floor** (K × fresh cap, an escape
//!   valve, a hold state): the flusher priced its cap at the ceiling but its
//!   tip at only ×1.1, so it could not clear a poster tip that had ratcheted
//!   — geth needs ≥10% on *both* — and recovery crash-looped on exactly the
//!   txs it exists to clear; the hold and the node's "already known" reply
//!   formed a fixed point that spun every five seconds; and the escape valve
//!   lifted the recorded cap above the ceiling, voiding the funding bound the
//!   ceiling existed to give.
//!
//! The machinery and the tests pinning it grew by about 1,450 lines across
//! nine files, and every round's review found the next hole in the previous
//! round's fix. Against a fallback that is already safe and a gap whose
//! expected cost is minutes, none of it earns its failure modes.
//!
//! ## What we do instead
//!
//! 1. Re-estimate and attempt every pending nonce, every tick. Nothing is
//!    skipped on the strength of remembered state: a fresh send is accepted,
//!    an identical or underpriced one is refused by the node, and the next
//!    tick tries again. (A refusal does assume the tx occupying that nonce is
//!    this payload's. If the payload↔nonce assignment has shifted, the
//!    refused payload goes out one round later, once the slot resolves —
//!    nothing is dropped, because nothing remembered is trusted.)
//! 2. Treat the node's "this slot is taken and you have not outbid it"
//!    replies — "replacement transaction underpriced", "already known", and
//!    the other clients' wordings listed at [`is_pending_tx_conflict`] — as
//!    what they mean, and report [`SubmitBatchesOutcome::Waiting`] instead of
//!    an error. A confirmation timeout reports the same. The worker sleeps
//!    its idle interval and re-estimates. (The slot is ours because the
//!    runtime's exclusive process lock owns the key.)
//! 3. Warn once a nonce has been unresolved longer than
//!    [`PENDING_WAIT_WARN_AFTER`], and again each interval, whatever the
//!    cause — refused re-broadcasts, or accepted ones that never mine. That
//!    log line is what makes this decision falsifiable. Revisit if it fires
//!    on the same nonce across more than one interval, or on several nonces
//!    in one stall; record the excerpt and the L1 block range in the review
//!    register next to this decision before proposing any memory of past
//!    prices. Two limits: the clock is process-local, so a restart resets it,
//!    and it starts at this process's first attempt, so it is a lower bound
//!    on pending age.
//! 4. Estimate gas without a nonce, at Latest, and pin it padded. Anvil
//!    applies mempool nonce policy to pending-block estimates and would
//!    otherwise reject the re-broadcast before it ever reaches the node.

use alloy::providers::{
    DynProvider, PendingTransactionBuilder, PendingTransactionConfig, PendingTransactionError,
    Provider,
};
use alloy::rpc::types::BlockNumberOrTag;
use async_trait::async_trait;
use cartesi_rollups_contracts::input_box::InputBox;
use sequencer_core::batch::Batch;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::l1::eip1559::{Eip1559Fees, estimate_fees, pad_gas_estimate};
use crate::l1::partition::{decode_evm_advance_input, get_input_added_events_ordered};
use crate::l1::watermark::WalletNonceWatermarkSink;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub type TxHash = alloy_primitives::B256;

/// A pending nonce is expected to resolve within minutes (see the fee-policy
/// note above). Warn once it has been unresolved this long, and again every
/// interval, so a longer wait is visible.
pub(crate) const PENDING_WAIT_WARN_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
pub struct BatchPosterConfig {
    pub l1_submit_address: alloy_primitives::Address,
    pub app_address: alloy_primitives::Address,
    pub batch_submitter_address: alloy_primitives::Address,
    pub start_block: u64,
    pub confirmation_depth: u64,
    /// Assumed L1 block time in seconds, used to derive a conservative
    /// confirmation timeout for watched batch-submission txs.
    pub seconds_per_block: u64,
    /// Error codes that trigger `get_logs` retries with a shorter block range.
    pub long_block_range_error_codes: Vec<String>,
    /// The pinned deployment chain id. Re-confirmed against the RPC immediately
    /// before every productive send (`submit_batches`), so a long-lived
    /// submitter whose load-balanced RPC fails over to another chain refuses to
    /// burn nonce slots on it rather than relying only on the one-shot boot /
    /// reader checks.
    pub expected_chain_id: u64,
}

#[derive(Debug, Error)]
pub enum BatchPosterError {
    #[error("provider/transport: {0}")]
    Provider(String),
    #[error("rpc chain id {rpc} does not match pinned chain id {expected}")]
    ChainIdMismatch { rpc: u64, expected: u64 },
}

/// What one `submit_batches` call did with the pending suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitBatchesOutcome {
    /// Every payload was broadcast this tick and confirmed to depth: one hash
    /// per payload, in nonce order. The worker re-enters at once to pick up
    /// newly closed batches.
    Submitted(Vec<TxHash>),
    /// At least one nonce is still unresolved at the end of the tick: either
    /// the node refused a re-broadcast because it already holds our tx there
    /// at a price this tick's estimate does not beat by ≥10% ("already known"
    /// / "replacement transaction underpriced"), or a broadcast timed out
    /// waiting for confirmation. Nothing is wrong and nothing can be done but
    /// wait for inclusion or for the market to move, so the worker sleeps and
    /// re-estimates. `broadcast` holds the hashes of the payloads that were
    /// sent this tick (fewer than the payload count when a re-broadcast was
    /// refused).
    Waiting { broadcast: Vec<TxHash> },
}

#[async_trait]
pub trait BatchPoster: Send + Sync {
    /// Broadcast the payloads as L1 txs at consecutive wallet nonces.
    /// Implementations must raise `watermark` to the highest nonce they
    /// are about to use *before* the first send (write-before-broadcast,
    /// review R1a).
    async fn submit_batches(
        &self,
        payloads: Vec<Vec<u8>>,
        watermark: &dyn WalletNonceWatermarkSink,
    ) -> Result<SubmitBatchesOutcome, BatchPosterError>;

    async fn observed_submitted_batch_nonces(
        &self,
        from_block: u64,
    ) -> Result<Vec<u64>, BatchPosterError>;
}

/// When this process first attempted a wallet nonce that is still unresolved,
/// when we last warned about it, and whether the node has refused a
/// re-broadcast at it yet.
#[derive(Debug, Clone, Copy)]
struct PendingSince {
    since: Instant,
    last_warned: Option<Instant>,
    refused_once: bool,
}

impl PendingSince {
    fn new(now: Instant) -> Self {
        Self {
            since: now,
            last_warned: None,
            refused_once: false,
        }
    }

    /// Warn once the wait exceeds [`PENDING_WAIT_WARN_AFTER`], then at most
    /// once per further interval.
    fn should_warn(&self, now: Instant) -> bool {
        now.duration_since(self.since) >= PENDING_WAIT_WARN_AFTER
            && self
                .last_warned
                .is_none_or(|last| now.duration_since(last) >= PENDING_WAIT_WARN_AFTER)
    }
}

#[derive(Clone)]
pub struct EthereumBatchPoster {
    provider: DynProvider,
    config: BatchPosterConfig,
    /// Wallet nonces this process has attempted that Latest has not yet
    /// passed. Observability only — nothing reads it to decide a price. Pruned
    /// once Latest passes the nonce; never cleared by an accepted send, so the
    /// clock measures how long the slot has been unresolved, whatever the
    /// cause.
    pending: Arc<Mutex<BTreeMap<u64, PendingSince>>>,
    /// Test-only: next `send_batch_at_nonce` returns this error string without
    /// broadcasting, so tests can drive the error-classification paths.
    #[cfg(test)]
    fail_next_send: Arc<Mutex<Option<String>>>,
}

impl EthereumBatchPoster {
    pub fn new(provider: DynProvider, config: BatchPosterConfig) -> Self {
        Self {
            provider,
            config,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            #[cfg(test)]
            fail_next_send: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_send_with_for_test(&self, message: &str) {
        *self.fail_next_send.lock().expect("fail_next_send lock") = Some(message.to_string());
    }

    #[cfg(test)]
    pub(crate) fn pending_nonces_for_test(&self) -> Vec<u64> {
        self.pending
            .lock()
            .expect("pending lock")
            .keys()
            .copied()
            .collect()
    }

    /// Conservative upper-bound timeout for waiting on confirmations, derived
    /// from the configured block time. Shorter block times on other chains just
    /// make the watch complete sooner.
    fn confirmation_timeout(&self) -> Duration {
        derive_confirmation_timeout(
            self.config.confirmation_depth,
            self.config.seconds_per_block,
        )
    }

    /// Record an attempt at `nonce` (first attempt starts its clock) and warn
    /// if it has been unresolved for too long.
    fn track_pending(&self, nonce: u64) {
        let now = Instant::now();
        let mut pending = self.pending.lock().expect("pending lock");
        let entry = pending
            .entry(nonce)
            .or_insert_with(|| PendingSince::new(now));
        if entry.should_warn(now) {
            entry.last_warned = Some(now);
            warn!(
                tx_nonce = nonce,
                pending_secs = now.duration_since(entry.since).as_secs(),
                "batch tx at this wallet nonce has been unresolved longer than expected (see \
                 the fee-policy note in l1::submitter::poster before changing fee policy)"
            );
        }
    }

    /// Mark a refused re-broadcast at `nonce`; true the first time.
    fn note_refused(&self, nonce: u64) -> bool {
        let mut pending = self.pending.lock().expect("pending lock");
        let entry = pending
            .entry(nonce)
            .or_insert_with(|| PendingSince::new(Instant::now()));
        !std::mem::replace(&mut entry.refused_once, true)
    }

    fn prune_pending_below(&self, latest_nonce: u64) {
        self.pending
            .lock()
            .expect("pending lock")
            .retain(|&nonce, _| nonce >= latest_nonce);
    }

    async fn latest_account_nonce(&self) -> Result<u64, BatchPosterError> {
        self.provider
            .get_transaction_count(self.config.batch_submitter_address)
            .block_id(BlockNumberOrTag::Latest.into())
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))
    }

    async fn send_batch_at_nonce(
        &self,
        payload: Vec<u8>,
        nonce: u64,
        fees: &Eip1559Fees,
    ) -> Result<PendingTransactionBuilder<alloy::network::Ethereum>, BatchPosterError> {
        #[cfg(test)]
        {
            if let Some(message) = self
                .fail_next_send
                .lock()
                .expect("fail_next_send lock")
                .take()
            {
                return Err(BatchPosterError::Provider(message));
            }
        }
        let input_box = InputBox::new(self.config.l1_submit_address, &self.provider);
        let call = input_box
            .addInput(self.config.app_address, payload.into())
            .max_fee_per_gas(fees.max_fee_per_gas)
            .max_priority_fee_per_gas(fees.max_priority_fee_per_gas)
            // Estimate at Latest (pinned explicitly: item 4 of the fee-policy
            // note depends on it) and without the nonce. Anvil applies
            // mempool nonce policy to pending-block `eth_estimateGas` and
            // rejects a re-broadcast at an already-pending nonce with "nonce
            // too low" before it reaches the node (geth skips nonce checks in
            // estimates, so only local/CI runs are affected).
            .block(BlockNumberOrTag::Latest.into());

        // Pin the padded estimate before the send. With gas and both fee
        // fields set, the GasFiller is Finished, so this estimate replaces
        // the filler's rather than adding a round-trip.
        let gas = pad_gas_estimate(
            call.estimate_gas()
                .await
                .map_err(|err| BatchPosterError::Provider(err.to_string()))?,
        );

        call.gas(gas)
            .nonce(nonce)
            .send()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))
    }

    /// Wait serially for each tx to reach `confirmation_depth + 1`
    /// confirmations. Returns `false` on the first timeout: the safe response
    /// is "re-enter `submit_batches` on the next tick", which re-derives the
    /// unresolved suffix from Latest — whatever did land is observed, whatever
    /// did not is re-broadcast — so nothing is skipped by stopping early.
    async fn wait_for_confirmations(&self, tx_hashes: &[TxHash]) -> Result<bool, BatchPosterError> {
        let timeout = self.confirmation_timeout();
        for tx_hash in tx_hashes {
            let watch = PendingTransactionConfig::new(*tx_hash)
                .with_required_confirmations(self.config.confirmation_depth.saturating_add(1))
                .with_timeout(Some(timeout))
                .with_provider(self.provider.root().clone());
            match watch.watch().await {
                Ok(_) => {
                    info!(
                        %tx_hash,
                        confirmation_depth = self.config.confirmation_depth,
                        required_confirmations = self.config.confirmation_depth.saturating_add(1),
                        "batch submission confirmed on L1"
                    );
                }
                Err(PendingTransactionError::TxWatcher(
                    alloy::providers::WatchTxError::Timeout,
                )) => {
                    info!(
                        %tx_hash,
                        confirmation_depth = self.config.confirmation_depth,
                        timeout_secs = timeout.as_secs(),
                        "batch submission not yet confirmed; next tick will re-check under fresher state"
                    );
                    return Ok(false);
                }
                Err(err) => return Err(BatchPosterError::Provider(err.to_string())),
            }
        }

        Ok(true)
    }
}

/// The node already holds our tx at this nonce and this send does not beat
/// it. Two replies mean that: a byte-identical re-send ("already known" on
/// geth/erigon, "already imported" on Anvil/reth, "known transaction" on
/// Besu, "AlreadyKnown" on Nethermind) and a fee-different one that fails the
/// ≥10% rule on either component ("replacement transaction underpriced" on
/// geth/reth/Anvil, capitalized on Besu; "FeeTooLowToCompete" on Nethermind).
/// Matched case-insensitively on the provider's error text.
///
/// A client that words it differently falls back to the generic
/// transient-error path — error log, idle-poll retry, and the tick aborted at
/// that nonce, so newly closed batches behind it wait for it to clear. That
/// is exactly what every rejected re-broadcast did before this classification
/// existed; the classified path is what lets the rest of the suffix go out.
fn is_pending_tx_conflict(err: &str) -> bool {
    let err = err.to_ascii_lowercase();
    err.contains("replacement transaction underpriced")
        || err.contains("feetoolowtocompete")
        || err.contains("already known")
        || err.contains("alreadyknown")
        || err.contains("already imported")
        || err.contains("known transaction")
}

fn derive_confirmation_timeout(confirmation_depth: u64, seconds_per_block: u64) -> Duration {
    let blocks_to_wait = confirmation_depth.saturating_add(1).saturating_mul(2);
    Duration::from_secs(blocks_to_wait.saturating_mul(seconds_per_block))
}

#[async_trait]
impl BatchPoster for EthereumBatchPoster {
    async fn submit_batches(
        &self,
        payloads: Vec<Vec<u8>>,
        watermark: &dyn WalletNonceWatermarkSink,
    ) -> Result<SubmitBatchesOutcome, BatchPosterError> {
        if payloads.is_empty() {
            return Ok(SubmitBatchesOutcome::Submitted(Vec::new()));
        }

        // Keyed-write chain-id gate (review): re-confirm the RPC still serves the
        // pinned chain immediately before any productive send. The submitter is
        // long-lived — its signing provider is built once at spawn and the
        // boot-time / reader chain-id checks are one-shot — so a load-balanced
        // RPC that fails over to another chain mid-life would otherwise burn
        // submitter nonce slots on the wrong chain. Reached only when there is
        // something to send (the empty early-return above), so idle ticks add no
        // RPC load. A mismatch is terminal (lifted out of the transient bucket by
        // the submitter run-loop); a transient RPC error retries like any blip.
        let rpc_chain_id = self
            .provider
            .get_chain_id()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))?;
        if rpc_chain_id != self.config.expected_chain_id {
            return Err(BatchPosterError::ChainIdMismatch {
                rpc: rpc_chain_id,
                expected: self.config.expected_chain_id,
            });
        }

        // This tick's market price, used as-is for every send (see the
        // fee-policy note at the top of this file).
        let fees = estimate_fees(&self.provider)
            .await
            .map_err(BatchPosterError::Provider)?;
        let mut next_nonce = self.latest_account_nonce().await?;
        self.prune_pending_below(next_nonce);

        // Write-before-broadcast (R1a): durably cover every nonce this
        // tick will use before the first send. One raise to the highest
        // covers the whole consecutive range.
        let highest_nonce = next_nonce.saturating_add(payloads.len() as u64 - 1);
        watermark
            .raise_to(highest_nonce)
            .map_err(BatchPosterError::Provider)?;

        let mut broadcast = Vec::with_capacity(payloads.len());
        let mut any_refused = false;
        // A refusal before any broadcast this tick means every broadcast sits
        // behind an occupied slot and cannot mine yet; watching would only
        // burn a confirmation timeout.
        let mut head_refused = false;

        for payload in payloads {
            self.track_pending(next_nonce);
            match self.send_batch_at_nonce(payload, next_nonce, &fees).await {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    debug!(
                        tx_nonce = next_nonce,
                        %tx_hash,
                        max_fee_per_gas = fees.max_fee_per_gas,
                        max_priority_fee_per_gas = fees.max_priority_fee_per_gas,
                        confirmation_depth = self.config.confirmation_depth,
                        "sent batch submission tx to L1"
                    );
                    broadcast.push(tx_hash);
                }
                Err(BatchPosterError::Provider(ref msg)) if is_pending_tx_conflict(msg) => {
                    // The mempool already holds our tx at this nonce and this
                    // tick's estimate does not beat it. Not an error: keep
                    // going so a longer suffix still gets its fresh sends, and
                    // let the worker sleep before the next estimate.
                    if self.note_refused(next_nonce) {
                        info!(
                            tx_nonce = next_nonce,
                            reason = %msg,
                            "batch tx already in the mempool at a price this estimate cannot beat; waiting on the fee market"
                        );
                    } else {
                        debug!(tx_nonce = next_nonce, reason = %msg, "still waiting on the fee market");
                    }
                    any_refused = true;
                    head_refused |= broadcast.is_empty();
                }
                Err(err) => return Err(err),
            }
            next_nonce = next_nonce.saturating_add(1);
        }

        let confirmed = if head_refused {
            false
        } else {
            self.wait_for_confirmations(broadcast.as_slice()).await?
        };
        Ok(if any_refused || !confirmed {
            SubmitBatchesOutcome::Waiting { broadcast }
        } else {
            SubmitBatchesOutcome::Submitted(broadcast)
        })
    }

    async fn observed_submitted_batch_nonces(
        &self,
        from_block: u64,
    ) -> Result<Vec<u64>, BatchPosterError> {
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(|err| BatchPosterError::Provider(err.to_string()))?;
        let start_block = from_block.max(self.config.start_block);
        if start_block > latest {
            return Ok(Vec::new());
        }

        // Ordered fetch: `advance_expected_batch_nonce` folds these nonces
        // assuming L1 event order, so a raw `eth_getLogs` reorder would
        // under-advance the frontier and resubmit an already-mined suffix
        // (wasted gas + InputBox noise). The `_ordered` helper guarantees the
        // canonical (block, tx_index, log_index) order — the same the reader
        // relies on for its contiguity check.
        let events = get_input_added_events_ordered(
            &self.provider,
            self.config.app_address,
            &self.config.l1_submit_address,
            start_block,
            latest,
            self.config.long_block_range_error_codes.as_slice(),
        )
        .await
        .map_err(|err| BatchPosterError::Provider(format!("get_input_added_events: {err}")))?;

        let mut observed_nonces = Vec::new();
        for (event, _log) in events {
            let evm_advance = decode_evm_advance_input(event.input.as_ref()).map_err(|err| {
                BatchPosterError::Provider(format!(
                    "decode EvmAdvance for InputAdded index {}: {err}",
                    event.index
                ))
            })?;
            if evm_advance.msgSender != self.config.batch_submitter_address {
                continue;
            }
            let batch: Batch = ssz::Decode::from_ssz_bytes(evm_advance.payload.as_ref())
                .map_err(|err| BatchPosterError::Provider(format!("{err:?}")))?;
            observed_nonces.push(batch.nonce);
        }

        Ok(observed_nonces)
    }
}

#[cfg(test)]
pub(crate) mod mock {
    use super::{Batch, BatchPoster, BatchPosterError, SubmitBatchesOutcome, TxHash};
    use crate::l1::watermark::WalletNonceWatermarkSink;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Debug)]
    pub struct MockBatchPoster {
        pub submissions: Mutex<Vec<(u64, usize)>>,
        pub observed_submitted_nonces: Mutex<Vec<u64>>,
        pub observed_submitted_error: Mutex<Option<String>>,
        pub last_from_block: Mutex<Option<u64>>,
        /// `Some(k)`: the first `k` payloads are broadcast, the rest are
        /// reported as already held by the mempool, and the outcome is
        /// `Waiting`. `None`: everything is broadcast and confirmed.
        pub waiting_after: Mutex<Option<usize>>,
    }

    impl MockBatchPoster {
        pub fn new() -> Self {
            Self {
                submissions: Mutex::new(Vec::new()),
                observed_submitted_nonces: Mutex::new(Vec::new()),
                observed_submitted_error: Mutex::new(None),
                last_from_block: Mutex::new(None),
                waiting_after: Mutex::new(None),
            }
        }

        pub fn submissions(&self) -> Vec<(u64, usize)> {
            self.submissions.lock().expect("lock").clone()
        }

        pub fn set_observed_submitted_nonces(&self, value: Vec<u64>) {
            *self.observed_submitted_nonces.lock().expect("lock") = value;
        }

        pub fn set_observed_submitted_error(&self, value: Option<&str>) {
            *self.observed_submitted_error.lock().expect("lock") = value.map(str::to_string);
        }

        pub fn last_from_block(&self) -> Option<u64> {
            *self.last_from_block.lock().expect("lock")
        }

        /// Report every payload as already held by the mempool.
        pub fn set_waiting(&self, waiting: bool) {
            *self.waiting_after.lock().expect("lock") = waiting.then_some(0);
        }

        /// Broadcast the first `broadcast` payloads, report the rest as held.
        pub fn set_waiting_after(&self, broadcast: usize) {
            *self.waiting_after.lock().expect("lock") = Some(broadcast);
        }
    }

    #[async_trait]
    impl BatchPoster for MockBatchPoster {
        async fn submit_batches(
            &self,
            payloads: Vec<Vec<u8>>,
            _watermark: &dyn WalletNonceWatermarkSink,
        ) -> Result<SubmitBatchesOutcome, BatchPosterError> {
            let waiting_after = *self.waiting_after.lock().expect("lock");
            let mut tx_hashes = Vec::with_capacity(payloads.len());
            for (index, payload) in payloads.into_iter().enumerate() {
                let batch_index = ssz::Decode::from_ssz_bytes(payload.as_ref())
                    .map(|b: Batch| b.nonce)
                    .unwrap_or(0);
                self.submissions
                    .lock()
                    .expect("lock")
                    .push((batch_index, payload.len()));
                if waiting_after.is_none_or(|k| index < k) {
                    tx_hashes.push(TxHash::ZERO);
                }
            }
            if waiting_after.is_some() {
                Ok(SubmitBatchesOutcome::Waiting {
                    broadcast: tx_hashes,
                })
            } else {
                Ok(SubmitBatchesOutcome::Submitted(tx_hashes))
            }
        }

        async fn observed_submitted_batch_nonces(
            &self,
            from_block: u64,
        ) -> Result<Vec<u64>, BatchPosterError> {
            *self.last_from_block.lock().expect("lock") = Some(from_block);
            if let Some(err) = self.observed_submitted_error.lock().expect("lock").clone() {
                return Err(BatchPosterError::Provider(err));
            }
            let configured = self.observed_submitted_nonces.lock().expect("lock").clone();
            if !configured.is_empty() {
                return Ok(configured);
            }
            Ok(self
                .submissions
                .lock()
                .expect("lock")
                .iter()
                .map(|(idx, _)| *idx)
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::{
        BatchPoster, BatchPosterConfig, BatchPosterError, EthereumBatchPoster,
        PENDING_WAIT_WARN_AFTER, PendingSince, SubmitBatchesOutcome, derive_confirmation_timeout,
        is_pending_tx_conflict, mock::MockBatchPoster,
    };
    use crate::l1::watermark::WalletNonceWatermarkSink;
    use alloy::node_bindings::Anvil;
    use alloy::providers::Provider;
    use alloy::rpc::types::BlockNumberOrTag;

    /// Hashes broadcast this tick, whichever outcome carried them.
    fn broadcast_hashes(outcome: SubmitBatchesOutcome) -> Vec<super::TxHash> {
        match outcome {
            SubmitBatchesOutcome::Submitted(hashes) => hashes,
            SubmitBatchesOutcome::Waiting { broadcast } => broadcast,
        }
    }

    fn require_anvil() {
        assert!(
            std::process::Command::new("anvil")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok(),
            "anvil not found on PATH — install Foundry (https://getfoundry.sh)"
        );
    }

    /// A watermark sink that records every `raise_to` call and (optionally)
    /// fails, so a test can observe whether the raise happened — and in what
    /// order relative to the first send.
    struct RecordingWatermarkSink {
        calls: Mutex<Vec<u64>>,
        fail: bool,
    }

    impl RecordingWatermarkSink {
        fn failing() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: true,
            }
        }

        fn passing() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: false,
            }
        }

        fn calls(&self) -> Vec<u64> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl WalletNonceWatermarkSink for RecordingWatermarkSink {
        fn raise_to(&self, highest: u64) -> Result<(), String> {
            self.calls.lock().expect("lock").push(highest);
            if self.fail {
                Err("recording sink: forced failure".to_string())
            } else {
                Ok(())
            }
        }
    }

    const SUBMITTER_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const SUBMITTER: alloy_primitives::Address =
        alloy_primitives::address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    fn poster_config(anvil: &alloy::node_bindings::AnvilInstance) -> BatchPosterConfig {
        BatchPosterConfig {
            l1_submit_address: alloy_primitives::Address::repeat_byte(0x11),
            app_address: alloy_primitives::Address::repeat_byte(0x22),
            batch_submitter_address: SUBMITTER,
            start_block: 0,
            // confirmation_depth 0 → watch timeout is 2 * seconds_per_block;
            // keep it short so --no-mining ticks return promptly on timeout.
            confirmation_depth: 0,
            seconds_per_block: 1,
            long_block_range_error_codes: vec![],
            expected_chain_id: anvil.chain_id(),
        }
    }

    /// R1a write-before-broadcast: `submit_batches` must raise the watermark to
    /// cover the whole consecutive nonce range *before* the first send. We lock
    /// it with a sink that fails on `raise_to`: a correct poster aborts the tick
    /// before broadcasting anything, so the submitter's pending nonce is
    /// unchanged. If `raise_to` were moved after the first `addInput` send
    /// (re-opening the F1 zombie-tx hole), that send would bump the pending
    /// nonce and this test would go red. Also pins the raise count (once) and
    /// value (`base + payloads.len() - 1`). (Mutation-checked: moving the raise
    /// after the send loop fails this test.)
    #[tokio::test]
    async fn submit_batches_raises_watermark_before_any_send() {
        require_anvil();
        let anvil = Anvil::default().spawn();
        let provider =
            crate::l1::provider::create_signer_provider(&anvil.endpoint(), SUBMITTER_KEY, false)
                .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));

        let base_nonce = provider
            .get_transaction_count(SUBMITTER)
            .await
            .expect("base nonce");
        let sink = RecordingWatermarkSink::failing();
        let payloads = vec![vec![0u8; 4], vec![1u8; 4], vec![2u8; 4]]; // 3 consecutive nonces

        let result = poster.submit_batches(payloads, &sink).await;

        assert!(
            matches!(result, Err(BatchPosterError::Provider(_))),
            "a failing watermark sink must abort submit_batches, got {result:?}"
        );
        // (a) raised exactly once, (b) to the highest nonce of the range.
        assert_eq!(
            sink.calls(),
            vec![base_nonce + 2],
            "raise_to must be called once with base_nonce + payloads.len() - 1"
        );
        // (c) before any send — no tx broadcast, so pending nonce is unchanged.
        let pending = provider
            .get_transaction_count(SUBMITTER)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending nonce");
        assert_eq!(
            pending, base_nonce,
            "raise_to must run before any send; a broadcast would have bumped the pending nonce"
        );
    }

    /// Keyed-write chain-id gate: a long-lived submitter pointed at an RPC that
    /// serves a different chain than the pinned one must refuse to submit, before
    /// any productive work — no watermark raise, no broadcast. Anvil's chain id
    /// is 31337; we pin a different one and assert the `ChainIdMismatch` refusal
    /// fires ahead of the (would-otherwise-fail-later) watermark raise.
    #[tokio::test]
    async fn submit_batches_refuses_on_wrong_chain_before_any_work() {
        require_anvil();
        let anvil = Anvil::default().spawn();
        let provider =
            crate::l1::provider::create_signer_provider(&anvil.endpoint(), SUBMITTER_KEY, false)
                .expect("signer provider");

        let wrong_chain_id = anvil.chain_id() + 1;
        let config = BatchPosterConfig {
            expected_chain_id: wrong_chain_id,
            ..poster_config(&anvil)
        };
        let poster = EthereumBatchPoster::new(provider.clone(), config);

        let base_nonce = provider
            .get_transaction_count(SUBMITTER)
            .await
            .expect("base nonce");
        // A sink that would *succeed* — so the only thing that can stop a send is
        // the chain-id gate, not the watermark guard. (Recording proves the gate
        // fires first: a passing chain check would reach `raise_to`.)
        let sink = RecordingWatermarkSink::passing();
        let payloads = vec![vec![0u8; 4], vec![1u8; 4]];

        let result = poster.submit_batches(payloads, &sink).await;

        assert!(
            matches!(
                result,
                Err(BatchPosterError::ChainIdMismatch { rpc, expected })
                    if rpc == anvil.chain_id() && expected == wrong_chain_id
            ),
            "wrong-chain RPC must abort submit_batches with ChainIdMismatch, got {result:?}"
        );
        assert!(
            sink.calls().is_empty(),
            "chain-id gate must fire before the watermark raise (no raise_to call)"
        );
        let pending = provider
            .get_transaction_count(SUBMITTER)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending nonce");
        assert_eq!(
            pending, base_nonce,
            "no tx may be broadcast on the wrong chain"
        );
    }

    #[tokio::test]
    async fn mock_poster_tracks_requested_suffix_start_block() {
        let poster = MockBatchPoster::new();
        let observed = poster
            .observed_submitted_batch_nonces(42)
            .await
            .expect("observe submitted batches");

        assert!(observed.is_empty());
        assert_eq!(poster.last_from_block(), Some(42));
    }

    #[test]
    fn confirmation_timeout_derives_from_seconds_per_block() {
        assert_eq!(derive_confirmation_timeout(2, 12), Duration::from_secs(72));
        assert_eq!(derive_confirmation_timeout(2, 1), Duration::from_secs(6));
        assert_eq!(derive_confirmation_timeout(5, 3), Duration::from_secs(36));
    }

    /// The negative half is the load-bearing half: "nonce too low" must stay a
    /// hard error, or a regression to an estimate-with-pending-nonce would be
    /// silently reclassified as a quiet, permanent `Waiting`.
    #[test]
    fn pending_tx_conflict_matches_client_wordings_case_insensitively() {
        for reply in [
            "server returned an error response: error code -32000: replacement transaction underpriced",
            "Replacement transaction underpriced", // Besu
            "FeeTooLowToCompete",                  // Nethermind (replacement)
            "already known",                       // geth / erigon
            "Already Imported",                    // Anvil / reth
            "Known transaction",                   // Besu
            "AlreadyKnown",                        // Nethermind (identical)
        ] {
            assert!(is_pending_tx_conflict(reply), "must classify: {reply}");
        }
        for reply in [
            "nonce too low",
            "insufficient funds for gas * price + value",
            "max priority fee per gas higher than max fee per gas",
            "test-injected send failure",
        ] {
            assert!(!is_pending_tx_conflict(reply), "must not classify: {reply}");
        }
    }

    #[test]
    fn pending_since_warns_after_threshold_and_then_once_per_interval() {
        let t0 = Instant::now();
        let mut pending = PendingSince::new(t0);
        let just_under = PENDING_WAIT_WARN_AFTER - Duration::from_secs(1);

        assert!(!pending.should_warn(t0));
        assert!(!pending.should_warn(t0 + just_under));
        assert!(pending.should_warn(t0 + PENDING_WAIT_WARN_AFTER));

        pending.last_warned = Some(t0 + PENDING_WAIT_WARN_AFTER);
        assert!(!pending.should_warn(t0 + PENDING_WAIT_WARN_AFTER + just_under));
        assert!(pending.should_warn(t0 + 2 * PENDING_WAIT_WARN_AFTER));
    }

    /// A broadcast that has not confirmed by the end of the tick is `Waiting`,
    /// and the re-broadcast of that still-pending tx on a flat market is
    /// byte-identical (same fees, same Latest-state gas estimate, deterministic
    /// signature), so the node answers "already imported" (geth: "already
    /// known") — also `Waiting`, not an error: nothing is re-sent, nothing is
    /// raised, and the original lands once mining resumes.
    ///
    /// This test also pins the nonce-free gas estimate: with an estimate at the
    /// pending nonce, Anvil answers "nonce too low", which is deliberately not
    /// classified as a conflict, so the second submit would fail instead.
    #[tokio::test]
    async fn submit_batches_waits_when_mempool_already_holds_the_tx() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let provider =
            crate::l1::provider::create_signer_provider(&anvil.endpoint(), SUBMITTER_KEY, false)
                .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(SUBMITTER)
            .await
            .expect("base nonce");

        // 1) Broadcast parks in the mempool; the confirmation watch times out.
        let first = poster
            .submit_batches(vec![vec![0u8; 4]], &sink)
            .await
            .expect("first submit parks a pending tx");
        let SubmitBatchesOutcome::Waiting { broadcast: first } = first else {
            panic!("an unconfirmed broadcast must be Waiting, got {first:?}");
        };
        assert_eq!(first.len(), 1);

        // 2) Next tick re-estimates (unchanged under --no-mining) and
        //    re-broadcasts the identical tx.
        let outcome = poster
            .submit_batches(vec![vec![0u8; 4]], &sink)
            .await
            .expect("a mempool conflict is not an error");
        assert_eq!(
            outcome,
            SubmitBatchesOutcome::Waiting {
                broadcast: Vec::new()
            }
        );
        assert_eq!(poster.pending_nonces_for_test(), vec![base_nonce]);

        let pending = provider
            .get_transaction_count(SUBMITTER)
            .block_id(BlockNumberOrTag::Pending.into())
            .await
            .expect("pending nonce");
        assert_eq!(pending, base_nonce + 1, "still exactly one pending slot");

        // 3) Mining resumes: the original lands; nothing replaced it.
        let _: serde_json::Value = provider
            .raw_request("evm_mine".into(), ())
            .await
            .expect("mine");
        assert!(
            provider
                .get_transaction_receipt(first[0])
                .await
                .expect("receipt rpc")
                .is_some(),
            "the original tx lands; nothing replaced it"
        );
    }

    /// Under automine every broadcast confirms within the tick, so the outcome
    /// is `Submitted` and the resolved nonce is forgotten on the next tick.
    #[tokio::test]
    async fn submit_batches_reports_submitted_once_confirmed() {
        require_anvil();
        let anvil = Anvil::default().timeout(30_000).spawn();
        let provider =
            crate::l1::provider::create_signer_provider(&anvil.endpoint(), SUBMITTER_KEY, false)
                .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(SUBMITTER)
            .await
            .expect("base nonce");

        let outcome = poster
            .submit_batches(vec![vec![0u8; 4], vec![1u8; 4]], &sink)
            .await
            .expect("automined submit");
        assert!(
            matches!(outcome, SubmitBatchesOutcome::Submitted(ref h) if h.len() == 2),
            "confirmed broadcasts are Submitted, got {outcome:?}"
        );
        assert_eq!(
            poster.pending_nonces_for_test(),
            vec![base_nonce, base_nonce + 1],
            "attempted nonces stay tracked until the next tick sees Latest pass them"
        );

        let outcome = poster
            .submit_batches(vec![vec![2u8; 4]], &sink)
            .await
            .expect("next automined submit");
        assert!(matches!(outcome, SubmitBatchesOutcome::Submitted(ref h) if h.len() == 1));
        assert_eq!(poster.pending_nonces_for_test(), vec![base_nonce + 2]);
    }

    /// An underpriced-replacement rejection is the other reply that means "the
    /// mempool already holds our tx"; it is classified the same way.
    #[tokio::test]
    async fn submit_batches_waits_on_underpriced_replacement_rejection() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let provider =
            crate::l1::provider::create_signer_provider(&anvil.endpoint(), SUBMITTER_KEY, false)
                .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();
        let base_nonce = provider
            .get_transaction_count(SUBMITTER)
            .await
            .expect("base nonce");

        poster.fail_next_send_with_for_test(
            "server returned an error response: error code -32000: Replacement transaction underpriced",
        );
        let outcome = poster
            .submit_batches(vec![vec![0u8; 4]], &sink)
            .await
            .expect("an underpriced rejection is not an error");
        assert_eq!(
            outcome,
            SubmitBatchesOutcome::Waiting {
                broadcast: Vec::new()
            }
        );
        assert_eq!(poster.pending_nonces_for_test(), vec![base_nonce]);
    }

    /// Any other send failure is still a transient provider error for the
    /// worker to log and retry.
    #[tokio::test]
    async fn submit_batches_still_errors_on_other_send_failures() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let provider =
            crate::l1::provider::create_signer_provider(&anvil.endpoint(), SUBMITTER_KEY, false)
                .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        poster.fail_next_send_with_for_test("test-injected send failure");
        let result = poster.submit_batches(vec![vec![0u8; 4]], &sink).await;
        assert!(
            matches!(result, Err(BatchPosterError::Provider(ref msg)) if msg.contains("test-injected")),
            "unclassified send failures must surface, got {result:?}"
        );
    }

    /// A conflict at one nonce does not abort the tick: later payloads that
    /// are new still get their fresh sends (at-least-once re-broadcast of the
    /// whole suffix), the outcome reports only the hashes that went out, and
    /// a tick after inclusion forgets the resolved nonces.
    #[tokio::test]
    async fn submit_batches_keeps_sending_new_payloads_past_a_waiting_nonce() {
        require_anvil();
        let anvil = Anvil::default().arg("--no-mining").timeout(30_000).spawn();
        let provider =
            crate::l1::provider::create_signer_provider(&anvil.endpoint(), SUBMITTER_KEY, false)
                .expect("signer provider");
        let poster = EthereumBatchPoster::new(provider.clone(), poster_config(&anvil));
        let sink = RecordingWatermarkSink::passing();

        let base_nonce = provider
            .get_transaction_count(SUBMITTER)
            .await
            .expect("base nonce");
        let suffix = vec![vec![0u8; 4], vec![1u8; 4], vec![2u8; 4]];

        let first = broadcast_hashes(
            poster
                .submit_batches(suffix.clone(), &sink)
                .await
                .expect("first multi-nonce submit"),
        );
        assert_eq!(first.len(), 3);

        // Next tick: the same three are still pending (all conflicts) and a
        // fourth batch has closed since.
        let mut longer = suffix;
        longer.push(vec![3u8; 4]);
        let started = Instant::now();
        let outcome = poster
            .submit_batches(longer, &sink)
            .await
            .expect("conflicts on the pending prefix must not abort the tick");
        let SubmitBatchesOutcome::Waiting { broadcast } = outcome else {
            panic!("expected Waiting, got {outcome:?}");
        };
        assert_eq!(
            broadcast.len(),
            1,
            "only the new fourth payload is broadcast"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a broadcast behind a refused head must not be watched for the full timeout"
        );
        assert_eq!(
            poster.pending_nonces_for_test(),
            (0..4).map(|i| base_nonce + i).collect::<Vec<_>>()
        );

        let _: serde_json::Value = provider
            .raw_request("evm_mine".into(), ())
            .await
            .expect("mine");
        let latest = provider
            .get_transaction_count(SUBMITTER)
            .block_id(BlockNumberOrTag::Latest.into())
            .await
            .expect("latest");
        assert_eq!(latest, base_nonce + 4, "all four land in nonce order");

        // A tick after inclusion prunes the resolved nonces.
        let outcome = poster
            .submit_batches(vec![vec![4u8; 4]], &sink)
            .await
            .expect("fresh send after inclusion");
        assert!(
            matches!(outcome, SubmitBatchesOutcome::Waiting { ref broadcast } if broadcast.len() == 1)
        );
        assert_eq!(poster.pending_nonces_for_test(), vec![base_nonce + 4]);
    }
}
