// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch landing: the oldest batch L1 has not accepted, the facts that end the
//! era (a divergence or a void history), and the one batch `run` opens at
//! start. Every read derives from the database as it is; nothing here is a
//! stored flag.

use rusqlite::{Connection, OptionalExtension, Result, TransactionBehavior, params};
use sequencer_core::protocol::age_exceeds;

use super::Storage;
use super::acceptance::{Divergence, canonical_divergence_in, frontier_nonce};
use super::convert::{i64_to_u64, u64_to_i64};
use super::ingress::open_fresh_tip_in_tx;
use super::l1_inputs::query_deployment_identity;
use super::queries::current_safe_block;

/// The oldest batch L1 has not accepted, closed or open, at the safe head
/// `H`, with its first frame's safe block `s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LandingLag {
    pub batch_nonce: u64,
    pub first_frame_safe_block: u64,
    pub safe_block: u64,
}

impl LandingLag {
    /// `H + 1 - s`, in L1 blocks: the batch's age at `H + 1`, the earliest
    /// block it can still land in. The history is void exactly when this
    /// reaches `max_wait_blocks`, so the two compare directly.
    pub fn blocks(&self) -> u64 {
        (self.safe_block + 1).saturating_sub(self.first_frame_safe_block)
    }

    /// No block after the safe head can accept this batch. Any landing still
    /// to come is in block `H + 1` or later, and the scheduler skips a batch
    /// that lands `max_wait_blocks` or more after its first frame's safe
    /// block, keeping its nonce, so no later batch can be accepted either.
    pub fn is_void(&self, max_wait_blocks: u64) -> bool {
        age_exceeds(
            self.safe_block + 1,
            self.first_frame_safe_block,
            max_wait_blocks,
        )
    }
}

/// A fact that ends the era: `run` refuses while it holds, and it never stops
/// holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TerminalFact {
    /// A foreign or different batch was accepted at one of our nonces.
    #[error(
        "canonical divergence at batch nonce {}; only cockroach recovery (fresh-directory rebuild) can proceed",
        .0.nonce
    )]
    Divergence(Divergence),
    /// The oldest unaccepted batch can no longer be accepted.
    #[error(
        "the history is void: batch {}, opened at safe block {}, was not accepted by safe block {}, \
         and no later block can accept it (max_wait_blocks {max_wait_blocks}); recover with \
         setup --base in a new data directory",
        lag.batch_nonce,
        lag.first_frame_safe_block,
        lag.safe_block
    )]
    VoidHistory {
        lag: LandingLag,
        max_wait_blocks: u64,
    },
}

/// Opening the start batch was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenBatchError {
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error(transparent)]
    Terminal(TerminalFact),
    /// The open transaction left no open batch: a broken postcondition,
    /// rolled back.
    #[error("opening a batch left no open batch in its own transaction")]
    OpenBatchMissing,
}

impl Storage {
    /// The canonical-divergence marker (I15), if present.
    #[cfg(test)]
    pub fn canonical_divergence(&mut self) -> Result<Option<Divergence>> {
        self.read(|tx| canonical_divergence_in(tx))
    }

    /// The oldest batch L1 has not accepted, at the persisted safe head.
    /// `None` when there is no safe head yet or every batch is accepted.
    pub fn landing_lag(&mut self) -> Result<Option<LandingLag>> {
        self.read(|tx| landing_lag_in(tx))
    }

    /// The fact that ends the era, if one holds: a recorded divergence first,
    /// then a void history under the pinned window. Reads only the database,
    /// so it answers with L1 unreachable.
    pub fn terminal_fact(&mut self) -> Result<Option<TerminalFact>> {
        self.read(|tx| terminal_fact_in(tx))
    }

    /// Open a batch at the safe head if none is open, in one transaction that
    /// refuses while a terminal fact holds. Returns whether it opened one.
    /// The new batch drains the undrained directs. The first `run` of an era
    /// opens its first batch here; setup opens none.
    pub(crate) fn open_batch_if_none(&mut self) -> std::result::Result<bool, OpenBatchError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(fact) = terminal_fact_in(&tx)? {
            return Err(OpenBatchError::Terminal(fact));
        }
        if open_batch(&tx)?.is_some() {
            return Ok(false);
        }
        open_fresh_tip_in_tx(&tx)?;
        // A broken postcondition must roll back this transaction and refuse
        // startup, including in release builds.
        if open_batch(&tx)?.is_none() {
            return Err(OpenBatchError::OpenBatchMissing);
        }
        tx.commit()?;
        Ok(true)
    }
}

/// The staleness window `setup` pinned with the deployment identity. Storage
/// reads it from the pin, as it reads the submitter.
fn pinned_max_wait_blocks_in(conn: &Connection) -> Result<u64> {
    Ok(query_deployment_identity(conn)?
        .ok_or(rusqlite::Error::QueryReturnedNoRows)?
        .max_wait_blocks)
}

pub(super) fn terminal_fact_in(conn: &Connection) -> Result<Option<TerminalFact>> {
    if let Some(divergence) = canonical_divergence_in(conn)? {
        return Ok(Some(TerminalFact::Divergence(divergence)));
    }
    let Some(lag) = landing_lag_in(conn)? else {
        return Ok(None);
    };
    let max_wait_blocks = pinned_max_wait_blocks_in(conn)?;
    Ok(lag
        .is_void(max_wait_blocks)
        .then_some(TerminalFact::VoidHistory {
            lag,
            max_wait_blocks,
        }))
}

/// The batch at the accepted frontier's next nonce: the first unaccepted
/// closed batch, else the open batch. First-frame safe blocks do not
/// decrease along the batches (I3), so it is the oldest unaccepted one.
fn landing_lag_in(conn: &Connection) -> Result<Option<LandingLag>> {
    let Some(safe_block) = current_safe_block(conn)? else {
        return Ok(None);
    };
    let batch_nonce = match first_unaccepted_closed_batch(conn)? {
        Some(nonce) => nonce,
        None => match open_batch(conn)? {
            Some(nonce) => nonce,
            None => return Ok(None),
        },
    };
    Ok(Some(LandingLag {
        batch_nonce,
        first_frame_safe_block: first_frame_safe_block_of(conn, batch_nonce)?,
        safe_block,
    }))
}

/// First closed batch with `nonce >= frontier_nonce`, the next nonce
/// the scheduler is expected to accept. `>=`, not `>`: the batch at the
/// frontier carries `nonce == frontier_nonce`. The nonce seek, not
/// `accepted_input IS NULL`, keeps this off the accepted prefix: the landing
/// watch calls it every tick. Returns `None` if every closed batch is
/// accepted.
fn first_unaccepted_closed_batch(conn: &Connection) -> Result<Option<u64>> {
    let frontier = frontier_nonce(conn)?;
    let nonce: Option<i64> = conn
        .query_row(
            "SELECT nonce FROM closed_batches \
             WHERE nonce >= ?1 ORDER BY nonce ASC LIMIT 1",
            params![u64_to_i64(frontier)],
            |row| row.get(0),
        )
        .optional()?;
    Ok(nonce.map(i64_to_u64))
}

fn open_batch(conn: &Connection) -> Result<Option<u64>> {
    let nonce: Option<i64> = conn
        .query_row("SELECT nonce FROM open_batch", [], |row| row.get(0))
        .optional()?;
    Ok(nonce.map(i64_to_u64))
}

/// `frames.safe_block` of the lowest `frame_in_batch` in `batch_nonce`.
/// Every committed batch has a first frame; missing one is an invariant
/// violation and propagates as `QueryReturnedNoRows`.
fn first_frame_safe_block_of(conn: &Connection, batch_nonce: u64) -> Result<u64> {
    conn.query_row(
        "SELECT safe_block FROM frames \
         WHERE batch_nonce = ?1 ORDER BY frame_in_batch ASC LIMIT 1",
        params![u64_to_i64(batch_nonce)],
        |row| row.get::<_, i64>(0).map(i64_to_u64),
    )
}

#[cfg(test)]
mod tests;
