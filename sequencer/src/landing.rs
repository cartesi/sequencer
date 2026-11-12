// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! The landing watch: every `poll_interval` it reads
//! [`Storage::terminal_fact`] and [`Storage::landing_lag`]. A terminal
//! fact (a divergence or a void history) ends the worker with
//! [`LandingWatchExit::Terminal`], which stops the process terminally;
//! otherwise it publishes the reading for `/healthz` and logs it. It is its own
//! worker so slow L1 submission never delays the stop (I15).

use std::time::Duration;

use thiserror::Error;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::runtime::process_lock::{ProcessLock, spawn_blocking_with_lock};
use crate::runtime::shutdown::ShutdownSignal;
use crate::storage::{LandingLag, Storage, StorageOpenError, TerminalFact};

/// How often the reading is logged at `info`, after the first tick; every
/// tick logs at `debug`.
const INFO_LOG_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// What the landing watch publishes and logs each tick: the landing lag beside
/// the window, and the safe block's L1 timestamp for the safe head's age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LandingReading {
    pub lag: Option<LandingLag>,
    /// The window `setup` pinned with the deployment identity.
    pub max_wait_blocks: u64,
    /// L1 timestamp (Unix seconds) of the persisted safe block; `None` before
    /// the first sync.
    pub safe_block_timestamp: Option<u64>,
}

impl LandingReading {
    /// `landing_lag_blocks`: `H + 1 - s` ([`LandingLag::blocks`]), void at
    /// `max_wait_blocks`; `None` when no batch is unaccepted or there is no
    /// safe head yet.
    pub fn landing_lag_blocks(&self) -> Option<u64> {
        self.lag.map(|lag| lag.blocks())
    }

    /// `safe_head_age_seconds`: the wall clock minus the safe block's L1
    /// timestamp, saturating at 0. It keeps growing while the provider is
    /// down, when the landing lag freezes.
    pub fn safe_head_age_seconds(&self, now_ms: u64) -> Option<u64> {
        self.safe_block_timestamp
            .map(|timestamp| (now_ms / 1000).saturating_sub(timestamp))
    }
}

/// The latest landing reading, for `/healthz`.
pub(crate) type LandingFeed = watch::Receiver<LandingReading>;

/// How the watch's loop exited.
#[derive(Debug)]
pub enum LandingWatchExit {
    /// Shutdown signal fired before any terminal fact held.
    Shutdown,
    /// A fact that ends the era holds. The runtime stops terminally; every
    /// later `run` refuses on the same fact.
    Terminal(TerminalFact),
}

#[derive(Debug, Error)]
pub enum LandingWatchError {
    #[error(transparent)]
    OpenStorage(#[from] StorageOpenError),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
    #[error("storage task panicked while reading the landing lag: persistent invariant failure")]
    StorageTaskPanicked,
    #[error("landing watch join error: {0}")]
    Join(String),
}

impl LandingWatchError {
    /// Whether this error poisons the run rather than restarting. Named
    /// arms, no wildcard: a new variant must classify itself here.
    pub(crate) fn is_terminal_invariant(&self) -> bool {
        match self {
            Self::Storage(source) => crate::storage::is_persistent_storage_error(source),
            Self::OpenStorage(source) => crate::storage::is_persistent_storage_open_error(source),
            Self::StorageTaskPanicked => true,
            // A non-panic inner-task join is shutdown-path cancellation.
            Self::Join(_) => false,
        }
    }
}

/// One tick's result: a terminal fact, or the reading to publish.
enum Tick {
    Terminal(TerminalFact),
    Reading(LandingReading),
}

pub struct LandingWatch {
    db_path: String,
    max_wait_blocks: u64,
    poll_interval: Duration,
    publish: watch::Sender<LandingReading>,
    /// Retains data-directory exclusivity in detached blocking reads.
    /// Required at construction.
    process_lock: ProcessLock,
}

impl LandingWatch {
    /// The watch and the feed `/healthz` reads. Until the first tick the feed
    /// carries the pinned window and no lag.
    pub(crate) fn new(
        db_path: impl Into<String>,
        max_wait_blocks: u64,
        poll_interval: Duration,
        process_lock: ProcessLock,
    ) -> (Self, LandingFeed) {
        let (publish, feed) = watch::channel(LandingReading {
            lag: None,
            max_wait_blocks,
            safe_block_timestamp: None,
        });
        (
            Self {
                db_path: db_path.into(),
                max_wait_blocks,
                poll_interval,
                publish,
                process_lock,
            },
            feed,
        )
    }

    #[cfg(test)]
    pub(crate) fn start(
        self,
        shutdown: ShutdownSignal,
    ) -> Result<
        tokio::task::JoinHandle<Result<LandingWatchExit, LandingWatchError>>,
        StorageOpenError,
    > {
        self.preflight_storage()?;
        Ok(self.start_preflighted(shutdown))
    }

    /// Validate the storage dependency without starting a task.
    pub(crate) fn preflight_storage(&self) -> Result<(), StorageOpenError> {
        let _ = Storage::open_read_only(self.db_path.as_str())?;
        Ok(())
    }

    /// Spawn after [`Self::preflight_storage`] succeeded. Infallible so the
    /// runtime can launch all workers in one non-yielding ownership step.
    pub(crate) fn start_preflighted(
        self,
        shutdown: ShutdownSignal,
    ) -> tokio::task::JoinHandle<Result<LandingWatchExit, LandingWatchError>> {
        tokio::spawn(async move { self.run_forever(shutdown).await })
    }

    /// The biased shutdown arm returns promptly; an in-flight blocking read
    /// retains its own process-lock clone until it stops.
    async fn run_forever(
        self,
        shutdown: ShutdownSignal,
    ) -> Result<LandingWatchExit, LandingWatchError> {
        tokio::select! {
            biased;
            _ = shutdown.wait_for_shutdown() => Ok(LandingWatchExit::Shutdown),
            result = self.run_loop() => result,
        }
    }

    async fn run_loop(self) -> Result<LandingWatchExit, LandingWatchError> {
        let mut last_info: Option<Instant> = None;
        loop {
            match self.check_once().await? {
                Tick::Terminal(fact) => {
                    tracing::error!(%fact, "terminal fact: stopping the sequencer for good");
                    return Ok(LandingWatchExit::Terminal(fact));
                }
                Tick::Reading(reading) => {
                    let now = Instant::now();
                    let info =
                        last_info.is_none_or(|at| now.duration_since(at) >= INFO_LOG_INTERVAL);
                    if info {
                        last_info = Some(now);
                    }
                    log_reading(&reading, info);
                    self.publish.send_replace(reading);
                }
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn check_once(&self) -> Result<Tick, LandingWatchError> {
        let db_path = self.db_path.clone();
        let max_wait_blocks = self.max_wait_blocks;
        let process_lock = self.process_lock.clone();
        spawn_blocking_with_lock(process_lock, move || {
            let mut storage = Storage::open_read_only(&db_path)?;
            if let Some(fact) = storage.terminal_fact()? {
                return Ok(Tick::Terminal(fact));
            }
            Ok(Tick::Reading(LandingReading {
                lag: storage.landing_lag()?,
                max_wait_blocks,
                safe_block_timestamp: storage.current_safe_block_timestamp()?,
            }))
        })
        .await
        .map_err(map_storage_task_join)?
    }
}

fn log_reading(reading: &LandingReading, info: bool) {
    let batch_nonce = reading.lag.map(|lag| lag.batch_nonce);
    let landing_lag_blocks = reading.landing_lag_blocks();
    let max_wait_blocks = reading.max_wait_blocks;
    let safe_head_age_seconds = reading.safe_head_age_seconds(crate::clock::unix_now_ms());
    if info {
        tracing::info!(
            batch_nonce,
            landing_lag_blocks,
            max_wait_blocks,
            safe_head_age_seconds,
            "landing lag"
        );
    } else {
        tracing::debug!(
            batch_nonce,
            landing_lag_blocks,
            max_wait_blocks,
            safe_head_age_seconds,
            "landing lag"
        );
    }
}

/// A feed with no watch behind it: the window and no reading, for servers
/// started without a runtime.
#[cfg(test)]
pub(crate) fn test_feed(max_wait_blocks: u64) -> LandingFeed {
    watch::channel(LandingReading {
        lag: None,
        max_wait_blocks,
        safe_block_timestamp: None,
    })
    .1
}

fn map_storage_task_join(err: tokio::task::JoinError) -> LandingWatchError {
    if err.is_panic() {
        LandingWatchError::StorageTaskPanicked
    } else {
        LandingWatchError::Join(err.to_string())
    }
}

#[cfg(test)]
mod tests;
