// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Inclusion-lane writer: opens the initial batch/frame, appends user-op chunks,
//! and rotates frame/batch boundaries on the hot path.
//!
//! The lane also reads classified external directs and the open
//! state (resumed on startup) — those reads live here too because they're driven
//! by the lane's flow, not by an L1 ingress event.

use std::path::Path;

use alloy_primitives::Address;
use rusqlite::{OptionalExtension, Result, Transaction, params};

#[cfg(test)]
use super::StoredSafeInput;
#[cfg(test)]
use super::convert::external_u64_to_i64;
use super::convert::{
    from_unix_ms, i64_to_u64, now_unix_ms, saturating_query_bound, to_unix_ms, u64_to_i64,
};
use super::history::{next_executed_input_count_in, query_history_state};
use super::mutations::{
    insert_new_batch, insert_open_frame, persist_frame_direct_sequence,
    persist_frame_direct_sequence_derived, seal_batch,
};
use super::queries::{
    current_safe_block_required, load_current_write_head, query_batch_policy,
    query_latest_safe_input_index_exclusive,
};
use super::safe_accepted_batches::canonical_divergence_in;
use super::snapshot_dumps::insert_batch_snapshot_in;
use super::{
    BatchPolicy, DirectInputExecution, ExecutedInputCount, SafeFrontierState, SafeInputFrontier,
    SafeInputRange, Storage, WriteHead,
};
use crate::ingress::inclusion_lane::{IncludedUserOp, PendingUserOp};

impl Storage {
    /// First L1 input beyond the latest surviving frame's accounted block,
    /// bounded below by the immutable era prefix.
    pub fn next_undrained_safe_input_index(&mut self) -> Result<u64> {
        self.read(next_undrained_safe_input_index_in)
    }

    /// Resume the lane on startup. Returns `None` if storage is empty (caller
    /// must establish the Tip through startup recovery).
    pub fn open_state(&mut self) -> Result<Option<WriteHead>> {
        self.read(load_current_write_head)
    }

    /// Open-frame fee and current `recommended_fee`. `None` if there is no Tip.
    pub fn current_fee_quote(&mut self) -> Result<Option<(u16, u16)>> {
        self.read(|tx| {
            let Some(head) = load_current_write_head(tx)? else {
                return Ok(None);
            };
            let policy = query_batch_policy(tx)?;
            Ok(Some((head.frame_fee, policy.recommended_fee)))
        })
    }

    /// Bootstrap the very first batch + frame with explicit values, returning
    /// its loaded [`WriteHead`]. Asserts no open state exists.
    ///
    /// Production opens the genesis Tip through guarded startup recovery, which derives `safe_block`/leading range from the
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
            persist_frame_direct_sequence_derived(tx, batch_index, 0, leading_direct_range)?;
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
    /// This unguarded form exists for test harnesses only. Production uses
    /// `ensure_open_tip_for_recovery`, which reasserts the startup facts in its
    /// write transaction. The lane only ever *loads* a Tip (fail-loud if
    /// absent); Cascade owns its own atomic reopen using the same mechanism.
    ///
    /// **Precondition (genesis branch only):** a safe-head observation must
    /// exist. A fresh DB requires L1 at bootstrap plus a successful recovery
    /// sync, else startup refuses before reaching here; the warm-DB early
    /// return fires before any safe-head read.
    #[cfg(test)]
    pub(crate) fn ensure_open_tip(&mut self) -> Result<()> {
        self.write(|tx| {
            if load_current_write_head(tx)?.is_some() {
                return Ok(());
            }
            open_fresh_tip_in_tx(tx)
        })
    }

    /// Open the rebuilt root at the baseline's L1 stop block. Its prefix is
    /// represented by the baseline artifact and contributes no replay rows.
    #[cfg(test)]
    pub(crate) fn open_recovery_tip(&mut self, stop_block: u64) -> Result<()> {
        external_u64_to_i64(stop_block, "recovery checkpoint block")?;
        self.write(|tx| open_recovery_tip_in_tx(tx, stop_block))
    }

    /// Snapshot the current L1 reconciliation state. A canonical-divergence
    /// marker outranks and withholds the otherwise-usable frontier.
    ///
    /// **Precondition:** at least one safe-head observation must have been
    /// recorded. The lane only starts after the recovery procedure admits,
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

    pub(crate) fn fill_direct_inputs(
        &mut self,
        range: SafeInputRange,
        out: &mut Vec<super::StoredDirectInput>,
    ) -> Result<()> {
        out.clear();
        if range.is_empty() {
            return Ok(());
        }
        let identity = super::l1_inputs::query_deployment_identity(&self.conn)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let mut statement = self.conn.prepare_cached(
            "SELECT safe_input_index, sender, payload, block_number FROM safe_inputs
             WHERE safe_input_index >= ?1 AND safe_input_index < ?2 AND sender != ?3
             ORDER BY safe_input_index",
        )?;
        let rows = statement.query_map(
            params![
                u64_to_i64(range.start()),
                u64_to_i64(range.end()),
                identity.batch_submitter_address.as_slice()
            ],
            |row| {
                let sender: Vec<u8> = row.get(1)?;
                Ok(super::StoredDirectInput {
                    safe_input_index: i64_to_u64(row.get(0)?),
                    input: sequencer_core::l2_tx::DirectInput {
                        sender: Address::from_slice(&sender),
                        payload: row.get(2)?,
                        block_number: i64_to_u64(row.get(3)?),
                    },
                })
            },
        )?;
        for row in rows {
            out.push(row?);
        }
        Ok(())
    }

    /// Replace `out`'s contents with the safe-input rows in `range`. Asserts
    /// contiguity — gaps in `safe_input_index` are a bug, not a runtime
    /// condition.
    #[cfg(test)]
    pub(crate) fn fill_safe_inputs(
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

    /// Commit frame advancement together with its application inputs.
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

    /// Fixture frame rotation with derived application offsets. Production must supply explicit
    /// execution attributions through
    /// [`Storage::close_frame_only_with_executions`].
    #[cfg(test)]
    pub fn close_frame_only(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        leading_direct_range: SafeInputRange,
    ) -> Result<()> {
        let policy = self
            .write(|tx| close_frame_derived_in(tx, head, next_safe_block, leading_direct_range))?;
        head.advance_frame(policy, next_safe_block);
        Ok(())
    }

    /// Close the current batch and open a fresh one with its first frame,
    /// without registering a snapshot. Test-only: production closes
    /// through [`Storage::close_frame_and_batch_with_snapshot`], which
    /// registers the snapshot row in the same transaction (I7).
    ///
    /// Atomically: seal the current Tip (sets `sealed_at_ms`), insert the new
    /// Tip with `parent_batch_index = head.batch_index`, open its first frame.
    /// Order matters: sealing first removes the old row from the
    /// `ux_single_valid_tip` partial index, making room for the new Tip.
    #[cfg(test)]
    pub(crate) fn close_frame_and_batch(
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

    /// The artifact is durable before sealing and registering its snapshot in
    /// one transaction. A failed commit leaves only an unreferenced artifact.
    pub fn close_frame_and_batch_with_snapshot(
        &mut self,
        head: &mut WriteHead,
        next_safe_block: u64,
        dump_dir: &Path,
        batch_index: u64,
        executed_input_count: ExecutedInputCount,
    ) -> Result<()> {
        assert_eq!(
            batch_index, head.batch_index,
            "snapshot belongs to another batch"
        );
        let (next_batch_index, now_ms, policy) = self.write(|tx| {
            assert_eq!(
                next_executed_input_count_in(tx)?,
                executed_input_count,
                "application count changed between dump creation and batch close"
            );
            let result = seal_and_open_next_batch(tx, head.batch_index, next_safe_block)?;
            insert_batch_snapshot_in(tx, dump_dir, batch_index, executed_input_count)?;
            Ok(result)
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

/// Insert the Tip and its first frame. Callers separately attach the complete
/// leading application range, except for a recovery baseline's empty root.
/// Returns its local identity; callers load `WriteHead` through the shared reader.
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
/// startup recovery's guarded Tip creation (genesis / first startup) and
/// recovery's cascade (reopening the Tip it just invalidated, atomically — see
/// `storage/recovery.rs`). Lineage is derived from the tree: `parent` is the
/// highest-indexed valid batch — `None` when the valid path is empty (genesis,
/// or a fully-torn cascade), rooting a batch at the deployment anchor; `batch_index` is the
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

/// Capture the unaccounted range before creating its new frame, then attribute
/// its external directs. After launch, lane catch-up executes these rows before
/// processing queued user operations.
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

/// The folded prefix is represented by the baseline, so the root contains no
/// replay entries for it. Inputs beyond the stop block remain unaccounted.
pub(super) fn open_recovery_tip_in_tx(tx: &Transaction<'_>, stop_block: u64) -> Result<()> {
    assert!(
        load_current_write_head(tx)?.is_none(),
        "recovery tip already exists"
    );
    insert_tip_rows(tx, Some(0), None, stop_block)?;
    Ok(())
}

fn safe_input_index_exclusive_through_block_in(tx: &Transaction<'_>, block: u64) -> Result<u64> {
    let last: Option<i64> = tx
        .query_row(
            "SELECT safe_input_index FROM safe_inputs WHERE block_number <= ?1
         ORDER BY block_number DESC, safe_input_index DESC LIMIT 1",
            [saturating_query_bound(block)],
            |row| row.get(0),
        )
        .optional()?;
    Ok(last.map_or(0, |index| {
        i64_to_u64(index)
            .checked_add(1)
            .expect("safe input index overflow")
    }))
}

fn next_undrained_safe_input_index_in(tx: &Transaction<'_>) -> Result<u64> {
    let floor = query_history_state(tx)?.base_safe_block;
    let latest_frame: Option<i64> = tx
        .query_row(
            "SELECT safe_block FROM frames
         WHERE batch_index = (SELECT MAX(batch_index) FROM valid_batches)
         ORDER BY frame_in_batch DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    safe_input_index_exclusive_through_block_in(tx, floor.max(latest_frame.map_or(0, i64_to_u64)))
}

/// Seal the current Tip and open the successor batch's first frame, in `tx`.
///
/// Shared by [`Storage::close_frame_and_batch`] and
/// [`Storage::close_frame_and_batch_with_snapshot`] so the seal ordering
/// invariant lives in one place: seal first (which frees the old row from the
/// `ux_single_valid_tip` partial index), then insert the successor as the new
/// Tip. Returns the new batch index, the close timestamp, and the sampled
/// policy for the caller to apply to its in-memory write head.
fn seal_and_open_next_batch(
    tx: &Transaction<'_>,
    closing_batch_index: u64,
    next_safe_block: u64,
) -> Result<(u64, i64, BatchPolicy)> {
    let current = load_current_write_head(tx)?.expect("a batch close requires the Tip");
    assert_eq!(
        current.safe_block, next_safe_block,
        "batch closure cannot advance the frame clock"
    );
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
/// range into it. Shared by the attributed production frame-close path and
/// their application-history test siblings so the frame-rotation invariant lives in
/// one place. Returns the sampled policy for the caller to apply to the
/// in-memory write head.
fn close_frame_in(
    tx: &Transaction<'_>,
    head: &WriteHead,
    next_safe_block: u64,
    leading_direct_range: SafeInputRange,
    executions: &[DirectInputExecution],
) -> Result<BatchPolicy> {
    assert_eq!(
        leading_direct_range,
        SafeInputRange::new(
            next_undrained_safe_input_index_in(tx)?,
            safe_input_index_exclusive_through_block_in(tx, next_safe_block)?,
        ),
        "frame advancement must account for its complete L1 interval"
    );
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
fn close_frame_derived_in(
    tx: &Transaction<'_>,
    head: &WriteHead,
    next_safe_block: u64,
    leading_direct_range: SafeInputRange,
) -> Result<BatchPolicy> {
    let (policy, next_frame_in_batch) = open_successor_frame_in(tx, head, next_safe_block)?;
    persist_frame_direct_sequence_derived(
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
    assert!(
        next_safe_block >= head.safe_block,
        "frame clock cannot regress"
    );
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

/// Fixture insertion of source operations and their application positions.
#[cfg(test)]
fn insert_user_ops_batch(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    frame_pos_start: u32,
    user_ops: &[PendingUserOp],
) -> Result<()> {
    let mut next = next_executed_input_count_in(tx)?;
    insert_user_op_iter(
        tx,
        batch_index,
        frame_in_batch,
        frame_pos_start,
        user_ops.iter(),
    )?;
    for position in 0..user_ops.len() {
        tx.execute("INSERT INTO application_inputs (offset,batch_index,frame_in_batch,user_op_pos_in_frame) VALUES (?1,?2,?3,?4)",
            params![u64_to_i64(next.get()),u64_to_i64(batch_index),i64::from(frame_in_batch),
                i64::from(frame_pos_start.checked_add(u32::try_from(position).unwrap()).unwrap())])?;
        next = next.checked_next().expect("input count overflow");
    }
    Ok(())
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

/// User-op source and application position become durable before acknowledgement.
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
    let mut stmt = tx.prepare_cached(
        "INSERT INTO application_inputs (offset, batch_index, frame_in_batch, user_op_pos_in_frame)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (position, item) in user_ops.iter().enumerate() {
        let position = frame_pos_start
            .checked_add(u32::try_from(position).expect("chunk fits u32"))
            .expect("user-op position overflow");
        stmt.execute(params![
            u64_to_i64(item.executed_input_offset.get()),
            u64_to_i64(batch_index),
            i64::from(frame_in_batch),
            i64::from(position)
        ])?;
    }
    Ok(())
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
    fn snapshot_registration_failure_rolls_back_batch_close_and_cached_head() {
        let db = temp_db("snapshot-close-rollback");
        let mut storage = Storage::open(&db.path).unwrap();
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .unwrap();
        let prefix = std::path::Path::new("existing-baseline");
        storage
            .insert_baseline_snapshot(prefix, ExecutedInputCount::ZERO)
            .unwrap();
        let batch = head.batch_index;
        let error = storage
            .close_frame_and_batch_with_snapshot(
                &mut head,
                10,
                prefix,
                batch,
                ExecutedInputCount::ZERO,
            )
            .expect_err("colliding artifact name must roll back the whole close");
        assert!(error.to_string().contains("UNIQUE"));
        assert_eq!(head.batch_index, batch);
        let persisted = storage.open_state().unwrap().unwrap();
        assert_eq!(persisted.batch_index, batch);
        assert_eq!(persisted.frame_in_batch, head.frame_in_batch);
        assert_eq!(
            storage
                .conn
                .query_row("SELECT COUNT(*) FROM batches", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            storage
                .conn
                .query_row("SELECT COUNT(*) FROM valid_closed_batches", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
        assert_eq!(
            storage
                .conn
                .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    #[should_panic(expected = "frame clock cannot regress")]
    fn frame_clock_cannot_regress_even_when_the_l1_interval_has_no_inputs() {
        let db = temp_db("empty-frame-clock-regression");
        let mut storage = Storage::open(&db.path).unwrap();
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .unwrap();
        storage
            .close_frame_only_with_executions(&mut head, 9, SafeInputRange::empty_at(0), &[])
            .unwrap();
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
    fn mismatched_execution_offset_rolls_back_source_and_history() {
        let db = temp_db("user-execution-offset-atomicity");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        let included = included_user_op(0, 1);
        let error = storage
            .append_executed_user_ops_chunk(&mut head, &[included])
            .expect_err("non-canonical execution offset must fail loud");
        assert!(
            error.to_string().contains("must equal next count"),
            "unexpected trigger error: {error}"
        );

        for table in ["user_ops", "application_inputs"] {
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
            .prepare("SELECT offset FROM application_inputs ORDER BY offset")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(current, vec![0, 1]);
        let source_rows: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM user_ops", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            source_rows, 3,
            "invalidation preserves original signed operations"
        );
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
        assert_eq!(
            storage.current_fee_quote().expect("quote"),
            Some((1356, 1456)),
            "quote splits the frozen open-frame fee from the advanced recommended_fee",
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
        assert_eq!(
            storage.current_fee_quote().expect("quote"),
            Some((1456, 1456)),
            "after rotation the quote's two fees agree again",
        );
    }

    #[test]
    fn next_undrained_input_is_derived_from_accounted_frame_block() {
        let db = temp_db("safe-cursor");
        let mut storage = Storage::open(db.path.as_str()).expect("open storage");
        pin_deployment_identity(&mut storage, SENDER_A);
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
        pin_deployment_identity(&mut storage, SENDER_A);
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

        let replay = crate::storage::test_helpers::all_ordered_l2_txs(&mut storage);
        assert_eq!(replay.len(), 2);
        match &replay[0] {
            SequencedL2Tx::Direct(value) => assert_eq!(value.payload.as_slice(), &[0xaa]),
            _ => panic!("expected direct input at position 0"),
        }
        match &replay[1] {
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
        let bounds = storage.history_bounds().expect("bounds");
        let replay = storage
            .canonical_history_page(
                sequencer_core::history::HistoryClaim {
                    version: bounds.version,
                    next_input: bounds.available_from,
                },
                100,
            )
            .expect("load replay")
            .rows;
        assert_eq!(
            replay.len(),
            2,
            "leading directs are in the replay stream for catch-up"
        );
        assert_eq!(replay[0].offset, ExecutedInputCount::ZERO);
        assert_eq!(replay[1].offset, ExecutedInputCount::new(1));
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
    #[test]
    fn application_rows_require_initialized_baseline_and_a_live_tip() {
        let db = temp_db("application-history-baseline-required");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Rebuild).unwrap();
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .unwrap();
        assert!(
            storage
                .append_executed_user_ops_chunk(&mut head, &[included_user_op(0, 0)])
                .is_err()
        );
        storage
            .write(|tx| {
                super::super::history::initialize_history_in(tx, ExecutedInputCount::ZERO, 0)
            })
            .unwrap();
        storage
            .append_executed_user_ops_chunk(&mut head, &[included_user_op(0, 0)])
            .unwrap();
        storage.close_frame_and_batch(&mut head, 0).unwrap();
        let err = storage.conn.execute("INSERT INTO application_inputs (offset,batch_index,frame_in_batch,user_op_pos_in_frame) VALUES (1,0,0,0)", []).unwrap_err();
        assert!(err.to_string().contains("current valid Tip"));
        assert!(
            storage
                .conn
                .execute("UPDATE application_inputs SET offset = 1", [])
                .is_err()
        );
        assert!(
            storage
                .conn
                .execute("DELETE FROM application_inputs", [])
                .is_err()
        );
    }

    #[test]
    fn envelopes_advance_accounting_without_becoming_application_inputs() {
        let db = temp_db("envelope-only-accounting");
        let mut storage = Storage::open(&db.path).unwrap();
        pin_deployment_identity(&mut storage, SENDER_A);
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .unwrap();
        storage
            .append_safe_inputs(
                10,
                &[StoredSafeInput {
                    sender: SENDER_A,
                    payload: vec![0xff],
                    block_number: 10,
                }],
                SENDER_A,
                &default_protocol_timing(),
            )
            .unwrap();
        storage
            .close_frame_only_with_executions(&mut head, 10, SafeInputRange::new(0, 1), &[])
            .unwrap();
        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 1);
        assert_eq!(
            storage.next_executed_input_count().unwrap(),
            ExecutedInputCount::ZERO
        );
        assert!(crate::storage::test_helpers::all_ordered_l2_txs(&mut storage).is_empty());
        storage.close_frame_and_batch(&mut head, 10).unwrap();
        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 1);
    }
}
