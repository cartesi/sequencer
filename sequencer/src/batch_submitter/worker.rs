// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Batch submitter worker: at-least-once submission to L1, deduplicated by the scheduler.
//!
//! We use **at-least-once** semantics (same batch may be submitted more than once). The scheduler
//! deduplicates by **batch nonce** (batch index): it checks whether the nonce is the next expected
//! one and invalidates otherwise, so duplicate submissions of the same batch are harmless.
//!
//! Loop each tick:
//! 1. **Wake up** (start of tick).
//! 2. **Read S from the chain** via the poster (Latest minus scan_depth); S is not persisted.
//! 3. **Compare with local closed batches**: see if the DB has closed batches not yet submitted.
//! 4. **If there are unsubmitted batches**, submit them (each with its batch index as nonce).
//! 5. **Sleep** for `idle_poll_interval` when no work was done, then loop.

use std::sync::Arc;
use std::time::Duration;

use crate::batch_submitter::BatchSubmitterConfig;
use crate::onchain::BatchPoster;
use crate::shutdown::ShutdownSignal;
use crate::storage::Storage;
use crate::storage::StorageOpenError;

pub struct BatchSubmitter<P: BatchPoster> {
    db_path: String,
    poster: Arc<P>,
    idle_poll_interval: Duration,
    max_batches_per_loop: usize,
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
            max_batches_per_loop: config.max_batches_per_loop,
            shutdown,
        }
    }

    pub fn start(self) -> Result<tokio::task::JoinHandle<()>, StorageOpenError> {
        // Fail fast if storage cannot be opened (read-only is enough for the submitter).
        let _ = Storage::open_read_only(self.db_path.as_str())?;
        Ok(tokio::spawn(async move {
            self.run_forever().await;
        }))
    }

    async fn run_forever(self) {
        loop {
            if self.shutdown.is_shutdown_requested() {
                return;
            }

            // One tick: wake → read S → compare → submit (if any) → then sleep if idle.
            let work_done = self.tick_once().await;

            if !work_done {
                tokio::select! {
                    _ = self.shutdown.wait_for_shutdown() => return,
                    _ = tokio::time::sleep(self.idle_poll_interval) => {}
                }
            }
        }
    }

    /// One iteration: derive S from the chain via the poster, compare with local closed batches,
    /// submit any unsubmitted ones.
    pub(crate) async fn tick_once(&self) -> bool {
        // 1. Read latest local batch index from storage (open batch); S is derived from chain each tick.
        let db_path = self.db_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            let latest = storage
                .latest_batch_index()
                .map_err(StorageOpenError::from)?;
            Ok::<_, StorageOpenError>(latest)
        })
        .await;

        let Ok(Ok(latest_batch_opt)) = result else {
            return false;
        };

        let latest_submitted = self
            .poster
            .latest_submitted_batch_index()
            .await
            .ok()
            .flatten();

        let Some(latest_batch_index) = latest_batch_opt else {
            return false;
        };

        // When only batch 0 exists it is still open; no closed batch to submit.
        if latest_batch_index == 0 {
            return false;
        }

        // Last closed batch index (open batch is latest_batch_index).
        let last_closed = latest_batch_index.saturating_sub(1);
        // No batch submitted yet => None; first to submit is 0. Otherwise first_to_submit = S + 1.
        let first_to_submit = latest_submitted.map(|s| s + 1).unwrap_or(0);
        if first_to_submit > last_closed {
            return false;
        }
        let mut any_submitted = false;

        for batch_index in first_to_submit..=last_closed {
            if (batch_index - first_to_submit) as usize >= self.max_batches_per_loop {
                break;
            }

            let db_path = self.db_path.clone();
            let load_result = tokio::task::spawn_blocking(move || {
                let mut storage = Storage::open_read_only(&db_path)?;
                storage
                    .load_batch_for_submission(batch_index)
                    .map_err(StorageOpenError::from)
            })
            .await;

            let Ok(Ok(batch)) = load_result else {
                // Storage error; stop this tick.
                break;
            };

            let payload = batch.encode_for_scheduler();
            if self.poster.submit_batch(payload).await.is_err() {
                break;
            }
            any_submitted = true;
        }

        any_submitted
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::batch_submitter::BatchSubmitterConfig;
    use crate::onchain::batch_poster::mock::MockBatchPoster;
    use crate::shutdown::ShutdownSignal;
    use crate::storage::{DirectInputRange, Storage};
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

    /// Seed storage so batches 1 and 2 are closed and batch 3 is open.
    fn seed_two_closed_batches(db_path: &str) {
        let mut storage = Storage::open(db_path, SQLITE_SYNCHRONOUS_PRAGMA).expect("open storage");
        let mut head = storage
            .initialize_open_state(0, DirectInputRange::empty_at(0))
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
    async fn tick_once_submits_closed_batches_not_open() {
        let (_dir, path) = temp_db("tick-submits");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        let config = BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
            max_batches_per_loop: 10,
        };
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            mock.clone(),
            ShutdownSignal::default(),
            config,
        );

        let done = submitter.tick_once().await;
        assert!(done, "one or more batches should have been submitted");

        let submissions = mock.submissions();
        assert_eq!(
            submissions.len(),
            3,
            "should submit batch 0, 1, and 2 (all closed)"
        );
        assert_eq!(submissions[0].0, 0);
        assert_eq!(submissions[1].0, 1);
        assert_eq!(submissions[2].0, 2);
    }

    #[tokio::test]
    async fn tick_once_respects_max_batches_per_loop() {
        let (_dir, path) = temp_db("tick-max-per-loop");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        let config = BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
            max_batches_per_loop: 1,
        };
        let submitter =
            super::BatchSubmitter::new(path, mock.clone(), ShutdownSignal::default(), config);

        let done = submitter.tick_once().await;
        assert!(done);
        assert_eq!(mock.submissions().len(), 1, "only one batch per loop");
        assert_eq!(mock.submissions()[0].0, 0);
    }

    #[tokio::test]
    async fn tick_once_submits_nothing_when_already_caught_up() {
        let (_dir, path) = temp_db("tick-caught-up");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        let config = BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
            max_batches_per_loop: 10,
        };
        let submitter = super::BatchSubmitter::new(
            path.clone(),
            mock.clone(),
            ShutdownSignal::default(),
            config,
        );

        let first_done = submitter.tick_once().await;
        assert!(first_done);
        assert_eq!(mock.submissions().len(), 3, "batches 0, 1, 2");

        // Second tick: poster reports S=2 (from its submissions), so first_to_submit=3 > last_closed=2.
        let second_done = submitter.tick_once().await;
        assert!(!second_done);
    }

    #[tokio::test]
    async fn tick_once_returns_false_when_poster_fails_submit() {
        let (_dir, path) = temp_db("tick-fail-latest");
        seed_two_closed_batches(&path);

        let mock = Arc::new(MockBatchPoster::new());
        *mock.fail_submit.lock().expect("lock") = true;
        let config = BatchSubmitterConfig {
            idle_poll_interval_ms: 1000,
            max_batches_per_loop: 10,
        };
        let submitter = super::BatchSubmitter::new(path, mock, ShutdownSignal::default(), config);

        let done = submitter.tick_once().await;
        assert!(!done);
    }
}
