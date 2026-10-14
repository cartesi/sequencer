// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Write-before-broadcast hook (I14): the poster and the flusher durably raise the
//! wallet-nonce watermark to the highest nonce they are about to send, then send,
//! so a flush consumes every slot ever used without trusting the node's mempool.
//! A crash between the two only over-covers.

use thiserror::Error;

use crate::storage::{
    Storage, StorageOpenError, is_persistent_storage_error, is_persistent_storage_open_error,
};

#[derive(Debug, Error)]
pub enum WalletNonceWatermarkError {
    #[error("watermark sink could not open storage: {0}")]
    OpenStorage(#[source] StorageOpenError),
    #[error("watermark sink storage write failed: {0}")]
    Storage(#[source] rusqlite::Error),
}

impl WalletNonceWatermarkError {
    pub(crate) fn is_persistent_invariant(&self) -> bool {
        match self {
            Self::OpenStorage(source) => is_persistent_storage_open_error(source),
            Self::Storage(source) => is_persistent_storage_error(source),
        }
    }
}

/// Durable raise of the wallet-nonce watermark, called *before* broadcasting.
/// `raise_to(h)` must commit `watermark = max(watermark, h)` power-loss
/// durably before returning; the caller may then broadcast txs at wallet
/// nonces `<= h`.
pub trait WalletNonceWatermarkSink: Send + Sync {
    fn raise_to(&self, highest_wallet_nonce: u64) -> Result<(), WalletNonceWatermarkError>;
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
    fn raise_to(&self, highest_wallet_nonce: u64) -> Result<(), WalletNonceWatermarkError> {
        let mut storage =
            Storage::open_writer(&self.db_path).map_err(WalletNonceWatermarkError::OpenStorage)?;
        storage
            .raise_wallet_nonce_watermark(highest_wallet_nonce)
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
    }
}
