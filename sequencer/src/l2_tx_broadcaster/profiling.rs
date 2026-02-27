// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::{Duration, Instant};
use tracing::info;

#[derive(Debug, Clone, Copy)]
pub(super) struct FanoutOutcome {
    pub(super) delivered: u64,
    pub(super) dropped_closed: u64,
    pub(super) dropped_full: u64,
    pub(super) subscriber_count_before: u64,
    pub(super) subscriber_count_after: u64,
}

#[derive(Debug)]
pub(super) struct BroadcasterMetrics {
    enabled: bool,
    log_interval: Duration,
    page_size: usize,
    subscriber_buffer_capacity: usize,
    idle_poll_interval: Duration,
    window_started_at: Instant,
    loops: u64,
    empty_polls: u64,
    read_errors: u64,
    loaded_txs: u64,
    fanout_delivered: u64,
    dropped_closed: u64,
    dropped_full: u64,
    max_subscribers_before: u64,
    max_subscribers_after: u64,
    read_phase: Duration,
    fanout_phase: Duration,
    idle_sleep: Duration,
}

impl BroadcasterMetrics {
    pub(super) fn new(
        enabled: bool,
        log_interval: Duration,
        page_size: usize,
        subscriber_buffer_capacity: usize,
        idle_poll_interval: Duration,
    ) -> Self {
        Self {
            enabled,
            log_interval,
            page_size,
            subscriber_buffer_capacity,
            idle_poll_interval,
            window_started_at: Instant::now(),
            loops: 0,
            empty_polls: 0,
            read_errors: 0,
            loaded_txs: 0,
            fanout_delivered: 0,
            dropped_closed: 0,
            dropped_full: 0,
            max_subscribers_before: 0,
            max_subscribers_after: 0,
            read_phase: Duration::ZERO,
            fanout_phase: Duration::ZERO,
            idle_sleep: Duration::ZERO,
        }
    }

    pub(super) fn phase_started_at(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    pub(super) fn on_loop_start(&mut self) {
        if !self.enabled {
            return;
        }
        self.loops = self.loops.saturating_add(1);
    }

    pub(super) fn on_empty_poll(&mut self) {
        if !self.enabled {
            return;
        }
        self.empty_polls = self.empty_polls.saturating_add(1);
    }

    pub(super) fn on_read_error(&mut self, started_at: Option<Instant>) {
        if !self.enabled {
            return;
        }
        self.read_errors = self.read_errors.saturating_add(1);
        self.read_phase = self.read_phase.saturating_add(elapsed_or_zero(started_at));
    }

    pub(super) fn on_read_end(&mut self, started_at: Option<Instant>, loaded_txs: u64) {
        if !self.enabled {
            return;
        }
        self.loaded_txs = self.loaded_txs.saturating_add(loaded_txs);
        self.read_phase = self.read_phase.saturating_add(elapsed_or_zero(started_at));
    }

    pub(super) fn on_fanout_end(&mut self, started_at: Option<Instant>, outcome: FanoutOutcome) {
        if !self.enabled {
            return;
        }
        self.fanout_delivered = self.fanout_delivered.saturating_add(outcome.delivered);
        self.dropped_closed = self.dropped_closed.saturating_add(outcome.dropped_closed);
        self.dropped_full = self.dropped_full.saturating_add(outcome.dropped_full);
        self.max_subscribers_before = self
            .max_subscribers_before
            .max(outcome.subscriber_count_before);
        self.max_subscribers_after = self
            .max_subscribers_after
            .max(outcome.subscriber_count_after);
        self.fanout_phase = self
            .fanout_phase
            .saturating_add(elapsed_or_zero(started_at));
    }

    pub(super) fn on_idle_sleep_end(&mut self, started_at: Option<Instant>) {
        if !self.enabled {
            return;
        }
        self.idle_sleep = self.idle_sleep.saturating_add(elapsed_or_zero(started_at));
    }

    pub(super) fn maybe_log_window(&mut self) {
        if !self.enabled {
            return;
        }
        let elapsed = self.window_started_at.elapsed();
        if elapsed < self.log_interval {
            return;
        }
        self.log_window(elapsed, false);
        self.reset_window();
    }

    pub(super) fn log_final(&mut self) {
        if !self.enabled {
            return;
        }
        let elapsed = self.window_started_at.elapsed();
        if elapsed.is_zero() && self.loops == 0 {
            return;
        }
        self.log_window(elapsed, true);
    }

    fn log_window(&self, elapsed: Duration, final_window: bool) {
        let elapsed_secs = elapsed.as_secs_f64();
        let loaded_tps = if elapsed_secs > 0.0 {
            self.loaded_txs as f64 / elapsed_secs
        } else {
            0.0
        };
        info!(
            final_window,
            window_ms = elapsed.as_millis() as u64,
            page_size = self.page_size,
            subscriber_buffer_capacity = self.subscriber_buffer_capacity,
            idle_poll_interval_ms = self.idle_poll_interval.as_millis() as u64,
            loops = self.loops,
            empty_polls = self.empty_polls,
            read_errors = self.read_errors,
            loaded_txs = self.loaded_txs,
            loaded_tps = loaded_tps,
            fanout_delivered = self.fanout_delivered,
            dropped_closed = self.dropped_closed,
            dropped_full = self.dropped_full,
            max_subscribers_before = self.max_subscribers_before,
            max_subscribers_after = self.max_subscribers_after,
            read_phase_ms = self.read_phase.as_millis() as u64,
            fanout_phase_ms = self.fanout_phase.as_millis() as u64,
            idle_sleep_ms = self.idle_sleep.as_millis() as u64,
            "l2 tx broadcaster metrics"
        );
    }

    fn reset_window(&mut self) {
        self.window_started_at = Instant::now();
        self.loops = 0;
        self.empty_polls = 0;
        self.read_errors = 0;
        self.loaded_txs = 0;
        self.fanout_delivered = 0;
        self.dropped_closed = 0;
        self.dropped_full = 0;
        self.max_subscribers_before = 0;
        self.max_subscribers_after = 0;
        self.read_phase = Duration::ZERO;
        self.fanout_phase = Duration::ZERO;
        self.idle_sleep = Duration::ZERO;
    }
}

fn elapsed_or_zero(started_at: Option<Instant>) -> Duration {
    started_at.map_or(Duration::ZERO, |value| value.elapsed())
}
