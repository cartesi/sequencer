// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Write-before-broadcast hook for the wallet-nonce watermark.
//!
//! Every component that broadcasts a transaction from the batch-submitter key
//! — the batch poster and the mempool flusher's no-ops alike — must first
//! durably commit `watermark = max(watermark, highest_nonce_about_to_send)`
//! and only then send. One uniform rule, no case analysis: the invariant is
//! simply "the watermark covers the nonce of everything we ever sent", which
//! is what lets the flush consume every slot we ever used without trusting
//! the local node's volatile mempool memory (the zombie counterexample).
//!
//! A crash between the commit and the send only over-covers: the flush later
//! no-ops a never-used slot — one wasted no-op, harmless.

use thiserror::Error;

use crate::storage::{
    Storage, StorageOpenError, is_persistent_storage_error, is_persistent_storage_open_error,
};

#[derive(Debug, Error)]
pub enum WalletNonceWatermarkError {
    #[error("watermark sink could not open storage")]
    OpenStorage(#[source] StorageOpenError),
    #[error("watermark sink storage write failed")]
    Storage(#[source] rusqlite::Error),
    #[error("watermark sink failed: {0}")]
    Other(String),
}

impl WalletNonceWatermarkError {
    pub(crate) fn is_persistent_invariant(&self) -> bool {
        match self {
            Self::OpenStorage(source) => is_persistent_storage_open_error(source),
            Self::Storage(source) => is_persistent_storage_error(source),
            Self::Other(_) => false,
        }
    }
}

/// Durable raise of the wallet-nonce watermark, called *before* broadcasting.
/// `raise_to(h)` must commit `watermark = max(watermark, h)` power-loss
/// durably before returning; the caller may then broadcast txs at nonces
/// `<= h`.
pub trait WalletNonceWatermarkSink: Send + Sync {
    fn raise_to(&self, highest: u64) -> Result<(), WalletNonceWatermarkError>;
}

/// Sink backed by the sequencer DB's `wallet_nonce_watermark` singleton.
/// Opens a short-lived writer connection per raise — raises happen at most
/// once per submitter tick / flush pass, well off the hot path.
pub struct StorageWatermarkSink {
    db_path: String,
}

impl StorageWatermarkSink {
    pub fn new(db_path: impl Into<String>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }
}

impl WalletNonceWatermarkSink for StorageWatermarkSink {
    fn raise_to(&self, highest: u64) -> Result<(), WalletNonceWatermarkError> {
        let mut storage =
            Storage::open_writer(&self.db_path).map_err(WalletNonceWatermarkError::OpenStorage)?;
        storage
            .raise_wallet_nonce_watermark(highest)
            .map_err(WalletNonceWatermarkError::Storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_classifier_preserves_storage_provenance() {
        assert!(
            WalletNonceWatermarkError::Storage(rusqlite::Error::QueryReturnedNoRows)
                .is_persistent_invariant()
        );
        assert!(
            !WalletNonceWatermarkError::Storage(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::DatabaseBusy,
                    extended_code: 5,
                },
                None,
            ))
            .is_persistent_invariant()
        );
        assert!(
            !WalletNonceWatermarkError::Other("injected operational failure".into())
                .is_persistent_invariant()
        );
    }
}
