// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! `run`'s startup, under the process lock with no worker running: check the
//! terminal facts on the database as it is, sync L1, and open a batch if none
//! is open. Nothing else is repaired, flushed or replaced, and
//! nothing can change between these steps and the launch.

use crate::commands::error::{BootstrapError, CommandError};
use crate::l1::reader::{InputReader, InputReaderError};
use crate::storage::Storage;

/// The one L1 operation startup performs. Tests substitute it and keep the
/// real storage reads and the real open transaction.
pub(crate) trait StartupSync {
    async fn sync(&mut self) -> Result<(), InputReaderError>;
}

impl StartupSync for InputReader {
    async fn sync(&mut self) -> Result<(), InputReaderError> {
        self.sync_to_current_safe_head().await
    }
}

/// Refuse while a fact that ends the era holds: a recorded divergence or a
/// void history. Reads only the database, so it answers with L1 unreachable.
pub(super) fn refuse_terminal_fact(db_path: &str) -> Result<(), CommandError> {
    let mut storage = Storage::open_read_only(db_path)?;
    match storage.terminal_fact()? {
        Some(fact) => Err(fact.into()),
        None => Ok(()),
    }
}

/// The whole startup: refuse on a terminal fact before any L1 call, sync L1,
/// then open a batch if none is open. The opening transaction checks the
/// terminal facts again, so one the sync revealed refuses there.
///
/// Only a provider failure of the sync falls back to the stored view, which
/// setup always leaves, and only when a batch is already open: a stale view
/// does not refuse a restart. With no batch open (only the first `run` of an
/// era) the provider failure refuses as retryable instead: a batch opened now
/// would take its first frame's safe block from a stored view that may be
/// hours old, that block is fixed for good, and the batch could then be void
/// before it can land.
pub(crate) async fn start(db_path: &str, l1: &mut impl StartupSync) -> Result<(), CommandError> {
    refuse_terminal_fact(db_path)?;
    match l1.sync().await {
        Ok(()) => tracing::info!("L1 safe head synced"),
        Err(InputReaderError::Provider(error)) => {
            if Storage::open_read_only(db_path)?.open_state()?.is_none() {
                return Err(BootstrapError::StartupSync(InputReaderError::Provider(error)).into());
            }
            tracing::warn!(
                error = %error,
                "L1 unreachable during the startup sync; starting on the stored L1 view"
            );
        }
        Err(error) => return Err(BootstrapError::StartupSync(error).into()),
    }
    let mut storage = Storage::open_writer(db_path)?;
    if storage.open_batch_if_none()? {
        tracing::info!("opened a batch at the safe head");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
