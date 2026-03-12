// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Integration tests for the batch submitter: worker loop with real storage and mock poster.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sequencer::batch_submitter::{BatchSubmitter, BatchSubmitterConfig};
use sequencer::onchain::{BatchPoster, BatchPosterError, TxHash};
use sequencer::shutdown::ShutdownSignal;
use sequencer::storage::{DirectInputRange, Storage};
use sequencer_core::batch::Batch;
use tempfile::TempDir;

/// Minimal mock for integration tests: records submissions.
struct TestMock {
    submissions: std::sync::Mutex<Vec<(u64, usize)>>,
}

impl TestMock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            submissions: std::sync::Mutex::new(Vec::new()),
        })
    }
    fn submissions(&self) -> Vec<(u64, usize)> {
        self.submissions.lock().expect("lock").clone()
    }
}

#[async_trait]
impl BatchPoster for TestMock {
    async fn submit_batch(&self, payload: Vec<u8>) -> Result<TxHash, BatchPosterError> {
        let batch_index = ssz::Decode::from_ssz_bytes(payload.as_slice())
            .map(|b: Batch| b.nonce)
            .unwrap_or(0);
        self.submissions
            .lock()
            .expect("lock")
            .push((batch_index, payload.len()));
        Ok(TxHash::ZERO)
    }

    async fn latest_submitted_batch_index(&self) -> Result<Option<u64>, BatchPosterError> {
        Ok(self
            .submissions
            .lock()
            .expect("lock")
            .iter()
            .map(|(idx, _)| *idx)
            .max())
    }
}

const SQLITE_SYNCHRONOUS_PRAGMA: &str = "NORMAL";

fn temp_db(name: &str) -> (TempDir, String) {
    let dir = tempfile::Builder::new()
        .prefix(format!("sequencer-batch-submitter-it-{name}-").as_str())
        .tempdir()
        .expect("create temporary test directory");
    let path = dir.path().join("sequencer.sqlite");
    (dir, path.to_string_lossy().into_owned())
}

/// Seeds storage so batches 1 and 2 are closed and batch 3 is open.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submitter_loop_submits_closed_batches_then_exits_on_shutdown() {
    let (_dir, path) = temp_db("loop-submits");
    seed_two_closed_batches(&path);

    let mock = TestMock::new();
    let shutdown = ShutdownSignal::default();
    let config = BatchSubmitterConfig {
        idle_poll_interval_ms: 5000,
        max_batches_per_loop: 10,
    };
    let submitter = BatchSubmitter::new(path, mock.clone(), shutdown.clone(), config);
    let handle = submitter.start().expect("start batch submitter");

    // Allow at least one tick to run (worker may submit batch 1 and 2 in one tick).
    tokio::time::sleep(Duration::from_millis(200)).await;

    shutdown.request_shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    let submissions = mock.submissions();
    assert!(
        submissions.len() >= 3,
        "submitter should have submitted at least batch 0, 1, and 2, got {:?}",
        submissions
    );
    assert_eq!(submissions[0].0, 0, "first submission should be batch 0");
    assert_eq!(submissions[1].0, 1, "second submission should be batch 1");
    assert_eq!(submissions[2].0, 2, "third submission should be batch 2");
}
