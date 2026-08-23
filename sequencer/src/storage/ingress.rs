// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Inclusion-lane writer: opens the initial batch/frame, appends user-op chunks,
//! and rotates frame/batch boundaries on the hot path.
//!
//! The lane also reads `safe_inputs` (executed by the application) and the open
//! state (resumed on startup) — those reads live here too because they're driven
//! by the lane's flow, not by an L1 ingress event.

use std::path::Path;

use alloy_primitives::Address;
use rusqlite::{Result, Transaction, params};

use super::convert::{
    external_u64_to_i64, from_unix_ms, i64_to_u64, now_unix_ms, saturating_query_bound, to_unix_ms,
    u64_to_i64,
};
use super::history::{ExecutedInputMapping, attach_executed_inputs_in, safe_input_floor_in};
use super::mutations::{
    insert_new_batch, insert_open_frame, persist_frame_direct_sequence,
    persist_frame_direct_sequence_derived, persist_frame_direct_sequence_physical_only, seal_batch,
};
use super::queries::{
    current_safe_block_required, load_current_write_head, query_batch_policy,
    query_latest_safe_input_index_exclusive, valid_ordered_l2_tx_head,
};
use super::safe_accepted_batches::canonical_divergence_in;
use super::snapshot_dumps::{insert_pending_dump_in, promote_finalized_in};
use super::{
    BatchPolicy, DirectInputExecution, ExecutedInputCount, SafeFrontierState, SafeInputFrontier,
    SafeInputRange, Storage, StoredSafeInput, WriteHead,
};
use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp};

impl Storage {
    /// Cursor for the next safe input to drain into a frame. Takes the maximum
    /// of the era's durable drain floor and the highest already-drained
    /// `safe_input_index` from valid (non-invalidated) `sequenced_l2_txs` rows
    /// plus one.
    ///
    /// Using `MAX + 1` instead of `COUNT(*)` makes this robust against gaps:
    /// when a batch is invalidated, those rows drop out of the view and the
    /// cursor naturally rewinds, allowing the recovery batch to re-drain only
    /// the invalidated suffix. The durable floor prevents a standard recovery
    /// from crossing back into cockroach-root padding already represented by
    /// the recovered application snapshot.
    pub fn next_undrained_safe_input_index(&mut self) -> Result<u64> {
        self.read(next_undrained_safe_input_index_in)
    }

    /// Resume the lane on startup. Returns `None` if storage is empty (caller
    /// should follow up with [`Storage::initialize_open_state`]).
    pub fn open_state(&mut self) -> Result<Option<WriteHead>> {
        self.read(load_current_write_head)
    }

    /// Bootstrap the very first batch + frame with explicit values, returning
    /// its loaded [`WriteHead`]. Asserts no open state exists.
    ///
    /// Production opens the genesis Tip through the reducer's guarded
    /// `EnsureOpenTip` phase, which derives `safe_block`/leading range from the
    /// synced L1 view; this explicit form is kept for tests that seed a
    /// specific open state without a safe-head observation.
    #[cfg(test)]
    pub fn initialize_open_state(
        &mut self,
        safe_block: u64,
        leading_direct_range: SafeInputRange,
    ) -> Result<WriteHead> {
        self.write(|tx| {
            assert!(
                load_current_write_head(tx)?.is_none(),
                "open state already exists"
            );
            let batch_index = insert_tip_rows(tx, Some(0), None, safe_block)?;
            persist_frame_direct_sequence_physical_only(tx, batch_index, 0, leading_direct_range)?;
            Ok(load_current_write_head(tx)?.expect("genesis tip just inserted"))
        })
    }

    /// Ensure a valid open Tip exists. Establishes the tip-existence
    /// invariant; does **not** return the head — callers that need it load it
    /// via [`Storage::open_state`], so [`WriteHead`] has a single constructor
    /// (`load_current_write_head`) rather than a hand-built twin.
    ///
    /// No-op on a warm DB (or after a recovery cascade already reopened the
    /// Tip). On a genuinely fresh DB it opens the genesis Tip via the shared
    /// `open_fresh_tip_in_tx` mechanism: index 0 / nonce 0 at the current
    /// safe head, the leading safe-input range **sequenced but not executed**.
    /// Those directs are executed by the lane's catch-up replay (the same path
    /// warm resume and recovery batches use), so there is no cold-start drain.
    ///
    /// This unguarded public form exists for test harnesses. Production uses
    /// `ensure_open_tip_for_recovery`, which reasserts the reducer facts in its
    /// write transaction. The lane only ever *loads* a Tip (fail-loud if
    /// absent); Cascade owns its own atomic reopen using the same mechanism.
    ///
    /// **Precondition (genesis branch only):** a safe-head observation must
    /// exist. A fresh DB requires L1 at bootstrap plus a successful recovery
    /// sync, else startup refuses before reaching here; the warm-DB early
    /// return fires before any safe-head read.
    pub fn ensure_open_tip(&mut self) -> Result<()> {
        self.write(|tx| {
            if load_current_write_head(tx)?.is_some() {
                return Ok(());
            }
            open_fresh_tip_in_tx(tx)
        })
    }

    /// Open the cockroach-recovery root tip: a fresh anchored tip (`batch_index`
    /// 0, parentless, nonce = the batch-tree anchor `N'`) whose first frame sits
    /// at the checkpoint stop block `C` and leads exactly the directs the fold
    /// folded into `S'` — those with `block_number <= C`.
    ///
    /// Distinct from [`Storage::ensure_open_tip`] / [`open_fresh_tip_in_tx`],
    /// which drain the **whole** synced table at the live safe head. That is
    /// correct for genesis, but wrong here: `setup --recovery` resyncs to the
    /// live safe head `H1`, which is normally **past** `C`, and the fold only
    /// folded `<= C` into `S'`. Draining `(C, H1]` into this tip would sequence
    /// those directs as "already executed" (advancing the cursor / snapshot
    /// `l2_tx_index` past them) even though `S'` never executed them — they would
    /// then be skipped by catch-up and never re-led, vanishing from local state
    /// while the scheduler drains them on-chain (divergence). Capping the drain
    /// at `C` leaves `(C, H1]` undrained, so `run`'s lane leads and executes them
    /// exactly once as the safe frontier advances `C -> H1`.
    pub(crate) fn open_recovery_tip(&mut self, stop_block: u64) -> Result<()> {
        external_u64_to_i64(stop_block, "recovery checkpoint block")?;
        self.write(|tx| open_recovery_tip_in_tx(tx, stop_block))
    }

    /// Snapshot the current L1 reconciliation state. A canonical-divergence
    /// marker outranks and withholds the otherwise-usable frontier.
    ///
    /// **Precondition:** at least one safe-head observation must have been
    /// recorded. The lane only starts after the recovery reducer admits,
    /// which guarantees this in production.
    pub fn safe_frontier_state(&mut self) -> Result<SafeFrontierState> {
        self.read(|tx| {
            if let Some((nonce, safe_input_index)) = canonical_divergence_in(tx)? {
                return Ok(SafeFrontierState::CanonicalDivergence {
                    nonce,
                    safe_input_index,
                });
            }
            Ok(SafeFrontierState::Open(SafeInputFrontier {
                safe_block: current_safe_block_required(tx)?,
                end_exclusive: query_latest_safe_input_index_exclusive(tx)?,
            }))
        })
    }

    /// Replace `out`'s contents with the safe-input rows in `range`. Asserts
    /// contiguity — gaps in `safe_input_index` are a bug, not a runtime
    /// condition.
    pub fn fill_safe_inputs(
        &mut self,
        range: SafeInputRange,
        out: &mut Vec<StoredSafeInput>,
    ) -> Result<()> {
        out.clear();
        if range.is_empty() {
            return Ok(());
        }

        const SQL: &str = "
            SELECT safe_input_index, sender, payload, block_number
            FROM safe_inputs
            WHERE safe_input_index >= ?1 AND safe_input_index < ?2
            ORDER BY safe_input_index ASC
        ";
        let mut stmt = self.conn.prepare_cached(SQL)?;
        let rows = stmt.query_map(
            params![u64_to_i64(range.start()), u64_to_i64(range.end())],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;

        let mut fetched_count = 0_u64;
        for (offset, row) in rows.enumerate() {
            let (index_i64, sender, payload, block_number_i64) = row?;
            let index = i64_to_u64(index_i64);
            let offset = u64::try_from(offset)
                .expect("safe-input result offset exceeds u64: contract-impossible");
            let expected = range
                .start()
                .checked_add(offset)
                .expect("safe-input expected index overflow: contract-impossible");

            assert_eq!(
                index, expected,
                "non-contiguous safe-input index: expected {expected}, found {index}"
            );

            out.push(StoredSafeInput {
                sender: Address::from_slice(sender.as_slice()),
                payload,
                block_number: i64_to_u64(block_number_i64),
            });
            fetched_count = fetched_count
                .checked_add(1)
                .expect("safe-input fetched count overflow: contract-impossible");
        }

        let fetched_end = range
            .start()
            .checked_add(fetched_count)
            .expect("safe-input fetched range overflow: contract-impossible");
        assert_eq!(
            fetched_end,
            range.end(),
            "safe-input range {range:?} not fully populated"
        );

        Ok(())
    }

    /// Persist a chunk of user ops into the open frame and bump `head`'s
    /// counters.
    ///
    /// `head` is trusted as a coherent cache: SQLite is durable authority, and
    /// the lane is the only writer of open-frame state. A stale `WriteHead`
    /// therefore indicates a bug in the lane, not a runtime condition. The
    /// schema's FK + PK constraints catch the dangerous failure modes (write
    /// to a non-existent frame, duplicate `pos_in_frame`) by failing the
    /// INSERT.
    pub(crate) fn append_executed_user_ops_chunk(
        &mut self,
        head: &mut WriteHead,
        user_ops: &[IncludedUserOp],
    ) -> Result<()> {
        if user_ops.is_empty() {
            return Ok(());
        }
        // Validate both in-memory counter advances before the transaction.
        // Otherwise an overflow panic after commit would leave durable rows
        // that the unchanged `WriteHead` cannot describe.
        let mut next_head = *head;
        next_head.increment_batch_user_op_count(user_ops.len());
        self.write(|tx| {
            insert_executed_user_ops_batch(
                tx,
                head.batch_index,
                head.frame_in_batch,
                head.open_frame_user_op_count,
                user_ops,
            )
        })?;
        *head = next_head;
        Ok(())
    }

    /// Physical-only fixture writer. Production must use
    /// [`Storage::append_executed_user_ops_chunk`] so creation and canonical
    /// execution attribution commit atomically.
    #[cfg(test)]
    pub fn append_user_ops_chunk(
        &mut self,
        head: &mut WriteHead,
        user_ops: &[PendingUserOp],
    ) -> Result<()> {
        if user_ops.is_empty() {
            return Ok(());
        }
        let mut next_head = *head;
        next_head.increment_batch_user_op_count(user_ops.len());
        self.write(|tx| {
            insert_user_ops_batch(
                tx,
                head.batch_index,
                head.frame_in_batch,
                head.open_frame_user_op_count,
                user_ops,
            )
        })?;
        *head = next_head;
        Ok(())
    }

    /// Rotate to the next frame inside the same batch. Used when the safe
    /// block advances but batch policy hasn't triggered a batch close — the
    /// new frame inherits the batch and gets a fresh fee/safe-block.
    pub fn close_frame_only_with_executions(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        leading_direct_range: SafeInputRange,
        executions: &[DirectInputExecution],
    ) -> Result<()> {
        let policy = self.write(|tx| {
            close_frame_in(tx, head, next_safe_block, leading_direct_range, executions)
        })?;
        head.advance_frame(policy, next_safe_block);
        Ok(())
    }

    /// Physical-only fixture frame rotation. Production must supply explicit
    /// execution attributions through
    /// [`Storage::close_frame_only_with_executions`].
    #[cfg(test)]
    pub fn close_frame_only(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        leading_direct_range: SafeInputRange,
    ) -> Result<()> {
        let policy = self.write(|tx| {
            close_frame_physical_only_in(tx, head, next_safe_block, leading_direct_range)
        })?;
        head.advance_frame(policy, next_safe_block);
        Ok(())
    }

    /// Like [`Storage::close_frame_only`], but in the **same transaction** also
    /// promotes the snapshot for `max_nonce` (which landed at `inclusion_block`)
    /// to finalized.
    ///
    /// Used when a safe-frontier advance observed one of our batches landing on
    /// L1. The promotion and the drain advance it derives from commit
    /// atomically, so a crash can never leave a promoted-but-undrained batch —
    /// the state where a restart re-processes the safe input, re-derives the
    /// accepted nonce, and re-promotes on a now-deleted pending row
    /// (`QueryReturnedNoRows`, a fail-loud wedge).
    pub fn close_frame_only_promoting_with_executions(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        leading_direct_range: SafeInputRange,
        executions: &[DirectInputExecution],
        max_nonce: u64,
        inclusion_block: u64,
    ) -> Result<()> {
        let policy = self.write(|tx| {
            let policy =
                close_frame_in(tx, head, next_safe_block, leading_direct_range, executions)?;
            promote_finalized_in(tx, max_nonce, inclusion_block)?;
            Ok(policy)
        })?;
        head.advance_frame(policy, next_safe_block);
        Ok(())
    }

    /// Physical-only fixture form of the atomic drain + promotion operation.
    #[cfg(test)]
    pub fn close_frame_only_promoting(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        leading_direct_range: SafeInputRange,
        max_nonce: u64,
        inclusion_block: u64,
    ) -> Result<()> {
        let policy = self.write(|tx| {
            let policy =
                close_frame_physical_only_in(tx, head, next_safe_block, leading_direct_range)?;
            promote_finalized_in(tx, max_nonce, inclusion_block)?;
            Ok(policy)
        })?;
        head.advance_frame(policy, next_safe_block);
        Ok(())
    }

    /// Close the current batch and open a fresh one with its first frame.
    /// Used when batch policy (size/deadline) triggers a batch close.
    ///
    /// Atomically: seal the current Tip (sets `sealed_at_ms`), insert the new
    /// Tip with `parent_batch_index = head.batch_index`, open its first frame.
    /// Order matters: sealing first removes the old row from the
    /// `ux_single_valid_tip` partial index, making room for the new Tip.
    pub fn close_frame_and_batch(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
    ) -> Result<()> {
        let (next_batch_index, now_ms, policy) =
            self.write(|tx| seal_and_open_next_batch(tx, head.batch_index, next_safe_block))?;
        head.move_to_next_batch(
            next_batch_index,
            from_unix_ms(now_ms),
            policy,
            next_safe_block,
        );
        Ok(())
    }

    /// Like [`Storage::close_frame_and_batch`], but in the same
    /// transaction also registers the pending snapshot for the batch
    /// being sealed.
    ///
    /// The caller must have already created the dump on disk at
    /// `dump_prefix` (filesystem-first, outside this tx). That ordering
    /// gives clean failure semantics:
    /// - `create_dump` fails → caller never reaches here → batch stays
    ///   the open Tip → retried next pass.
    /// - this tx fails → seal rolls back → no sealed-without-dump batch,
    ///   only an orphan directory the startup sweep reaps.
    /// - this tx commits → the sealed batch always has a promotable
    ///   `pending_snapshots` row, closing the "seal succeeds, snapshot
    ///   fails, promotion wedges on `QueryReturnedNoRows` forever" gap.
    ///
    /// `nonce` is the closing batch's nonce (assigned at open, so known
    /// before the seal). `l2_tx_index` is the global valid replay head
    /// the caller read when writing the dump's `info.toml` — correct
    /// even when the closing batch is empty.
    pub fn close_frame_and_batch_with_pending_dump(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        dump_dir: &Path,
        nonce: u64,
        l2_tx_index: u64,
        executed_input_count: ExecutedInputCount,
    ) -> Result<()> {
        let (next_batch_index, now_ms, policy) = self.write(|tx| {
            // The lane is the single writer and nothing sequences between
            // the caller's head read and this close, so the head cannot
            // have moved. Assert it: the snapshot row and the dump's
            // info.toml must record the same cursor.
            let head_now = valid_ordered_l2_tx_head(tx)?;
            assert_eq!(
                head_now, l2_tx_index,
                "replay head moved between dump creation and batch close"
            );
            let (next_batch_index, now_ms, policy) =
                seal_and_open_next_batch(tx, head.batch_index, next_safe_block)?;
            insert_pending_dump_in(tx, dump_dir, nonce, l2_tx_index, executed_input_count)?;
            Ok((next_batch_index, now_ms, policy))
        })?;
        head.move_to_next_batch(
            next_batch_index,
            from_unix_ms(now_ms),
            policy,
            next_safe_block,
        );
        Ok(())
    }

    pub fn batch_policy(&mut self) -> Result<BatchPolicy> {
        query_batch_policy(&self.conn)
    }
}

/// Insert a fresh open batch (the Tip) and its first empty frame inside `tx`.
/// The caller immediately persists the leading direct range through either the
/// production attributed path or the explicit cockroach/test padding path.
/// Lineage is the caller's: `batch_index_opt = Some(0)` forces the genesis index, `None`
/// auto-assigns the PK; `parent = None` roots a nonce-0 batch (genesis or a
/// fully-torn refork), else it inherits `parent.nonce + 1`. The single-Tip
/// invariant is enforced by the `ux_single_valid_tip` partial index.
///
/// Does **not** build a [`WriteHead`] — callers load it via
/// `load_current_write_head`, so the head has one constructor. Returns the new
/// `batch_index`.
fn insert_tip_rows(
    tx: &Transaction<'_>,
    batch_index_opt: Option<u64>,
    parent: Option<u64>,
    safe_block: u64,
) -> Result<u64> {
    let now_ms = now_unix_ms();
    let policy = query_batch_policy(tx)?;
    let batch_index = insert_new_batch(tx, batch_index_opt, parent, now_ms)?;
    insert_open_frame(
        tx,
        batch_index,
        0,
        now_ms,
        policy.recommended_fee,
        safe_block,
    )?;
    Ok(batch_index)
}

/// Extend the valid batch path with a fresh open Tip at the current safe head,
/// draining all currently-undrained safe inputs into its first frame.
///
/// One mechanism, two callers with distinct intents (each keeps its own guard):
/// the runtime's [`Storage::ensure_open_tip`] (genesis / first startup) and
/// recovery's cascade (reopening the Tip it just invalidated, atomically — see
/// `storage/recovery.rs`). Lineage is derived from the tree: `parent` is the
/// highest-indexed valid batch — `None` when the valid path is empty (genesis,
/// or a fully-torn cascade), rooting a nonce-0 batch; `batch_index` is the
/// explicit genesis `0` only when the `batches` table is empty, otherwise the
/// monotonic PK (so recovery batches keep climbing and indices are never
/// reused).
pub(super) fn open_fresh_tip_in_tx(tx: &Transaction<'_>) -> Result<()> {
    let safe_block = current_safe_block_required(tx)?;
    let table_empty = tx
        .query_row("SELECT MAX(batch_index) FROM batches", [], |row| {
            row.get::<_, Option<i64>>(0)
        })?
        .is_none();
    let parent = tx
        .query_row("SELECT MAX(batch_index) FROM valid_batches", [], |row| {
            row.get::<_, Option<i64>>(0)
        })?
        .map(i64_to_u64);
    let batch_index_opt = if table_empty { Some(0) } else { None };
    insert_draining_tip_with_executions(
        tx,
        batch_index_opt,
        parent,
        safe_block,
        query_latest_safe_input_index_exclusive(tx)?,
    )
}

/// The shared tail of [`open_fresh_tip_in_tx`] and [`open_recovery_tip_in_tx`]:
/// open the single valid Tip draining `[next_undrained, drain_upper)`, framed at
/// `safe_block`, with the given lineage. The two callers differ only in
/// `drain_upper` — the live safe-input head on the fresh path vs the `<= C` cap
/// on the recovery path (the load-bearing `(C, H1]` difference) — plus
/// `safe_block` and lineage, which they pass explicitly so the cap rule stays
/// visible at each call site rather than hidden in one branch.
fn insert_draining_tip_with_executions(
    tx: &Transaction<'_>,
    batch_index: Option<u64>,
    parent: Option<u64>,
    safe_block: u64,
    drain_upper: u64,
) -> Result<()> {
    let leading_direct_range =
        SafeInputRange::new(next_undrained_safe_input_index_in(tx)?, drain_upper);
    let batch_index = insert_tip_rows(tx, batch_index, parent, safe_block)?;
    persist_frame_direct_sequence_derived(tx, batch_index, 0, leading_direct_range)?;
    Ok(())
}

/// See [`Storage::open_recovery_tip`]. Like [`open_fresh_tip_in_tx`] but the
/// frame's safe block is the checkpoint stop block `C` and the leading drain
/// range is capped at the `<= C` directs (not the whole synced table), so the
/// `(C, H1]` directs the resync pulled past `C` stay undrained for `run`.
fn open_recovery_tip_in_tx(tx: &Transaction<'_>, stop_block: u64) -> Result<()> {
    debug_assert!(
        load_current_write_head(tx)?.is_none(),
        "recovery tip opened over existing open state"
    );
    // Fresh DB after wipe: batch_index 0, parentless (rooted at the anchor via
    // `compute_next_nonce(None)` -> `N'`); drain capped at the `<= C` safe
    // inputs.
    //
    // That range is sender-unfiltered: it includes the `<= C` batch-submitter
    // rows alongside the user directs, exactly as the fresh / genesis tip drains
    // its whole `[next_undrained, latest)` span. Correct because the leading
    // range is *sequenced, not executed* — it only advances the replay cursor so
    // `run`'s catch-up (`offset > l2_tx_index`) skips the rows already in `S'`.
    // The `sender != batch_submitter` drop belongs to the *fold's* seed filter
    // (those batches were folded into `S'` as batches, not directs); it is not a
    // cursor concern, so it deliberately does not reappear here.
    let leading_direct_range = SafeInputRange::new(
        next_undrained_safe_input_index_in(tx)?,
        safe_input_index_exclusive_through_block_in(tx, stop_block)?,
    );
    // These rows are cursor padding for state already represented by the
    // recovered snapshot. They permanently remain outside `executed_inputs`.
    let batch_index = insert_tip_rows(tx, Some(0), None, stop_block)?;
    persist_frame_direct_sequence_physical_only(tx, batch_index, 0, leading_direct_range)?;
    Ok(())
}

/// Exclusive `safe_input_index` boundary separating directs at `block_number <=
/// block` from those past it. `safe_input_index` is dense and assigned in
/// block-ascending ingest order, so the `<= block` rows occupy `[0, count)` and
/// this count is the boundary.
fn safe_input_index_exclusive_through_block_in(tx: &Transaction<'_>, block: u64) -> Result<u64> {
    let boundary: i64 = tx.query_row(
        "SELECT COUNT(*) FROM safe_inputs WHERE block_number <= ?1",
        params![saturating_query_bound(block)],
        |row| row.get(0),
    )?;
    Ok(i64_to_u64(boundary))
}

/// Maximum of the durable era floor and `MAX(safe_input_index) + 1` over valid
/// drained rows (or 0 if none), inside `tx`. The valid attribution may rewind
/// when a batch is invalidated, but never below inputs already represented by
/// the cockroach-recovered base snapshot.
fn next_undrained_safe_input_index_in(tx: &Transaction<'_>) -> Result<u64> {
    const SQL: &str = "
        SELECT COALESCE(MAX(safe_input_index) + 1, 0)
        FROM valid_sequenced_l2_txs
        WHERE safe_input_index IS NOT NULL
    ";
    let value: i64 = tx.query_row(SQL, [], |row| row.get(0))?;
    Ok(safe_input_floor_in(tx)?.max(i64_to_u64(value)))
}

/// Seal the current Tip and open the successor batch's first frame, in `tx`.
///
/// Shared by [`Storage::close_frame_and_batch`] and
/// [`Storage::close_frame_and_batch_with_pending_dump`] so the seal ordering
/// invariant lives in one place: seal first (which frees the old row from the
/// `ux_single_valid_tip` partial index), then insert the successor as the new
/// Tip. Returns the new batch index, the close timestamp, and the sampled
/// policy for the caller to apply to its in-memory write head.
fn seal_and_open_next_batch(
    tx: &Transaction<'_>,
    closing_batch_index: u64,
    next_safe_block: u64,
) -> Result<(u64, i64, BatchPolicy)> {
    let now_ms = now_unix_ms();
    // Batch policy is sampled here: the derived fee is committed to the newly
    // opened frame, and the batch size target is stored on the write head.
    let policy = query_batch_policy(tx)?;
    // Hash-at-seal: encode the closing batch's wire bytes via
    // the same path the submitter uses and stamp their keccak256 on the row,
    // atomically with the seal. The content-identity check later compares
    // accepted L1 landings against this hash.
    let nonce = super::snapshot_dumps::batch_nonce_in(tx, closing_batch_index)?;
    let frames = super::l1_submission::load_batch_frames_in(tx, closing_batch_index)?;
    let encoded = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch { nonce, frames });
    let payload_hash = alloy_primitives::keccak256(&encoded);
    seal_batch(tx, closing_batch_index, now_ms, &payload_hash.0)?;
    let next_batch_index = insert_new_batch(tx, None, Some(closing_batch_index), now_ms)?;
    insert_open_frame(
        tx,
        next_batch_index,
        0,
        now_ms,
        policy.recommended_fee,
        next_safe_block,
    )?;
    Ok((next_batch_index, now_ms, policy))
}

/// Rotate to the next frame inside the current batch, in `tx`: open the
/// successor frame (fresh fee/safe-block) and sequence the drained safe-input
/// range into it. Shared by the attributed production close/promote paths and
/// their physical-only test siblings so the frame-rotation invariant lives in
/// one place. Returns the sampled policy for the caller to apply to the
/// in-memory write head.
fn close_frame_in(
    tx: &Transaction<'_>,
    head: &WriteHead,
    next_safe_block: u64,
    leading_direct_range: SafeInputRange,
    executions: &[DirectInputExecution],
) -> Result<BatchPolicy> {
    let (policy, next_frame_in_batch) = open_successor_frame_in(tx, head, next_safe_block)?;
    persist_frame_direct_sequence(
        tx,
        head.batch_index,
        next_frame_in_batch,
        leading_direct_range,
        executions,
    )?;
    Ok(policy)
}

#[cfg(test)]
fn close_frame_physical_only_in(
    tx: &Transaction<'_>,
    head: &WriteHead,
    next_safe_block: u64,
    leading_direct_range: SafeInputRange,
) -> Result<BatchPolicy> {
    let (policy, next_frame_in_batch) = open_successor_frame_in(tx, head, next_safe_block)?;
    persist_frame_direct_sequence_physical_only(
        tx,
        head.batch_index,
        next_frame_in_batch,
        leading_direct_range,
    )?;
    Ok(policy)
}

fn open_successor_frame_in(
    tx: &Transaction<'_>,
    head: &WriteHead,
    next_safe_block: u64,
) -> Result<(BatchPolicy, u32)> {
    let now_ms = now_unix_ms();
    let policy = query_batch_policy(tx)?;
    let next_frame_in_batch = head
        .frame_in_batch
        .checked_add(1)
        .expect("frame index overflow: contract-impossible");
    insert_open_frame(
        tx,
        head.batch_index,
        next_frame_in_batch,
        now_ms,
        policy.recommended_fee,
        next_safe_block,
    )?;
    Ok((policy, next_frame_in_batch))
}

/// Insert user ops into `user_ops`. The `trg_sequence_user_op` trigger then
/// appends the matching `sequenced_l2_txs` row for each insert.
#[cfg(test)]
fn insert_user_ops_batch(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    frame_pos_start: u32,
    user_ops: &[PendingUserOp],
) -> Result<()> {
    insert_user_op_iter(
        tx,
        batch_index,
        frame_in_batch,
        frame_pos_start,
        user_ops.iter(),
    )
}

fn insert_user_op_iter<'a>(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    frame_pos_start: u32,
    user_ops: impl IntoIterator<Item = &'a PendingUserOp>,
) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO user_ops (
            batch_index, frame_in_batch, pos_in_frame,
            sender, nonce, max_fee, data, sig, received_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for (offset, item) in user_ops.into_iter().enumerate() {
        let offset =
            u32::try_from(offset).expect("user-op chunk offset exceeds u32: contract-impossible");
        let pos_in_frame = frame_pos_start
            .checked_add(offset)
            .expect("user-op position overflow: contract-impossible");
        let sig = item.signed.signature.as_bytes();
        stmt.execute(params![
            u64_to_i64(batch_index),
            i64::from(frame_in_batch),
            i64::from(pos_in_frame),
            item.signed.sender.as_slice(),
            i64::from(item.signed.user_op.nonce),
            i64::from(item.signed.user_op.max_fee),
            item.signed.user_op.data.as_ref(),
            &sig[..],
            to_unix_ms(item.received_at),
        ])?;
    }
    Ok(())
}

/// Persist one included user-op chunk and attach every explicit application
/// offset before the transaction can commit. The trigger-created physical rows
/// are selected in frame-position order once per chunk, keeping the hot path to
/// one attribution read rather than one lookup per operation.
fn insert_executed_user_ops_batch(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    frame_pos_start: u32,
    user_ops: &[IncludedUserOp],
) -> Result<()> {
    insert_user_op_iter(
        tx,
        batch_index,
        frame_in_batch,
        frame_pos_start,
        user_ops.iter().map(|item| &item.pending),
    )?;
    if user_ops.is_empty() {
        return Ok(());
    }

    let chunk_len = u32::try_from(user_ops.len())
        .expect("user-op chunk length exceeds u32: contract-impossible");
    let frame_pos_end = frame_pos_start
        .checked_add(chunk_len)
        .expect("user-op position overflow: contract-impossible");
    let mut stmt = tx.prepare_cached(
        "SELECT offset, user_op_pos_in_frame \
         FROM sequenced_l2_txs \
         WHERE batch_index = ?1 \
           AND frame_in_batch = ?2 \
           AND user_op_pos_in_frame >= ?3 \
           AND user_op_pos_in_frame < ?4 \
         ORDER BY user_op_pos_in_frame ASC",
    )?;
    let rows = stmt.query_map(
        params![
            u64_to_i64(batch_index),
            i64::from(frame_in_batch),
            i64::from(frame_pos_start),
            i64::from(frame_pos_end),
        ],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let persisted = rows.collect::<Result<Vec<_>>>()?;
    drop(stmt);
    assert_eq!(
        persisted.len(),
        user_ops.len(),
        "user-op physical row count differs from included execution count"
    );

    let mut mappings = Vec::with_capacity(persisted.len());
    for (offset, ((physical_offset, position), executed_input_offset)) in persisted
        .into_iter()
        .zip(user_ops.iter().map(|item| item.executed_input_offset))
        .enumerate()
    {
        let offset =
            u32::try_from(offset).expect("user-op chunk offset exceeds u32: contract-impossible");
        assert_eq!(
            i64_to_u64(position),
            u64::from(
                frame_pos_start
                    .checked_add(offset)
                    .expect("user-op position overflow: contract-impossible")
            ),
            "user-op physical rows are not contiguous in frame order"
        );
        mappings.push(ExecutedInputMapping {
            sequenced_l2_tx_offset: i64_to_u64(physical_offset),
            executed_input_offset,
        });
    }
    attach_executed_inputs_in(tx, &mappings)
}

#[cfg(test)]
mod tests {
    use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp};
    use crate::storage::{
        DeploymentIdentity, DirectInputExecution, ExecutedInputCount, FeeOracleIdentity,
        LifecycleCommand, SafeFrontierState, SafeInputFrontier, SafeInputRange, Storage,
        StoredSafeInput,
        test_helpers::{SENDER_A, default_protocol_timing, record_canonical_divergence, temp_db},
    };
    use alloy_primitives::{Address, Signature};
    use sequencer_core::l2_tx::SequencedL2Tx;
    use sequencer_core::user_op::{SignedUserOp, UserOp};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::time::SystemTime;
    use tokio::sync::oneshot;

    fn pending_user_op(nonce: u32) -> PendingUserOp {
        let (respond_to, _response) = oneshot::channel();
        PendingUserOp {
            signed: SignedUserOp {
                sender: Address::ZERO,
                signature: Signature::test_signature(),
                user_op: UserOp {
                    nonce,
                    max_fee: u16::MAX,
                    data: vec![].into(),
                },
            },
            respond_to,
            received_at: SystemTime::now(),
        }
    }

    fn included_user_op(nonce: u32, offset: u64) -> IncludedUserOp {
        IncludedUserOp {
            pending: pending_user_op(nonce),
            executed_input_offset: ExecutedInputCount::new(offset),
        }
    }

    fn physical_user_op_offsets(storage: &Storage) -> Vec<i64> {
        storage
            .conn
            .prepare(
                "SELECT offset FROM sequenced_l2_txs \
                 WHERE user_op_pos_in_frame IS NOT NULL ORDER BY offset",
            )
            .expect("prepare physical user-op query")
            .query_map([], |row| row.get(0))
            .expect("query physical user-op offsets")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect physical user-op offsets")
    }

    fn pin_deployment_identity(storage: &mut Storage, batch_submitter_address: Address) {
        storage
            .load_or_insert_deployment_identity(DeploymentIdentity {
                chain_id: 1,
                app_address: Address::repeat_byte(0x11),
                input_box_address: Address::repeat_byte(0x22),
                app_deployment_block: 0,
                batch_submitter_address,
                fee_oracle: FeeOracleIdentity::Fixed { log_gas_price: 0 },
            })
            .expect("pin deployment identity");
    }

    #[test]
    fn open_state_is_idempotent_and_rotation_is_atomic() {
        let db = temp_db("open-state");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        assert!(
            storage.open_state().expect("load open state").is_none(),
            "fresh storage should not have an open frame yet"
        );

        let head_a = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let head_b = storage
            .open_state()
            .expect("load existing open state")
            .expect("open state should now exist");

        assert_eq!(head_a.batch_index, head_b.batch_index);
        assert_eq!(head_a.frame_in_batch, head_b.frame_in_batch);
        assert_eq!(head_a.frame_fee, head_b.frame_fee);
        // Default log_recommended_fee = 0+296+20+419+621 = 1356
        assert_eq!(head_a.frame_fee, 1356);

        let mut head_c = head_b;
        let next_safe_block = head_c.safe_block;
        storage
            .close_frame_only(&mut head_c, next_safe_block, SafeInputRange::empty_at(0))
            .expect("rotate within same batch");
        assert_eq!(head_c.batch_index, head_b.batch_index);
        assert_eq!(head_c.frame_in_batch, 1);

        let mut head_d = head_c;
        let next_safe_block = head_d.safe_block;
        storage
            .close_frame_and_batch(&mut head_d, next_safe_block)
            .expect("close batch and rotate");
        assert!(head_d.batch_index > head_c.batch_index);
        assert_eq!(head_d.frame_in_batch, 0);
    }

    #[test]
    fn safe_frontier_state_withholds_poisoned_projection() {
        let db = temp_db("safe-frontier-state-poison");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .append_safe_inputs(12, &[], SENDER_A, &default_protocol_timing())
            .expect("record safe head");

        assert_eq!(
            storage.safe_frontier_state().expect("read open frontier"),
            SafeFrontierState::Open(SafeInputFrontier {
                safe_block: 12,
                end_exclusive: 0,
            })
        );

        record_canonical_divergence(&mut storage, 7, 3);
        assert_eq!(
            storage
                .safe_frontier_state()
                .expect("read poisoned frontier"),
            SafeFrontierState::CanonicalDivergence {
                nonce: 7,
                safe_input_index: 3,
            },
            "the marker must outrank an otherwise-valid safe frontier"
        );
    }

    #[test]
    fn append_counter_overflow_happens_before_the_transaction() {
        let db = temp_db("append-counter-overflow-before-write");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        head.open_frame_user_op_count = u32::MAX;

        let (respond_to, _response) = oneshot::channel();
        let pending = PendingUserOp {
            signed: SignedUserOp {
                sender: Address::ZERO,
                signature: Signature::test_signature(),
                user_op: UserOp {
                    nonce: 0,
                    max_fee: u16::MAX,
                    data: vec![].into(),
                },
            },
            respond_to,
            received_at: SystemTime::now(),
        };

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = storage.append_user_ops_chunk(&mut head, &[pending]);
        }));
        assert!(panic.is_err(), "counter overflow must fail loud");

        let persisted: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM user_ops", [], |row| row.get(0))
            .expect("count user ops");
        assert_eq!(persisted, 0, "overflow must occur before any durable write");
        assert_eq!(
            head.open_frame_user_op_count,
            u32::MAX,
            "the authoritative head must remain unchanged"
        );
    }

    #[test]
    fn mismatched_user_execution_offset_rolls_back_physical_and_logical_rows() {
        let db = temp_db("user-execution-offset-atomicity");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        let included = included_user_op(0, 1);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = storage.append_executed_user_ops_chunk(&mut head, &[included]);
        }));
        assert!(
            panic.is_err(),
            "non-canonical execution offset must fail loud"
        );

        for table in ["user_ops", "sequenced_l2_txs", "executed_inputs"] {
            let persisted: i64 = storage
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count rolled-back rows");
            assert_eq!(persisted, 0, "{table} must roll back atomically");
        }
        assert_eq!(
            storage.next_executed_input_count().expect("next count"),
            ExecutedInputCount::ZERO
        );
    }

    #[test]
    fn schema_rejects_execution_attribution_to_a_non_tip_row() {
        let db = temp_db("executed-input-non-tip");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        storage
            .append_user_ops_chunk(&mut head, &[pending_user_op(0)])
            .expect("append physical-only user op");
        let physical_offset = physical_user_op_offsets(&storage)[0];
        storage
            .close_frame_and_batch(&mut head, 0)
            .expect("seal the physical row's batch");

        let err = storage
            .conn
            .execute(
                "INSERT INTO executed_inputs \
                 (sequenced_l2_tx_offset, executed_input_offset) VALUES (?1, 0)",
                [physical_offset],
            )
            .expect_err("a sealed batch cannot acquire execution attribution");
        assert!(
            err.to_string().contains("current valid Tip"),
            "unexpected trigger error: {err:?}"
        );
    }

    #[test]
    fn schema_rejects_execution_attribution_before_rebuild_base_is_bound() {
        let db = temp_db("executed-input-unbound-base");
        let mut storage =
            Storage::initialize_for_command(db.path.as_str(), LifecycleCommand::Rebuild)
                .expect("initialize rebuild storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize recovery root fixture");
        storage
            .append_user_ops_chunk(&mut head, &[pending_user_op(0)])
            .expect("append physical-only user op");
        let physical_offset = physical_user_op_offsets(&storage)[0];

        let err = storage
            .conn
            .execute(
                "INSERT INTO executed_inputs \
                 (sequenced_l2_tx_offset, executed_input_offset) VALUES (?1, 0)",
                [physical_offset],
            )
            .expect_err("an unbound rebuild cannot acquire execution attribution");
        assert!(
            err.to_string().contains("history base is not bound"),
            "unexpected trigger error: {err:?}"
        );
    }

    #[test]
    fn schema_rejects_execution_attribution_physical_backfill() {
        let db = temp_db("executed-input-physical-backfill");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        storage
            .append_user_ops_chunk(&mut head, &[pending_user_op(0), pending_user_op(1)])
            .expect("append physical-only user ops");
        let physical_offsets = physical_user_op_offsets(&storage);
        storage
            .conn
            .execute(
                "INSERT INTO executed_inputs \
                 (sequenced_l2_tx_offset, executed_input_offset) VALUES (?1, 0)",
                [physical_offsets[1]],
            )
            .expect("seed the later physical attribution");

        let err = storage
            .conn
            .execute(
                "INSERT INTO executed_inputs \
                 (sequenced_l2_tx_offset, executed_input_offset) VALUES (?1, 1)",
                [physical_offsets[0]],
            )
            .expect_err("execution attribution cannot move backward physically");
        assert!(
            err.to_string()
                .contains("must follow physical replay order"),
            "unexpected trigger error: {err:?}"
        );
    }

    #[test]
    fn schema_rejects_noncanonical_execution_offset() {
        let db = temp_db("executed-input-logical-gap");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        storage
            .append_user_ops_chunk(&mut head, &[pending_user_op(0)])
            .expect("append physical-only user op");
        let physical_offset = physical_user_op_offsets(&storage)[0];

        let err = storage
            .conn
            .execute(
                "INSERT INTO executed_inputs \
                 (sequenced_l2_tx_offset, executed_input_offset) VALUES (?1, 1)",
                [physical_offset],
            )
            .expect_err("the first canonical offset must equal the zero base");
        assert!(
            err.to_string().contains("must equal canonical next count"),
            "unexpected trigger error: {err:?}"
        );
    }

    #[test]
    fn schema_rejects_deleting_valid_execution_attribution() {
        let db = temp_db("executed-input-valid-delete");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        storage
            .append_executed_user_ops_chunk(&mut head, &[included_user_op(0, 0)])
            .expect("append mapped user op");

        let err = storage
            .conn
            .execute(
                "DELETE FROM executed_inputs WHERE executed_input_offset = 0",
                [],
            )
            .expect_err("valid canonical attribution is not deletable");
        assert!(
            err.to_string()
                .contains("valid executed input attribution cannot be deleted"),
            "unexpected trigger error: {err:?}"
        );
        assert_eq!(
            storage.next_executed_input_count().expect("preserved head"),
            ExecutedInputCount::new(1)
        );
    }

    #[test]
    fn live_direct_rotation_requires_complete_classified_attribution() {
        let db = temp_db("direct-execution-attribution-complete");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        pin_deployment_identity(&mut storage, SENDER_A);
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let directs = [
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::repeat_byte(0x44),
                payload: vec![0xbb],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, &directs, SENDER_A, &default_protocol_timing())
            .expect("persist direct inputs");

        let incomplete = [DirectInputExecution {
            safe_input_index: 0,
            executed_input_offset: ExecutedInputCount::ZERO,
        }];
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = storage.close_frame_only_with_executions(
                &mut head,
                10,
                SafeInputRange::new(0, 2),
                &incomplete,
            );
        }));
        assert!(panic.is_err(), "omitted executable direct must fail loud");
        assert_eq!(head.frame_in_batch, 0);
        let frames: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM frames", [], |row| row.get(0))
            .unwrap();
        assert_eq!(frames, 1, "failed rotation must roll back its new frame");

        let complete = [
            DirectInputExecution {
                safe_input_index: 0,
                executed_input_offset: ExecutedInputCount::ZERO,
            },
            DirectInputExecution {
                safe_input_index: 1,
                executed_input_offset: ExecutedInputCount::new(1),
            },
        ];
        storage
            .close_frame_only_with_executions(&mut head, 10, SafeInputRange::new(0, 2), &complete)
            .expect("commit complete direct attribution");
        assert_eq!(
            storage.next_executed_input_count().unwrap(),
            ExecutedInputCount::new(2)
        );
    }

    #[test]
    fn invalidation_rewinds_and_replacement_reuses_logical_offset() {
        let db = temp_db("execution-offset-invalidation-reuse");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        storage
            .append_executed_user_ops_chunk(&mut head, &[included_user_op(0, 0)])
            .expect("append gold input");
        storage
            .close_frame_and_batch(&mut head, 0)
            .expect("seal gold batch");
        storage
            .append_executed_user_ops_chunk(&mut head, &[included_user_op(1, 1)])
            .expect("append doomed input");
        assert_eq!(
            storage
                .next_executed_input_count()
                .expect("pre-recovery count"),
            ExecutedInputCount::new(2)
        );

        storage
            .append_safe_inputs(1_500, &[], SENDER_A, &default_protocol_timing())
            .expect("advance safe head");
        assert_eq!(
            storage
                .recover_aging_tip(1_200)
                .expect("invalidate stale Tip"),
            vec![1]
        );
        assert_eq!(
            storage.next_executed_input_count().expect("rewound count"),
            ExecutedInputCount::new(1)
        );

        let mut replacement = storage.open_state().unwrap().unwrap();
        storage
            .append_executed_user_ops_chunk(&mut replacement, &[included_user_op(1, 1)])
            .expect("reuse invalidated logical offset");

        let current: Vec<i64> = storage
            .conn
            .prepare(
                "SELECT executed_input_offset FROM executed_inputs \
                 ORDER BY sequenced_l2_tx_offset",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(current, vec![0, 1]);
        let valid: Vec<i64> = storage
            .conn
            .prepare(
                "SELECT executed_input_offset FROM valid_executed_inputs \
                 ORDER BY sequenced_l2_tx_offset",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(valid, vec![0, 1]);
        let physical_rows: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM sequenced_l2_txs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            physical_rows, 3,
            "invalidation removes only derived mappings, not physical audit rows"
        );
    }

    #[test]
    fn snapshot_promotion_preserves_executed_input_count() {
        let db = temp_db("snapshot-executed-input-count-promotion");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        storage
            .append_executed_user_ops_chunk(&mut head, &[included_user_op(0, 0)])
            .expect("append mapped user op");
        let physical_head = storage.valid_ordered_l2_tx_head().unwrap();
        storage
            .insert_pending_dump(&db._dir.path().join("pending"), 0, physical_head)
            .expect("insert pending checkpoint");
        assert_eq!(
            storage
                .latest_pending_dump()
                .unwrap()
                .unwrap()
                .executed_input_count,
            ExecutedInputCount::new(1)
        );

        storage
            .promote_finalized(0, 10)
            .expect("promote checkpoint");
        assert_eq!(
            storage
                .finalized_dump()
                .unwrap()
                .unwrap()
                .executed_input_count,
            ExecutedInputCount::new(1),
            "promotion must copy the application boundary with the physical cursor"
        );
    }

    #[test]
    fn cockroach_padding_stays_unmapped_above_absolute_history_base() {
        let db = temp_db("cockroach-padding-execution-offsets");
        let mut storage =
            Storage::initialize_for_command(db.path.as_str(), LifecycleCommand::Rebuild)
                .expect("initialize rebuild storage");
        let recovered = [
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, &recovered, SENDER_A, &default_protocol_timing())
            .expect("persist recovered L1 prefix");
        storage
            .open_recovery_tip(10)
            .expect("open padded recovery root");
        let physical_head = storage
            .valid_ordered_l2_tx_head()
            .expect("physical replay head");
        storage
            .insert_initial_finalized_dump(
                &db._dir.path().join("recovered"),
                10,
                physical_head,
                41,
                2,
            )
            .expect("bind recovered application base");

        let mapped: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM executed_inputs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mapped, 0, "recovery-root padding is never re-attributed");
        assert_eq!(
            storage
                .next_executed_input_count()
                .expect("absolute next count"),
            ExecutedInputCount::new(41)
        );
        let replay = storage.ordered_l2_txs_page_from(0, 10).unwrap();
        assert_eq!(replay.len(), 2);
        assert!(replay.iter().all(|row| row.executed_input_offset.is_none()));
    }

    #[test]
    fn recovery_checkpoint_block_uses_query_and_persistence_boundaries() {
        let db = temp_db("recovery-checkpoint-boundaries");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        storage
            .append_safe_inputs(
                10,
                &[StoredSafeInput {
                    sender: Address::ZERO,
                    payload: vec![],
                    block_number: 10,
                }],
                SENDER_A,
                &default_protocol_timing(),
            )
            .expect("append safe input");

        let boundary = storage
            .read(|tx| super::safe_input_index_exclusive_through_block_in(tx, u64::MAX))
            .expect("query upper bound above SQLite range");
        assert_eq!(
            boundary, 1,
            "a saturated upper predicate must include every representable block"
        );

        let err = storage
            .open_recovery_tip(i64::MAX as u64 + 1)
            .expect_err("unrepresentable persisted checkpoint must be refused");
        assert!(matches!(err, rusqlite::Error::ToSqlConversionFailure(_)));
    }

    #[test]
    fn next_frame_fee_comes_from_batch_policy() {
        let db = temp_db("batch-policy-fee");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let policy = storage.batch_policy().expect("default policy");
        // Default: log_gas_price=0, log_recommended_fee = 0+296+20+419+621 = 1356
        assert_eq!(policy.recommended_fee, 1356);

        storage.set_log_gas_price(100).expect("set log gas price");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let next_safe_block = head.safe_block;
        storage
            .close_frame_and_batch(&mut head, next_safe_block)
            .expect("rotate batch");

        let policy = storage.batch_policy().expect("read policy");
        // log_recommended_fee = 100+296+20+419+621 = 1456
        assert_eq!(head.frame_fee, 1456);
        assert_eq!(head.frame_fee, policy.recommended_fee);
        assert!(
            head.max_batch_user_op_bytes > 0,
            "batch size target should be set"
        );
    }

    #[test]
    fn frame_fee_is_immutable_for_the_lifetime_of_the_frame() {
        // : once a frame is opened at fee F, a policy update mid-frame
        // must NOT change the open frame's committed fee. Only the *next*
        // frame (after close) sees the new policy. This pins the write-once
        // contract `frames.fee` relies on — users submitting against the open
        // frame know the fee they're paying, regardless of upstream policy
        // drift during their round-trip.
        let db = temp_db("frame-fee-immutable");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let original_batch_index = head.batch_index;
        let original_frame_in_batch = head.frame_in_batch;
        // Default: log_gas_price=0 → log_recommended_fee = 0+296+20+419+621 = 1356
        assert_eq!(head.frame_fee, 1356);

        // Simulate an operator policy update mid-frame: fee oracle reports a
        // higher gas price. The derived view reflects the new fee immediately.
        storage
            .set_log_gas_price(100)
            .expect("set higher log gas price");
        let new_policy = storage.batch_policy().expect("read updated policy");
        assert_eq!(
            new_policy.recommended_fee, 1456,
            "policy-derived fee should reflect the new gas price",
        );

        // Invariant: the already-open frame's persisted fee stays at 1356.
        let persisted_frame_fee: i64 = storage
            .conn
            .query_row(
                "SELECT fee FROM frames WHERE batch_index = ?1 AND frame_in_batch = ?2",
                rusqlite::params![original_batch_index as i64, original_frame_in_batch as i64,],
                |row| row.get(0),
            )
            .expect("query open frame fee");
        assert_eq!(
            persisted_frame_fee, 1356,
            "open frame's committed fee must not change across policy updates",
        );

        // And the in-memory WriteHead mirror must also be stable — the lane
        // submitting against this head should see a consistent fee.
        assert_eq!(
            head.frame_fee, 1356,
            "WriteHead.frame_fee must stay stable until advance_frame runs",
        );

        // Closing the frame picks up the new policy — the *next* frame opens
        // at 1456. This is the expected policy-flow boundary.
        let next_safe_block = head.safe_block;
        storage
            .close_frame_only(&mut head, next_safe_block, SafeInputRange::empty_at(0))
            .expect("rotate within same batch");
        assert_eq!(
            head.frame_fee, 1456,
            "the next frame must use the updated policy's fee (policy flows in at close)",
        );
    }

    #[test]
    fn next_undrained_safe_input_index_is_derived_from_sequenced_directs() {
        let db = temp_db("safe-cursor");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        assert_eq!(
            storage
                .next_undrained_safe_input_index()
                .expect("empty cursor"),
            0
        );

        let head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let drained = vec![
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0x00],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0x02],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, drained.as_slice(), SENDER_A, &default_protocol_timing())
            .expect("insert direct inputs");
        let mut head = head;
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, drained.len() as u64))
            .expect("close frame with directs");

        assert_eq!(
            storage
                .next_undrained_safe_input_index()
                .expect("derived cursor"),
            2
        );
    }

    #[test]
    fn initialize_open_state_creates_first_real_batch_and_frame() {
        let db = temp_db("initialize-open-state");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        let head = storage
            .initialize_open_state(12, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        assert_eq!(head.batch_index, 0);
        assert_eq!(head.frame_in_batch, 0);
        assert_eq!(head.safe_block, 12);

        let loaded = storage
            .open_state()
            .expect("load open state")
            .expect("open state should exist");
        assert_eq!(loaded.batch_index, 0);
        assert_eq!(loaded.frame_in_batch, 0);
        assert_eq!(loaded.safe_block, 12);
    }

    #[test]
    fn replay_returns_direct_inputs_in_drain_order() {
        let db = temp_db("replay-order");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        let drained = vec![
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, drained.as_slice(), SENDER_A, &default_protocol_timing())
            .expect("insert direct inputs");
        let mut head = head;
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::new(0, drained.len() as u64))
            .expect("close frame with directs");

        let replay = storage
            .ordered_l2_txs_page_from(0, 100)
            .expect("load replay");
        assert_eq!(replay.len(), 2);
        match &replay[0].tx {
            SequencedL2Tx::Direct(value) => assert_eq!(value.payload.as_slice(), &[0xaa]),
            _ => panic!("expected direct input at position 0"),
        }
        match &replay[1].tx {
            SequencedL2Tx::Direct(value) => assert_eq!(value.payload.as_slice(), &[0xbb]),
            _ => panic!("expected direct input at position 1"),
        }
    }

    #[test]
    fn ensure_open_tip_opens_genesis_and_sequences_leading_range() {
        let db = temp_db("ensure-tip-genesis");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        pin_deployment_identity(&mut storage, SENDER_A);

        // Pre-existing L1 history: two safe inputs at block 10, and an
        // observed safe head of 10 (as the startup recovery sync would leave).
        let leading = vec![
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xaa],
                block_number: 10,
            },
            StoredSafeInput {
                sender: Address::ZERO,
                payload: vec![0xbb],
                block_number: 10,
            },
        ];
        storage
            .append_safe_inputs(10, leading.as_slice(), SENDER_A, &default_protocol_timing())
            .expect("seed safe inputs + head");

        assert!(
            storage.open_state().expect("load open state").is_none(),
            "fresh DB has no Tip before ensure_open_tip"
        );

        storage.ensure_open_tip().expect("open genesis tip");

        // The Tip now exists; load its head the way the lane does (it loads
        // from storage, never receives the head).
        let head = storage
            .open_state()
            .expect("load open state")
            .expect("genesis tip exists");
        assert_eq!(head.batch_index, 0, "genesis batch is index 0");
        assert_eq!(head.frame_in_batch, 0);
        assert_eq!(
            head.safe_block, 10,
            "genesis frame opens at the synced safe head"
        );

        // The leading range is *sequenced* into the Tip, so the drain cursor
        // has advanced past it.
        assert_eq!(
            storage
                .next_undrained_safe_input_index()
                .expect("derived cursor"),
            2,
            "leading range [0,2) sequenced into genesis frame 0"
        );

        // The rows are in the ordered L2-tx stream for catch-up to replay and
        // carry their creation-time application offsets. ensure_open_tip does
        // not itself run application code; replay validates those offsets.
        let replay = storage
            .ordered_l2_txs_page_from(0, 100)
            .expect("load replay");
        assert_eq!(
            replay.len(),
            2,
            "leading directs are in the replay stream for catch-up"
        );
        assert_eq!(
            replay[0].executed_input_offset,
            Some(ExecutedInputCount::ZERO)
        );
        assert_eq!(
            replay[1].executed_input_offset,
            Some(ExecutedInputCount::new(1))
        );
    }

    #[test]
    fn ensure_open_tip_is_noop_when_tip_already_exists() {
        let db = temp_db("ensure-tip-noop");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");

        // Seed an explicit open Tip at safe_block 5. Note: no safe-head
        // observation is recorded, so a genesis-branch `current_safe_block`
        // read would fail — proving the warm path returns before it.
        let seeded = storage
            .initialize_open_state(5, SafeInputRange::empty_at(0))
            .expect("seed open state");

        storage.ensure_open_tip().expect("warm path is a no-op");
        let head = storage
            .open_state()
            .expect("load open state")
            .expect("tip still exists");
        assert_eq!(
            head.batch_index, seeded.batch_index,
            "warm path leaves the existing Tip, does not reopen"
        );
        assert_eq!(
            head.safe_block, 5,
            "existing frame's safe_block is preserved, not refreshed"
        );
    }
}
