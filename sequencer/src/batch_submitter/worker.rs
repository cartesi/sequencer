// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter worker: at-least-once submission to L1, deduplicated by the scheduler.
//!
//! The worker is intentionally stateless with respect to submitted-batch progress.
//! On each tick it derives the highest submitted batch nonce from L1, compares that
//! with locally closed batches, submits the first missing batch if any, then loops.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::warn;

use crate::batch_submitter::{BatchPoster, BatchPosterError, BatchSubmitterConfig, TxHash};
use crate::shutdown::ShutdownSignal;
use crate::storage::{Storage, StorageOpenError};

#[derive(Debug, Error)]
pub enum BatchSubmitterError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("batch submitter join error: {0}")]
    Join(String),
    #[error(transparent)]
    Poster(#[from] BatchPosterError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    Idle,
    Submitted { batch_index: u64, tx_hash: TxHash },
}

pub struct BatchSubmitter<P: BatchPoster> {
    db_path: String,
    poster: Arc<P>,
    idle_poll_interval: Duration,
    shutdown: ShutdownSignal,
}

impl<P: BatchPoster + 'static> BatchSubmitter<P> {
    pub fn new(
        db_path: impl Into<String>,
        poster: Arc<P>,
        shutdown: ShutdownSignal,
        config: BatchSubmitterConfig,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            poster,
            idle_poll_interval: config.idle_poll_interval(),
            shutdown,
        }
    }

    pub fn start(
        self,
    ) -> Result<tokio::task::JoinHandle<Result<(), BatchSubmitterError>>, StorageOpenError> {
        let _ = Storage::open_read_only(self.db_path.as_str())?;
        Ok(tokio::spawn(async move { self.run_forever().await }))
    }

    async fn run_forever(self) -> Result<(), BatchSubmitterError> {
        loop {
            if self.shutdown.is_shutdown_requested() {
                return Ok(());
            }

            match self.tick_once().await {
                Ok(TickOutcome::Submitted { .. }) => continue,
                Ok(TickOutcome::Idle) => {}
                Err(BatchSubmitterError::Poster(source)) => {
                    warn!(error = %source, "batch submitter tick failed, will retry");
                }
                Err(err) => return Err(err),
            }

            tokio::select! {
                _ = self.shutdown.wait_for_shutdown() => return Ok(()),
                _ = tokio::time::sleep(self.idle_poll_interval) => {}
            }
        }
    }

    pub(crate) async fn tick_once(&self) -> Result<TickOutcome, BatchSubmitterError> {
        let latest_batch_opt = self.load_latest_batch_index().await?;
        let Some(latest_batch_index) = latest_batch_opt else {
            return Ok(TickOutcome::Idle);
        };

        if latest_batch_index == 0 {
            return Ok(TickOutcome::Idle);
        }

        let last_closed = latest_batch_index - 1;
        let latest_submitted = self.poster.latest_submitted_batch_index().await?;
        let first_to_submit = latest_submitted.map(|s| s + 1).unwrap_or(0);
        if first_to_submit > last_closed {
            return Ok(TickOutcome::Idle);
        }
        if first_to_submit < last_closed {
            let pending_batches = last_closed - first_to_submit + 1;
            warn!(
                first_to_submit,
                last_closed, pending_batches, "multiple closed batches are pending submission"
            );
        }

        let batch = self.load_batch_for_submission(first_to_submit).await?;
        let tx_hash = self
            .poster
            .submit_batch(batch.encode_for_scheduler())
            .await?;

        Ok(TickOutcome::Submitted {
            batch_index: first_to_submit,
            tx_hash,
        })
    }

    async fn load_latest_batch_index(&self) -> Result<Option<u64>, BatchSubmitterError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            storage
                .latest_batch_index()
                .map_err(BatchSubmitterError::from)
        })
        .await
        .map_err(|err| BatchSubmitterError::Join(err.to_string()))?
    }

    async fn load_batch_for_submission(
        &self,
        batch_index: u64,
    ) -> Result<sequencer_core::batch::BatchForSubmission, BatchSubmitterError> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            storage
                .load_batch_for_submission(batch_index)
                .map_err(BatchSubmitterError::from)
        })
        .await
        .map_err(|err| BatchSubmitterError::Join(err.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::batch_submitter::{
        BatchSubmitterConfig, BatchSubmitterError, TickOutcome, batch_poster::mock::MockBatchPoster,
    };
    use crate::shutdown::ShutdownSignal;
    use crate::storage::{SafeInputRange, Storage};
    use tempfile::TempDir;

    const SQLITE_SYNCHRONOUS_PRAGMA: &str = "NORMAL";

    fn temp_db(name: &str) -> (TempDir, String) {
        let dir = tempfile::Builder::new()
            .prefix(format!("sequencer-batch-submitter-{name}-").as_str())
            .tempdir()
            .expect("create temporary test directory");
        let path = dir.path().join("sequencer.sqlite");
        (dir, path.to_string_lossy().into_owned())
    }

    fn seed_two_closed_batches(db_path: &str) {
        let mut storage = Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, SafeInputRange::empty_at(0))
            .expect("initialize open state");
        let next_safe = head.safe_block;
        storage
            .close_frame_and_batch(&mut head, next_safe)
            .expect("close batch 0");
        storage
            .close_frame_and_batch(&mut head, next_safe)
            .expect("close batch 1");
        storage
            .close_frame_and_batch(&mut head, next_safe)
            .expect("close batch 2");
    }

    #[tokio::test]
    async fn tick_once_submits_first_missing_closed_batch() {
        let (_dir, path) = temp_db("tick-submits");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        let config = BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
        };
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            mock.clone(),
            ShutdownSignal::default(),
            config,
        );

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(
            outcome,
            TickOutcome::Submitted {
                batch_index: 0,
                tx_hash: alloy_primitives::B256::ZERO
            }
        );

        let submissions = mock.submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].0, 0);
    }

    #[tokio::test]
    async fn tick_once_submits_nothing_when_already_caught_up() {
        let (_dir, path) = temp_db("tick-caught-up");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_latest_submitted(Some(2));
        let config = BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
        };
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            mock.clone(),
            ShutdownSignal::default(),
            config,
        );

        let outcome = submitter.tick_once().await.expect("tick once");
        assert_eq!(outcome, TickOutcome::Idle);
        assert!(mock.submissions().is_empty());
    }

    #[tokio::test]
    async fn tick_once_propagates_poster_errors() {
        let (_dir, path) = temp_db("tick-poster-error");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        mock.set_latest_submitted_error(Some("rpc fail"));
        let submitter = super::BatchSubmitter::new(
            path,
            mock,
            ShutdownSignal::default(),
            BatchSubmitterConfig {
                idle_poll_interval_ms: 1000,
            },
        );

        let err = submitter
            .tick_once()
            .await
            .expect_err("poster error should propagate");
        assert!(matches!(err, BatchSubmitterError::Poster(_)));
    }
}
