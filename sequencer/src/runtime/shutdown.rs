// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct ShutdownSignal {
    is_shutting_down: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl ShutdownSignal {
    pub fn request_shutdown(&self) {
        let was_shutting_down = self.is_shutting_down.swap(true, Ordering::SeqCst);
        if !was_shutting_down {
            self.notify.notify_waiters();
        }
    }

    pub fn is_shutdown_requested(&self) -> bool {
        self.is_shutting_down.load(Ordering::SeqCst)
    }

    pub async fn wait_for_shutdown(&self) {
        if self.is_shutdown_requested() {
            return;
        }

        loop {
            let notified = self.notify.notified();
            if self.is_shutdown_requested() {
                return;
            }
            notified.await;
            if self.is_shutdown_requested() {
                return;
            }
        }
    }
}
