// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::time::{Duration, Instant};
use tracing::info;

#[derive(Debug)]
pub(super) struct InclusionLaneMetrics {
    enabled: bool,
    log_interval: Duration,
    window_started_at: Instant,
    loops: u64,
    included_user_ops: u64,
    drained_direct_inputs: u64,
    frame_only_closes: u64,
    frame_and_batch_closes: u64,
    idle_sleeps: u64,
    max_queue_depth: usize,
    user_op_phase: Duration,
    user_op_dequeue_phase: Duration,
    user_op_app_execute_phase: Duration,
    user_op_persist_phase: Duration,
    user_op_ack_phase: Duration,
    direct_phase: Duration,
    close_phase: Duration,
    idle_sleep: Duration,
}

impl InclusionLaneMetrics {
    pub(super) fn new(enabled: bool, log_interval: Duration) -> Self {
        Self {
            enabled,
            log_interval,
            window_started_at: Instant::now(),
            loops: 0,
            included_user_ops: 0,
            drained_direct_inputs: 0,
            frame_only_closes: 0,
            frame_and_batch_closes: 0,
            idle_sleeps: 0,
            max_queue_depth: 0,
            user_op_phase: Duration::ZERO,
            user_op_dequeue_phase: Duration::ZERO,
            user_op_app_execute_phase: Duration::ZERO,
            user_op_persist_phase: Duration::ZERO,
            user_op_ack_phase: Duration::ZERO,
            direct_phase: Duration::ZERO,
            close_phase: Duration::ZERO,
            idle_sleep: Duration::ZERO,
        }
    }

    pub(super) fn phase_started_at(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    pub(super) fn on_loop_start(&mut self, queue_depth: usize) {
        if !self.enabled {
            return;
        }
        self.loops = self.loops.saturating_add(1);
        self.max_queue_depth = self.max_queue_depth.max(queue_depth);
    }

    pub(super) fn on_user_ops_phase_end(
        &mut self,
        started_at: Option<Instant>,
        included_user_ops: u64,
    ) {
        if !self.enabled {
            return;
        }
        self.included_user_ops = self.included_user_ops.saturating_add(included_user_ops);
        self.user_op_phase = self
            .user_op_phase
            .saturating_add(elapsed_or_zero(started_at));
    }

    pub(super) fn on_user_op_dequeue_end(&mut self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        self.user_op_dequeue_phase = self.user_op_dequeue_phase.saturating_add(elapsed);
    }

    pub(super) fn on_user_op_app_execute_end(&mut self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        self.user_op_app_execute_phase = self.user_op_app_execute_phase.saturating_add(elapsed);
    }

    pub(super) fn on_user_op_persist_end(&mut self, started_at: Option<Instant>) {
        if !self.enabled {
            return;
        }
        self.user_op_persist_phase = self
            .user_op_persist_phase
            .saturating_add(elapsed_or_zero(started_at));
    }

    pub(super) fn on_user_op_ack_end(&mut self, started_at: Option<Instant>) {
        if !self.enabled {
            return;
        }
        self.user_op_ack_phase = self
            .user_op_ack_phase
            .saturating_add(elapsed_or_zero(started_at));
    }

    pub(super) fn on_directs_phase_end(
        &mut self,
        started_at: Option<Instant>,
        drained_direct_inputs: u64,
    ) {
        if !self.enabled {
            return;
        }
        self.drained_direct_inputs = self
            .drained_direct_inputs
            .saturating_add(drained_direct_inputs);
        self.direct_phase = self
            .direct_phase
            .saturating_add(elapsed_or_zero(started_at));
    }

    pub(super) fn on_close_phase_end(&mut self, started_at: Option<Instant>, closed_batch: bool) {
        if !self.enabled {
            return;
        }
        if closed_batch {
            self.frame_and_batch_closes = self.frame_and_batch_closes.saturating_add(1);
        } else {
            self.frame_only_closes = self.frame_only_closes.saturating_add(1);
        }
        self.close_phase = self.close_phase.saturating_add(elapsed_or_zero(started_at));
    }

    pub(super) fn on_idle_sleep_end(&mut self, started_at: Option<Instant>) {
        if !self.enabled {
            return;
        }
        self.idle_sleeps = self.idle_sleeps.saturating_add(1);
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
        let included_tps = if elapsed_secs > 0.0 {
            self.included_user_ops as f64 / elapsed_secs
        } else {
            0.0
        };
        let user_op_dequeue_share_pct = percentage(
            self.user_op_dequeue_phase.as_nanos(),
            self.user_op_phase.as_nanos(),
        );
        let user_op_app_execute_share_pct = percentage(
            self.user_op_app_execute_phase.as_nanos(),
            self.user_op_phase.as_nanos(),
        );
        let user_op_persist_share_pct = percentage(
            self.user_op_persist_phase.as_nanos(),
            self.user_op_phase.as_nanos(),
        );
        let user_op_ack_share_pct = percentage(
            self.user_op_ack_phase.as_nanos(),
            self.user_op_phase.as_nanos(),
        );
        let app_plus_persist = self
            .user_op_app_execute_phase
            .saturating_add(self.user_op_persist_phase);
        let user_op_app_share_pct_of_app_plus_persist = percentage(
            self.user_op_app_execute_phase.as_nanos(),
            app_plus_persist.as_nanos(),
        );
        let user_op_persist_share_pct_of_app_plus_persist = percentage(
            self.user_op_persist_phase.as_nanos(),
            app_plus_persist.as_nanos(),
        );
        info!(
            final_window,
            window_ms = elapsed.as_millis() as u64,
            loops = self.loops,
            included_user_ops = self.included_user_ops,
            drained_direct_inputs = self.drained_direct_inputs,
            included_tps = included_tps,
            frame_only_closes = self.frame_only_closes,
            frame_and_batch_closes = self.frame_and_batch_closes,
            idle_sleeps = self.idle_sleeps,
            max_queue_depth = self.max_queue_depth,
            user_op_phase_ms = self.user_op_phase.as_millis() as u64,
            user_op_dequeue_phase_ms = self.user_op_dequeue_phase.as_millis() as u64,
            user_op_app_execute_phase_ms = self.user_op_app_execute_phase.as_millis() as u64,
            user_op_persist_phase_ms = self.user_op_persist_phase.as_millis() as u64,
            user_op_ack_phase_ms = self.user_op_ack_phase.as_millis() as u64,
            user_op_dequeue_share_pct = user_op_dequeue_share_pct,
            user_op_app_execute_share_pct = user_op_app_execute_share_pct,
            user_op_persist_share_pct = user_op_persist_share_pct,
            user_op_ack_share_pct = user_op_ack_share_pct,
            user_op_app_share_pct_of_app_plus_persist = user_op_app_share_pct_of_app_plus_persist,
            user_op_persist_share_pct_of_app_plus_persist =
                user_op_persist_share_pct_of_app_plus_persist,
            direct_phase_ms = self.direct_phase.as_millis() as u64,
            close_phase_ms = self.close_phase.as_millis() as u64,
            idle_sleep_ms = self.idle_sleep.as_millis() as u64,
            "inclusion lane metrics"
        );
    }

    fn reset_window(&mut self) {
        self.window_started_at = Instant::now();
        self.loops = 0;
        self.included_user_ops = 0;
        self.drained_direct_inputs = 0;
        self.frame_only_closes = 0;
        self.frame_and_batch_closes = 0;
        self.idle_sleeps = 0;
        self.max_queue_depth = 0;
        self.user_op_phase = Duration::ZERO;
        self.user_op_dequeue_phase = Duration::ZERO;
        self.user_op_app_execute_phase = Duration::ZERO;
        self.user_op_persist_phase = Duration::ZERO;
        self.user_op_ack_phase = Duration::ZERO;
        self.direct_phase = Duration::ZERO;
        self.close_phase = Duration::ZERO;
        self.idle_sleep = Duration::ZERO;
    }
}

fn elapsed_or_zero(started_at: Option<Instant>) -> Duration {
    started_at.map_or(Duration::ZERO, |value| value.elapsed())
}

fn percentage(part: u128, total: u128) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (part as f64) * 100.0 / (total as f64)
}
