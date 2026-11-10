// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Inclusion-lane storage: open the first batch and frame, append user-op chunks,
//! rotate frames and batches, and the lane-driven reads (classified directs, open
//! state). The public `/fee` and `/nonce` reads live here too, since they derive
//! from the lane's rows.

use std::path::Path;
use std::time::SystemTime;

use alloy_primitives::Address;
use rusqlite::{OptionalExtension, Result, Transaction, named_params, params};
use sequencer_core::l2_tx::Direct;
use sequencer_core::user_op::SignedUserOp;

use super::acceptance::{Divergence, canonical_divergence_in};
use super::convert::{
    from_unix_ms, i64_to_u16, i64_to_u32, i64_to_u64, now_unix_ms, to_unix_ms, u64_to_i64,
};
use super::history::{era_in, next_l2_tx_count_in};
use super::mutations::{
    insert_new_batch, insert_open_frame, persist_frame_direct_sequence,
    persist_frame_direct_sequence_derived, seal_batch,
};
use super::queries::{
    DIRECT_EXECUTES_SQL, DirectOperands, current_safe_block_required, load_current_write_head,
    query_batch_policy, query_latest_safe_input_index_exclusive, safe_input_count_through_block_in,
};
use super::snapshots::insert_batch_snapshot_in;
use super::{
    BatchPolicy, DirectInputExecution, L2TxCount, SafeFrontierState, SafeInputFrontier,
    SafeInputRange, Storage, WriteHead,
};

/// One executed user op as storage persists it: the signed op, when the API
/// received it, and its application-history position. Borrows the caller's op.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExecutedUserOp<'a> {
    pub signed: &'a SignedUserOp,
    pub received_at: SystemTime,
    pub l2_tx_index: L2TxCount,
}

/// The `/fee` quote. Both fees are log-space exponents (base 129/128).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeQuote {
    /// The fee committed for the Tip's open frame.
    pub frame_fee: u16,
    /// The fee the next frame opens at under the current policy.
    pub recommended_fee: u16,
}

const LAST_VALID_USER_NONCE_SQL: &str = "SELECT u.nonce FROM user_ops u
     WHERE u.sender = ?1
       AND EXISTS (SELECT 1 FROM valid_batches b WHERE b.batch_index = u.batch_index)
     ORDER BY u.nonce DESC
     LIMIT 1";

impl Storage {
    /// First L1 input beyond the latest surviving frame's accounted block, or
    /// the era's first queued input while no frame of the era survives.
    pub fn next_undrained_safe_input_index(&mut self) -> Result<u64> {
        self.read(next_undrained_safe_input_index_in)
    }

    /// Resume the lane on startup. Returns `None` if storage is empty (caller
    /// must establish the Tip through startup recovery).
    pub fn open_state(&mut self) -> Result<Option<WriteHead>> {
        self.read(load_current_write_head)
    }

    /// The `/fee` quote: the fee of the Tip's latest frame and the policy's
    /// current `recommended_fee`, in one read. `None` if there is no Tip.
    pub fn current_fee_quote(&mut self) -> Result<Option<FeeQuote>> {
        self.read(|tx| {
            let tip: Option<i64> = tx
                .query_row("SELECT batch_index FROM valid_open_batch", [], |row| {
                    row.get(0)
                })
                .optional()?;
            let Some(tip) = tip else {
                return Ok(None);
            };
            // Every valid batch has a frame; a missing one fails loud.
            let frame_fee: i64 = tx.query_row(
                "SELECT fee FROM frames WHERE batch_index = ?1 \
                 ORDER BY frame_in_batch DESC LIMIT 1",
                params![tip],
                |row| row.get(0),
            )?;
            Ok(Some(FeeQuote {
                frame_fee: i64_to_u16(frame_fee),
                recommended_fee: query_batch_policy(tx)?.recommended_fee,
            }))
        })
    }

    /// The nonce `sender` must sign next: one past its latest user op in a
    /// valid batch, or 0 without one. Exact under the application contract's
    /// nonce rule, except for a sender whose last op predates the era's live
    /// rows (in its rebuilt history or below its base): that sender reads a
    /// stale value, and the 422's `expected` nonce corrects the client.
    ///
    /// Invalidated batches keep their rows, and resubmission after a cascade
    /// reuses their nonces, so the validity filter is load-bearing.
    pub fn next_user_nonce(&mut self, sender: Address) -> Result<u32> {
        self.read(|tx| {
            let last_nonce: Option<i64> = tx
                .prepare_cached(LAST_VALID_USER_NONCE_SQL)?
                .query_row(params![sender.as_slice()], |row| row.get(0))
                .optional()?;
            Ok(last_nonce.map_or(0, |nonce| {
                i64_to_u32(nonce)
                    .checked_add(1)
                    .expect("included user nonce u32::MAX has no successor: contract-impossible")
            }))
        })
    }

    /// Snapshot the current L1 reconciliation state. A canonical-divergence
    /// marker outranks and withholds the otherwise-usable frontier.
    ///
    /// **Precondition:** at least one safe-head observation must have been
    /// recorded. The lane only starts after the recovery procedure admits,
    /// which guarantees this in production.
    pub fn safe_frontier_state(&mut self) -> Result<SafeFrontierState> {
        self.read(|tx| {
            if let Some(Divergence {
                nonce,
                safe_input_index,
            }) = canonical_divergence_in(tx)?
            {
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

    /// The directs in `range` that execute, in input order, each with the
    /// index and timestamp the application receives. A direct over the
    /// pinned cap is left out, as storage's derivation of the frame's
    /// executions leaves it out.
    pub(crate) fn fill_direct_inputs(
        &mut self,
        range: SafeInputRange,
        out: &mut Vec<Direct>,
    ) -> Result<()> {
        out.clear();
        if range.is_empty() {
            return Ok(());
        }
        let operands = DirectOperands::load(&self.conn)?;
        let mut statement = self.conn.prepare_cached(&format!(
            "SELECT safe_input_index, sender, payload, block_number, block_timestamp
             FROM safe_inputs
             WHERE safe_input_index >= :start AND safe_input_index < :end
               AND {DIRECT_EXECUTES_SQL}
             ORDER BY safe_input_index"
        ))?;
        let rows = statement.query_map(
            operands
                .bind(named_params! {
                    ":start": u64_to_i64(range.start()),
                    ":end": u64_to_i64(range.end()),
                })
                .as_slice(),
            |row| {
                let sender: Vec<u8> = row.get(1)?;
                Ok(Direct {
                    input_index: i64_to_u64(row.get(0)?),
                    sender: Address::from_slice(&sender),
                    block: i64_to_u64(row.get(3)?),
                    timestamp: i64_to_u64(row.get(4)?),
                    payload: row.get(2)?,
                })
            },
        )?;
        for row in rows {
            out.push(row?);
        }
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
    pub(crate) fn append_executed_user_ops_chunk<'a>(
        &mut self,
        head: &mut WriteHead,
        user_ops: impl ExactSizeIterator<Item = ExecutedUserOp<'a>> + Clone,
    ) -> Result<()> {
        if user_ops.len() == 0 {
            return Ok(());
        }
        // Validate both in-memory counter advances before the transaction.
        // Otherwise an overflow panic after commit would leave durable rows
        // that the unchanged `WriteHead` cannot describe.
        let mut next_head = head.duplicate();
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

    /// Commit frame advancement together with its application inputs, which
    /// must be exactly the directs the range executes.
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

    /// The artifact is durable before sealing and registering its snapshot in
    /// one transaction. A failed commit leaves only an unreferenced artifact.
    /// The successor batch's first frame opens at the closing frame's safe
    /// block: a batch close never advances the frame clock.
    pub fn close_frame_and_batch_with_snapshot(
        &mut self,
        head: &mut WriteHead,
        dump_dir: &Path,
        l2_tx_count: L2TxCount,
    ) -> Result<()> {
        let next = self.write(|tx| {
            assert_eq!(
                next_l2_tx_count_in(tx)?,
                l2_tx_count,
                "application count changed between dump creation and batch close"
            );
            let next = seal_and_open_next_batch(tx, head)?;
            insert_batch_snapshot_in(tx, dump_dir, head.batch_index, l2_tx_count)?;
            Ok(next)
        })?;
        next.apply_to(head);
        Ok(())
    }
}

/// Insert the Tip and its first frame. Callers separately attach the complete
/// leading application range. Returns its local identity; callers load
/// `WriteHead` through the shared reader.
pub(super) fn insert_tip_rows(
    tx: &Transaction<'_>,
    batch_index_opt: Option<u64>,
    safe_block: u64,
) -> Result<u64> {
    let now_ms = now_unix_ms();
    let policy = query_batch_policy(tx)?;
    let (batch_index, _nonce) = insert_new_batch(tx, batch_index_opt, now_ms)?;
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
/// draining every undrained safe input into its first frame: each direct that
/// executes gets its row there (lane catch-up
/// executes them, I11).
///
/// One mechanism, two callers with distinct intents (each keeps its own guard):
/// startup recovery's guarded Tip creation (genesis / first startup) and
/// recovery's cascade (reopening the Tip it just invalidated, atomically — see
/// `storage/recovery.rs`). The new Tip follows the last valid batch, at the
/// era's anchor nonce when none is valid (genesis, or a fully-torn cascade);
/// `batch_index` is the explicit genesis `0` only when the `batches` table is
/// empty, otherwise the monotonic PK (so recovery batches keep climbing and
/// indices are never reused).
pub(super) fn open_fresh_tip_in_tx(tx: &Transaction<'_>) -> Result<()> {
    let safe_block = current_safe_block_required(tx)?;
    let table_empty = tx
        .query_row("SELECT MAX(batch_index) FROM batches", [], |row| {
            row.get::<_, Option<i64>>(0)
        })?
        .is_none();
    let batch_index_opt = if table_empty { Some(0) } else { None };
    insert_draining_tip_with_executions(
        tx,
        batch_index_opt,
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
    safe_block: u64,
    drain_upper: u64,
) -> Result<()> {
    let leading_direct_range =
        SafeInputRange::new(next_undrained_safe_input_index_in(tx)?, drain_upper);
    let batch_index = insert_tip_rows(tx, batch_index, safe_block)?;
    persist_frame_direct_sequence_derived(tx, batch_index, 0, leading_direct_range)?;
    Ok(())
}

/// The era's baseline accounts for the inputs through the stop block except
/// the directs still queued there, which the era's first frame drains: from
/// the first queued input, past submitter and over-cap inputs that never
/// execute. A cascade that tears every batch of the era drains from there too.
fn next_undrained_safe_input_index_in(tx: &Transaction<'_>) -> Result<u64> {
    let era = era_in(tx)?;
    let latest_frame: Option<i64> = tx
        .query_row(
            "SELECT safe_block FROM frames
         WHERE batch_index = (SELECT MAX(batch_index) FROM valid_batches)
         ORDER BY frame_in_batch DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match latest_frame {
        Some(frame_block) => {
            safe_input_count_through_block_in(tx, era.stop_block.max(i64_to_u64(frame_block)))
        }
        None => Ok(era.first_queued_input),
    }
}

/// The successor batch a close opened, for the caller to apply to its
/// in-memory write head after the commit.
pub(super) struct NextBatch {
    batch_index: u64,
    nonce: u64,
    created_at_ms: i64,
    policy: BatchPolicy,
}

impl NextBatch {
    pub(super) fn apply_to(self, head: &mut WriteHead) {
        head.move_to_next_batch(
            self.batch_index,
            self.nonce,
            from_unix_ms(self.created_at_ms),
            self.policy,
        );
    }
}

/// Seal the current Tip and open the successor batch's first frame at the
/// head's safe block, in `tx`.
///
/// Shared with the test fixture `close_frame_and_batch` so the seal ordering
/// lives in one place: seal first (which frees the old row from the
/// `ux_single_valid_tip` partial index), then insert the successor as the new
/// Tip.
pub(super) fn seal_and_open_next_batch(
    tx: &Transaction<'_>,
    head: &WriteHead,
) -> Result<NextBatch> {
    assert_head_matches_tip(tx, head)?;
    let now_ms = now_unix_ms();
    // Batch policy is sampled here: the derived fee is committed to the newly
    // opened frame, and the batch size target is stored on the write head.
    let policy = query_batch_policy(tx)?;
    // Hash-at-seal: encode the closing batch's wire bytes via
    // the same path the submitter uses and stamp their keccak256 on the row,
    // atomically with the seal. The content-identity check later compares
    // accepted L1 landings against this hash.
    let nonce = super::snapshots::batch_nonce_in(tx, head.batch_index)?;
    assert_eq!(
        nonce, head.batch_nonce,
        "the sealed batch's nonce differs from the write head's"
    );
    let frames = super::l1_submission::load_batch_frames_in(tx, head.batch_index)?;
    let encoded = ssz::Encode::as_ssz_bytes(&sequencer_core::batch::Batch { nonce, frames });
    let payload_hash = alloy_primitives::keccak256(&encoded);
    seal_batch(tx, head.batch_index, now_ms, &payload_hash.0)?;
    let (next_batch_index, next_nonce) = insert_new_batch(tx, None, now_ms)?;
    insert_open_frame(
        tx,
        next_batch_index,
        0,
        now_ms,
        policy.recommended_fee,
        head.safe_block,
    )?;
    Ok(NextBatch {
        batch_index: next_batch_index,
        nonce: next_nonce,
        created_at_ms: now_ms,
        policy,
    })
}

/// Fail loud unless the Tip reloaded inside `tx` agrees with `head` on every
/// persisted fact: batch, nonce, frame, counts, fee and safe block. The size
/// target is left out: a reload samples the policy now, while the head keeps
/// the value sampled when its frame opened. Runs once per rotation or close.
fn assert_head_matches_tip(tx: &Transaction<'_>, head: &WriteHead) -> Result<()> {
    let durable =
        load_current_write_head(tx)?.expect("a frame rotation or batch close requires the Tip");
    let persisted = |head: &WriteHead| {
        (
            head.batch_index,
            head.batch_nonce,
            head.frame_in_batch,
            head.batch_user_op_count,
            head.open_frame_user_op_count,
            head.frame_fee,
            head.safe_block,
        )
    };
    assert!(
        persisted(head) == persisted(&durable),
        "write head disagrees with the durable Tip: cached={head:?}, durable={durable:?}"
    );
    Ok(())
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
            safe_input_count_through_block_in(tx, next_safe_block)?,
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

pub(super) fn open_successor_frame_in(
    tx: &Transaction<'_>,
    head: &WriteHead,
    next_safe_block: u64,
) -> Result<(BatchPolicy, u32)> {
    assert_head_matches_tip(tx, head)?;
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

fn insert_user_op_iter<'a>(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    frame_pos_start: u32,
    user_ops: impl Iterator<Item = ExecutedUserOp<'a>>,
) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO user_ops (
            batch_index, frame_in_batch, pos_in_frame,
            sender, nonce, max_fee, data, sig, received_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for (offset, item) in user_ops.enumerate() {
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
fn insert_executed_user_ops_batch<'a>(
    tx: &Transaction<'_>,
    batch_index: u64,
    frame_in_batch: u32,
    frame_pos_start: u32,
    user_ops: impl Iterator<Item = ExecutedUserOp<'a>> + Clone,
) -> Result<()> {
    insert_user_op_iter(
        tx,
        batch_index,
        frame_in_batch,
        frame_pos_start,
        user_ops.clone(),
    )?;
    let mut stmt = tx.prepare_cached(
        "INSERT INTO application_inputs (l2_tx_count, batch_index, frame_in_batch, user_op_pos_in_frame)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (position, item) in user_ops.enumerate() {
        let position = frame_pos_start
            .checked_add(u32::try_from(position).expect("chunk fits u32"))
            .expect("user-op position overflow");
        stmt.execute(params![
            u64_to_i64(item.l2_tx_index.get()),
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
        DirectInputExecution, FeeQuote, IngestedSafeInput, L2TxCount, LifecycleCommand,
        SafeFrontierState, SafeInputFrontier, SafeInputRange, Storage, StoredSafeInput, WriteHead,
        test_helpers::{
            SENDER_A, TEST_MAX_DIRECT_PAYLOAD_BYTES, WriteHeadParts, default_protocol_timing,
            record_canonical_divergence, set_up_storage,
        },
    };
    use crate::test_support::{pin_test_deployment_identity, temp_db};
    use alloy_primitives::{Address, B256, Signature};
    use sequencer_core::l2_tx::{Direct, L2Tx};
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
            l2_tx_index: L2TxCount::new(offset),
        }
    }

    #[test]
    fn snapshot_registration_failure_rolls_back_batch_close_and_cached_head() {
        let db = temp_db("snapshot-close-rollback");
        // No baseline yet: the test registers its own at a known prefix.
        let mut storage = Storage::open_with_era(&db.path, &crate::storage::NewEra::EMPTY_GENESIS);
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .unwrap();
        let prefix = std::path::Path::new("existing-baseline");
        storage
            .insert_baseline_snapshot(prefix, L2TxCount::ZERO)
            .unwrap();
        let batch = head.batch_index();
        let error = storage
            .close_frame_and_batch_with_snapshot(&mut head, prefix, L2TxCount::ZERO)
            .expect_err("colliding artifact name must roll back the whole close");
        assert!(error.to_string().contains("UNIQUE"));
        assert_eq!(head.batch_index(), batch);
        let persisted = storage.open_state().unwrap().unwrap();
        assert_eq!(persisted.batch_index(), batch);
        assert_eq!(persisted.frame_in_batch(), head.frame_in_batch());
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

    /// Heads that each disagree with the durable Tip on one persisted fact:
    /// an older batch, a wrong nonce, an older or later frame, a position past
    /// the frame's rows, a batch count, a fee or a safe block.
    fn stale_heads(head: &WriteHead) -> Vec<(&'static str, WriteHead)> {
        let live = WriteHeadParts::of(head);
        let stale = |name, parts: WriteHeadParts| (name, parts.build());
        vec![
            stale(
                "batch",
                WriteHeadParts {
                    batch_index: live.batch_index - 1,
                    ..live
                },
            ),
            stale(
                "nonce",
                WriteHeadParts {
                    batch_nonce: live.batch_nonce + 1,
                    ..live
                },
            ),
            stale(
                "older frame",
                WriteHeadParts {
                    frame_in_batch: live.frame_in_batch - 1,
                    ..live
                },
            ),
            stale(
                "later frame",
                WriteHeadParts {
                    frame_in_batch: live.frame_in_batch + 1,
                    ..live
                },
            ),
            stale(
                "position",
                WriteHeadParts {
                    open_frame_user_op_count: live.open_frame_user_op_count + 1,
                    ..live
                },
            ),
            stale(
                "batch count",
                WriteHeadParts {
                    batch_user_op_count: live.batch_user_op_count + 1,
                    ..live
                },
            ),
            stale(
                "fee",
                WriteHeadParts {
                    frame_fee: live.frame_fee + 1,
                    ..live
                },
            ),
            stale(
                "safe block",
                WriteHeadParts {
                    safe_block: live.safe_block + 1,
                    ..live
                },
            ),
        ]
    }

    /// A Tip at batch 1, frame 1 of the batch, with one user op in each frame.
    fn tip_past_genesis(name: &str) -> (crate::test_support::TestDb, Storage, WriteHead) {
        let db = temp_db(name);
        let mut storage = set_up_storage(&db.path, SENDER_A);
        let mut head = storage
            .initialize_open_state(10, SafeInputRange::empty_at(0))
            .unwrap();
        storage.close_frame_and_batch(&mut head).unwrap();
        storage
            .append_included_user_ops(&mut head, &[included_user_op(0, 0)])
            .unwrap();
        storage
            .close_frame_only(&mut head, 10, SafeInputRange::empty_at(0))
            .unwrap();
        storage
            .append_included_user_ops(&mut head, &[included_user_op(1, 1)])
            .unwrap();
        (db, storage, head)
    }

    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default()
    }

    #[test]
    fn a_head_that_disagrees_with_the_tip_fails_loud_at_frame_rotation() {
        let (_db, mut storage, head) = tip_past_genesis("stale-head-rotation");
        for (name, mut stale) in stale_heads(&head) {
            let panic = catch_unwind(AssertUnwindSafe(|| {
                let _ = storage.close_frame_only_with_executions(
                    &mut stale,
                    10,
                    SafeInputRange::empty_at(0),
                    &[],
                );
            }))
            .expect_err(name);
            assert!(
                panic_message(panic).contains("write head disagrees with the durable Tip"),
                "{name}"
            );
        }
        assert_eq!(storage.open_state().unwrap().unwrap().frame_in_batch(), 1);
        let mut head = head;
        storage
            .close_frame_only_with_executions(&mut head, 10, SafeInputRange::empty_at(0), &[])
            .expect("the live head still rotates");
    }

    #[test]
    fn a_head_that_disagrees_with_the_tip_fails_loud_at_batch_close() {
        let (_db, mut storage, head) = tip_past_genesis("stale-head-close");
        let count = storage.next_l2_tx_count().unwrap();
        for (name, mut stale) in stale_heads(&head) {
            let panic = catch_unwind(AssertUnwindSafe(|| {
                let _ = storage.close_frame_and_batch_with_snapshot(
                    &mut stale,
                    std::path::Path::new("/snapshot/stale"),
                    count,
                );
            }))
            .expect_err(name);
            assert!(
                panic_message(panic).contains("write head disagrees with the durable Tip"),
                "{name}"
            );
        }
        assert_eq!(storage.latest_batch_index().unwrap(), Some(1));
        let mut head = head;
        storage
            .close_frame_and_batch_with_snapshot(
                &mut head,
                std::path::Path::new("/snapshot/live"),
                count,
            )
            .expect("the live head still closes");
        assert_eq!(head.batch_index(), crate::storage::BatchId::from_raw(2));
    }

    /// The head carries its batch's nonce from the loader and through every
    /// close, and a reload agrees.
    #[test]
    fn the_head_carries_the_open_batch_nonce() {
        let db = temp_db("head-batch-nonce");
        let mut storage = Storage::open_with_era(
            &db.path,
            &crate::storage::NewEra {
                anchor_batch_nonce: 7,
                ..crate::storage::NewEra::EMPTY_GENESIS
            },
        );
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .unwrap();
        assert_eq!(
            head.batch_nonce(),
            7,
            "the era's first batch is at the anchor"
        );
        storage.close_frame_and_batch(&mut head).unwrap();
        assert_eq!(head.batch_nonce(), 8);
        let count = storage.next_l2_tx_count().unwrap();
        storage
            .close_frame_and_batch_with_snapshot(&mut head, std::path::Path::new("/s/9"), count)
            .unwrap();
        assert_eq!(head.batch_nonce(), 9);
        assert_eq!(storage.open_state().unwrap().unwrap().batch_nonce(), 9);
    }

    #[test]
    #[should_panic(expected = "frame clock cannot regress")]
    fn frame_clock_cannot_regress_even_when_the_l1_interval_has_no_inputs() {
        let db = temp_db("empty-frame-clock-regression");
        let mut storage = set_up_storage(&db.path, SENDER_A);
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
        let mut storage = set_up_storage(&db.path, SENDER_A);

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

        assert_eq!(head_a.batch_index(), head_b.batch_index());
        assert_eq!(head_a.frame_in_batch(), head_b.frame_in_batch());
        assert_eq!(head_a.frame_fee(), head_b.frame_fee());
        // Default log_recommended_fee = 0+296+20+419+621 = 1356
        assert_eq!(head_a.frame_fee(), 1356);

        let batch_b = head_b.batch_index();
        let mut head = head_b;
        let next_safe_block = head.safe_block();
        storage
            .close_frame_only(&mut head, next_safe_block, SafeInputRange::empty_at(0))
            .expect("rotate within same batch");
        assert_eq!(head.batch_index(), batch_b);
        assert_eq!(head.frame_in_batch(), 1);

        storage
            .close_frame_and_batch(&mut head)
            .expect("close batch and rotate");
        assert!(head.batch_index() > batch_b);
        assert_eq!(head.frame_in_batch(), 0);
    }

    #[test]
    fn safe_frontier_state_withholds_poisoned_projection() {
        let db = temp_db("safe-frontier-state-poison");
        let mut storage = set_up_storage(&db.path, SENDER_A);
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
        let mut storage = set_up_storage(&db.path, SENDER_A);
        let head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let mut head = WriteHeadParts {
            open_frame_user_op_count: u32::MAX,
            ..WriteHeadParts::of(&head)
        }
        .build();

        let included = included_user_op(0, 0);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = storage.append_included_user_ops(&mut head, &[included]);
        }));
        assert!(panic.is_err(), "counter overflow must fail loud");

        let persisted: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM user_ops", [], |row| row.get(0))
            .expect("count user ops");
        assert_eq!(persisted, 0, "overflow must occur before any durable write");
        assert_eq!(
            head.open_frame_user_op_count(),
            u32::MAX,
            "the authoritative head must remain unchanged"
        );
    }

    #[test]
    fn mismatched_execution_offset_rolls_back_source_and_history() {
        let db = temp_db("user-execution-offset-atomicity");
        let mut storage = set_up_storage(&db.path, SENDER_A);
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        let included = included_user_op(0, 1);
        let error = storage
            .append_included_user_ops(&mut head, &[included])
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
            storage.next_l2_tx_count().expect("next count"),
            L2TxCount::ZERO
        );
    }

    #[test]
    fn live_direct_rotation_requires_complete_classified_attribution() {
        let db = temp_db("direct-execution-attribution-complete");
        let mut storage = set_up_storage(&db.path, SENDER_A);
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
            l2_tx_index: L2TxCount::ZERO,
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
        assert_eq!(head.frame_in_batch(), 0);
        let frames: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM frames", [], |row| row.get(0))
            .unwrap();
        assert_eq!(frames, 1, "failed rotation must roll back its new frame");

        let complete = [
            DirectInputExecution {
                safe_input_index: 0,
                l2_tx_index: L2TxCount::ZERO,
            },
            DirectInputExecution {
                safe_input_index: 1,
                l2_tx_index: L2TxCount::new(1),
            },
        ];
        storage
            .close_frame_only_with_executions(&mut head, 10, SafeInputRange::new(0, 2), &complete)
            .expect("commit complete direct attribution");
        assert_eq!(storage.next_l2_tx_count().unwrap(), L2TxCount::new(2));
    }

    #[test]
    fn invalidation_rewinds_and_replacement_reuses_logical_offset() {
        let db = temp_db("execution-offset-invalidation-reuse");
        let mut storage = set_up_storage(&db.path, SENDER_A);
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        storage
            .append_included_user_ops(&mut head, &[included_user_op(0, 0)])
            .expect("append gold input");
        storage
            .close_frame_and_batch(&mut head)
            .expect("seal gold batch");
        storage
            .append_included_user_ops(&mut head, &[included_user_op(1, 1)])
            .expect("append doomed input");
        assert_eq!(
            storage.next_l2_tx_count().expect("pre-recovery count"),
            L2TxCount::new(2)
        );

        storage
            .append_safe_inputs(1_500, &[], SENDER_A, &default_protocol_timing())
            .expect("advance safe head");
        assert_eq!(
            storage
                .recover_aging_tip(1_200)
                .expect("invalidate stale Tip"),
            crate::storage::test_helpers::batch_ids(&[1])
        );
        assert_eq!(
            storage.next_l2_tx_count().expect("rewound count"),
            L2TxCount::new(1)
        );

        let mut replacement = storage.open_state().unwrap().unwrap();
        storage
            .append_included_user_ops(&mut replacement, &[included_user_op(1, 1)])
            .expect("reuse invalidated logical offset");

        let current: Vec<i64> = storage
            .conn
            .prepare("SELECT l2_tx_count FROM application_inputs ORDER BY l2_tx_count")
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
    fn saturated_block_bound_counts_every_representable_block() {
        let db = temp_db("saturated-block-bound");
        let mut storage = set_up_storage(&db.path, SENDER_A);
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
            .read(|tx| super::safe_input_count_through_block_in(tx, u64::MAX))
            .expect("query upper bound above SQLite range");
        assert_eq!(
            boundary, 1,
            "a saturated upper predicate must include every representable block"
        );
    }

    #[test]
    fn next_frame_fee_comes_from_batch_policy() {
        let db = temp_db("batch-policy-fee");
        let mut storage = set_up_storage(&db.path, SENDER_A);
        let policy = storage.batch_policy().expect("default policy");
        // Default: log_gas_price=0, log_recommended_fee = 0+296+20+419+621 = 1356
        assert_eq!(policy.recommended_fee, 1356);

        storage.set_log_gas_price(100).expect("set log gas price");

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        storage
            .close_frame_and_batch(&mut head)
            .expect("rotate batch");

        let policy = storage.batch_policy().expect("read policy");
        // log_recommended_fee = 100+296+20+419+621 = 1456
        assert_eq!(head.frame_fee(), 1456);
        assert_eq!(head.frame_fee(), policy.recommended_fee);
        assert!(
            head.max_batch_user_op_bytes() > 0,
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
        let mut storage = set_up_storage(&db.path, SENDER_A);

        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let original_batch_index = head.batch_index();
        let original_frame_in_batch = head.frame_in_batch();
        // Default: log_gas_price=0 → log_recommended_fee = 0+296+20+419+621 = 1356
        assert_eq!(head.frame_fee(), 1356);

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
                rusqlite::params![
                    original_batch_index.to_raw() as i64,
                    original_frame_in_batch as i64,
                ],
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
            head.frame_fee(),
            1356,
            "WriteHead.frame_fee must stay stable until advance_frame runs",
        );
        assert_eq!(
            storage.current_fee_quote().expect("quote"),
            Some(FeeQuote {
                frame_fee: 1356,
                recommended_fee: 1456
            }),
            "quote splits the frozen open-frame fee from the advanced recommended_fee",
        );

        // Closing the frame picks up the new policy — the *next* frame opens
        // at 1456. This is the expected policy-flow boundary.
        let next_safe_block = head.safe_block();
        storage
            .close_frame_only(&mut head, next_safe_block, SafeInputRange::empty_at(0))
            .expect("rotate within same batch");
        assert_eq!(
            head.frame_fee(),
            1456,
            "the next frame must use the updated policy's fee (policy flows in at close)",
        );
        assert_eq!(
            storage.current_fee_quote().expect("quote"),
            Some(FeeQuote {
                frame_fee: 1456,
                recommended_fee: 1456
            }),
            "after rotation the quote's two fees agree again",
        );
    }

    #[test]
    fn next_undrained_input_is_derived_from_accounted_frame_block() {
        let db = temp_db("safe-cursor");
        let mut storage = set_up_storage(&db.path, SENDER_A);
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

    /// The era's root frame drains the directs still queued at the stop
    /// block, from the first queued input, and a cascade that tears every
    /// batch of the era drains from there again.
    #[test]
    fn the_era_root_drains_from_its_first_queued_input() {
        use crate::storage::test_helpers::{RecoveryFixture, all_ordered_l2_txs, era};
        let db = temp_db("era-root-drain");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
        pin_test_deployment_identity(&mut storage, SENDER_A);
        let input = |sender, payload: Vec<u8>, block_number| StoredSafeInput {
            sender,
            payload,
            block_number,
        };
        let direct = Address::repeat_byte(0x22);
        storage
            .append_safe_inputs_with_timestamp(
                12,
                12,
                &[
                    // Executed before the base.
                    input(direct, vec![0], 5),
                    // Queued at the stop block 10, from input 1.
                    input(direct, vec![1], 8),
                    input(SENDER_A, vec![0xff], 8),
                    input(
                        direct,
                        vec![2; TEST_MAX_DIRECT_PAYLOAD_BYTES as usize + 1],
                        9,
                    ),
                    input(direct, vec![4], 10),
                    // Past the stop block.
                    input(direct, vec![5], 12),
                ],
                SENDER_A,
                &default_protocol_timing(),
            )
            .unwrap();
        storage
            .publish_era(
                std::path::Path::new("/snapshot/era"),
                &crate::storage::NewEra {
                    first_queued_input: 1,
                    anchor_batch_nonce: 6,
                    ..era(1, 1, 10)
                },
            )
            .unwrap();
        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 1);

        let drained_directs = |storage: &mut Storage| -> Vec<u64> {
            all_ordered_l2_txs(storage)
                .into_iter()
                .map(|tx| match tx {
                    L2Tx::Direct(direct) => direct.input_index,
                    other => panic!("expected a direct, got {other:?}"),
                })
                .collect()
        };
        storage.ensure_open_tip().unwrap();
        let root = storage.open_state().unwrap().unwrap();
        assert_eq!(root.safe_block(), 12);
        assert_eq!(drained_directs(&mut storage), vec![1, 4, 5]);
        assert_eq!(storage.next_l2_tx_count().unwrap().get(), 4);
        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 6);

        storage.advance_safe_head(40);
        assert_eq!(
            storage.recover_aging_tip(10).unwrap(),
            vec![root.batch_index()]
        );
        let reopened = storage.open_state().unwrap().unwrap();
        assert_ne!(reopened.batch_index(), root.batch_index());
        assert_eq!(drained_directs(&mut storage), vec![1, 4, 5]);
        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 6);
    }

    #[test]
    fn initialize_open_state_creates_first_real_batch_and_frame() {
        let db = temp_db("initialize-open-state");
        let mut storage = set_up_storage(&db.path, SENDER_A);

        let head = storage
            .initialize_open_state(12, SafeInputRange::empty_at(0))
            .expect("initialize open state");

        assert_eq!(head.batch_index(), crate::storage::BatchId::from_raw(0));
        assert_eq!(head.frame_in_batch(), 0);
        assert_eq!(head.safe_block(), 12);

        let loaded = storage
            .open_state()
            .expect("load open state")
            .expect("open state should exist");
        assert_eq!(loaded.batch_index(), crate::storage::BatchId::from_raw(0));
        assert_eq!(loaded.frame_in_batch(), 0);
        assert_eq!(loaded.safe_block(), 12);
    }

    #[test]
    fn replay_returns_direct_inputs_in_drain_order() {
        let db = temp_db("replay-order");
        let mut storage = set_up_storage(&db.path, SENDER_A);
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
            L2Tx::Direct(value) => {
                assert_eq!(value.payload.as_slice(), &[0xaa]);
                assert_eq!((value.input_index, value.block), (0, 10));
            }
            _ => panic!("expected direct input at position 0"),
        }
        match &replay[1] {
            L2Tx::Direct(value) => {
                assert_eq!(value.payload.as_slice(), &[0xbb]);
                assert_eq!((value.input_index, value.block), (1, 10));
            }
            _ => panic!("expected direct input at position 1"),
        }
    }

    #[test]
    fn ensure_open_tip_opens_genesis_and_sequences_leading_range() {
        let db = temp_db("ensure-tip-genesis");
        let mut storage = set_up_storage(&db.path, SENDER_A);

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
        assert_eq!(
            head.batch_index(),
            crate::storage::BatchId::from_raw(0),
            "genesis batch is index 0"
        );
        assert_eq!(head.frame_in_batch(), 0);
        assert_eq!(
            head.safe_block(),
            10,
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
        assert_eq!(replay[0].offset, L2TxCount::ZERO);
        assert_eq!(replay[1].offset, L2TxCount::new(1));
    }

    #[test]
    fn ensure_open_tip_is_noop_when_tip_already_exists() {
        let db = temp_db("ensure-tip-noop");
        let mut storage = set_up_storage(&db.path, SENDER_A);

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
            head.batch_index(),
            seeded.batch_index(),
            "warm path leaves the existing Tip, does not reopen"
        );
        assert_eq!(
            head.safe_block(),
            5,
            "existing frame's safe_block is preserved, not refreshed"
        );
    }
    #[test]
    fn application_rows_require_initialized_baseline_and_a_live_tip() {
        let db = temp_db("application-history-baseline-required");
        let mut storage =
            Storage::initialize_for_command(&db.path, LifecycleCommand::Setup).unwrap();
        // Before the era: no Tip can root, and no application row lands.
        assert!(matches!(
            storage.initialize_open_state(0, SafeInputRange::empty_at(0)),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        let err = storage
            .conn
            .execute(
                "INSERT INTO application_inputs \
                 (l2_tx_count, batch_index, frame_in_batch, user_op_pos_in_frame) \
                 VALUES (0, 0, 0, 0)",
                [],
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "application history baseline is missing");
        storage
            .write(|tx| {
                super::super::history::insert_era_in(tx, &crate::storage::NewEra::EMPTY_GENESIS)
            })
            .unwrap();
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .unwrap();
        storage
            .append_included_user_ops(&mut head, &[included_user_op(0, 0)])
            .unwrap();
        storage.close_frame_and_batch(&mut head).unwrap();
        let err = storage.conn.execute("INSERT INTO application_inputs (l2_tx_count,batch_index,frame_in_batch,user_op_pos_in_frame) VALUES (1,0,0,0)", []).unwrap_err();
        assert!(err.to_string().contains("current valid Tip"));
        assert!(
            storage
                .conn
                .execute("UPDATE application_inputs SET l2_tx_count = 1", [])
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
        let mut storage = set_up_storage(&db.path, SENDER_A);
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
        assert_eq!(storage.next_l2_tx_count().unwrap(), L2TxCount::ZERO);
        assert!(crate::storage::test_helpers::all_ordered_l2_txs(&mut storage).is_empty());
        storage.close_frame_and_batch(&mut head).unwrap();
        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 1);
    }

    fn included_user_op_from(sender: Address, nonce: u32, offset: u64) -> IncludedUserOp {
        let mut included = included_user_op(nonce, offset);
        included.pending.signed.sender = sender;
        included
    }

    #[test]
    fn next_user_nonce_follows_the_latest_valid_op_across_a_cascade() {
        let alice = Address::repeat_byte(0xa1);
        let bob = Address::repeat_byte(0xb0);
        let db = temp_db("next-user-nonce");
        let mut storage = set_up_storage(&db.path, SENDER_A);
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .unwrap();
        assert_eq!(storage.next_user_nonce(alice).unwrap(), 0, "unseen sender");

        storage
            .append_included_user_ops(
                &mut head,
                &[
                    included_user_op_from(alice, 0, 0),
                    included_user_op_from(alice, 1, 1),
                    included_user_op_from(bob, 0, 2),
                ],
            )
            .unwrap();
        storage.close_frame_and_batch(&mut head).unwrap();
        storage
            .append_included_user_ops(
                &mut head,
                &[
                    included_user_op_from(alice, 2, 3),
                    included_user_op_from(alice, 3, 4),
                ],
            )
            .unwrap();
        assert_eq!(storage.next_user_nonce(alice).unwrap(), 4);
        assert_eq!(storage.next_user_nonce(bob).unwrap(), 1);

        // A cascade rolls Alice back; her invalidated rows stay for audit.
        storage
            .insert_invalid_batch(head.batch_index().to_raw())
            .unwrap();
        assert_eq!(storage.next_user_nonce(alice).unwrap(), 2);
        assert_eq!(storage.next_user_nonce(bob).unwrap(), 1);

        // Resubmitting nonce 2 leaves a duplicate (sender, nonce) pair across
        // the invalid and valid batches; only the valid one counts.
        storage
            .append_safe_inputs(0, &[], SENDER_A, &default_protocol_timing())
            .unwrap();
        storage.ensure_open_tip().unwrap();
        let mut head = storage.open_state().unwrap().unwrap();
        let offset = storage.next_l2_tx_count().unwrap().get();
        storage
            .append_included_user_ops(&mut head, &[included_user_op_from(alice, 2, offset)])
            .unwrap();
        assert_eq!(storage.next_user_nonce(alice).unwrap(), 3);
        let raw_max: i64 = storage
            .conn
            .query_row(
                "SELECT MAX(nonce) FROM user_ops WHERE sender = ?1",
                [alice.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            raw_max, 3,
            "an unfiltered MAX would still report the rolled-back op"
        );
    }

    #[test]
    fn next_user_nonce_seeks_the_sender_index_without_sorting() {
        let db = temp_db("next-user-nonce-plan");
        let storage = set_up_storage(&db.path, SENDER_A);
        let plan: Vec<String> = storage
            .conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                super::LAST_VALID_USER_NONCE_SQL
            ))
            .unwrap()
            .query_map([Address::ZERO.as_slice()], |row| row.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let plan = plan.join("\n");
        assert!(
            plan.contains("idx_user_ops_sender_nonce"),
            "nonce lookup must seek the sender index:\n{plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "index order must serve ORDER BY:\n{plan}"
        );
    }

    /// Four directs at block 10, timestamp 120: one at the payload cap, one a
    /// byte over it, then the submitter's own input, then one more direct.
    fn seed_directs_around_the_payload_cap(storage: &mut Storage) {
        let cap = usize::try_from(TEST_MAX_DIRECT_PAYLOAD_BYTES).unwrap();
        let row = |sender: Address, payload: Vec<u8>, hash: u8| IngestedSafeInput {
            sender,
            payload,
            block_number: 10,
            block_timestamp: 120,
            transaction_hash: B256::repeat_byte(hash),
        };
        storage
            .append_ingested_safe_inputs_with_timestamp(
                10,
                120,
                &[
                    row(Address::ZERO, vec![0xaa; cap], 1),
                    row(Address::ZERO, vec![0xbb; cap + 1], 2),
                    row(SENDER_A, vec![0xff], 3),
                    row(Address::repeat_byte(0x44), vec![0xcc], 4),
                ],
                &default_protocol_timing(),
            )
            .expect("persist direct inputs");
    }

    #[test]
    fn an_over_cap_direct_never_executes_and_later_directs_keep_contiguous_offsets() {
        let db = temp_db("direct-payload-cap");
        let mut storage = set_up_storage(&db.path, SENDER_A);
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        seed_directs_around_the_payload_cap(&mut storage);

        // The lane reads exactly the directs that execute, each with the
        // index and timestamp the application receives.
        let range = SafeInputRange::new(0, 4);
        let mut directs = Vec::new();
        storage
            .fill_direct_inputs(range, &mut directs)
            .expect("read executable directs");
        let cap = usize::try_from(TEST_MAX_DIRECT_PAYLOAD_BYTES).unwrap();
        let at_cap = Direct {
            input_index: 0,
            sender: Address::ZERO,
            block: 10,
            timestamp: 120,
            payload: vec![0xaa; cap],
        };
        let after_skip = Direct {
            input_index: 3,
            sender: Address::repeat_byte(0x44),
            block: 10,
            timestamp: 120,
            payload: vec![0xcc],
        };
        assert_eq!(directs, vec![at_cap.clone(), after_skip.clone()]);

        // Offering an execution for the over-cap direct breaks the frame's
        // agreement with storage's derivation and rolls the close back.
        let with_over_cap = [
            DirectInputExecution {
                safe_input_index: 0,
                l2_tx_index: L2TxCount::ZERO,
            },
            DirectInputExecution {
                safe_input_index: 1,
                l2_tx_index: L2TxCount::new(1),
            },
            DirectInputExecution {
                safe_input_index: 3,
                l2_tx_index: L2TxCount::new(2),
            },
        ];
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = storage.close_frame_only_with_executions(&mut head, 10, range, &with_over_cap);
        }));
        assert!(panic.is_err(), "an over-cap execution must fail loud");
        assert_eq!(head.frame_in_batch(), 0);
        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 0);

        let executions = [
            DirectInputExecution {
                safe_input_index: 0,
                l2_tx_index: L2TxCount::ZERO,
            },
            DirectInputExecution {
                safe_input_index: 3,
                l2_tx_index: L2TxCount::new(1),
            },
        ];
        storage
            .close_frame_only_with_executions(&mut head, 10, range, &executions)
            .expect("close the frame over the executable directs");

        assert_eq!(
            storage.next_undrained_safe_input_index().unwrap(),
            4,
            "the whole range drains, the over-cap direct included"
        );
        assert_eq!(storage.next_l2_tx_count().unwrap(), L2TxCount::new(2));
        let rows: Vec<(i64, i64)> = storage
            .conn
            .prepare(
                "SELECT l2_tx_count, safe_input_index FROM application_inputs ORDER BY l2_tx_count",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec![(0, 0), (1, 3)]);
        assert_eq!(
            crate::storage::test_helpers::all_ordered_l2_txs(&mut storage),
            vec![L2Tx::Direct(at_cap), L2Tx::Direct(after_skip)]
        );

        storage
            .close_frame_and_batch(&mut head)
            .expect("the batch closes over the drained frame");
        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 4);
    }

    #[test]
    fn a_derived_drain_skips_the_over_cap_direct_too() {
        let db = temp_db("direct-payload-cap-derived");
        let mut storage = set_up_storage(&db.path, SENDER_A);
        seed_directs_around_the_payload_cap(&mut storage);

        storage.ensure_open_tip().expect("open genesis tip");

        assert_eq!(storage.next_undrained_safe_input_index().unwrap(), 4);
        let replay = crate::storage::test_helpers::all_ordered_l2_txs(&mut storage);
        let indexes: Vec<u64> = replay
            .iter()
            .map(|tx| match tx {
                L2Tx::Direct(direct) => direct.input_index,
                L2Tx::UserOp(op) => panic!("unexpected user op {op:?}"),
            })
            .collect();
        assert_eq!(indexes, vec![0, 3]);
        assert_eq!(storage.next_l2_tx_count().unwrap(), L2TxCount::new(2));
    }
}
