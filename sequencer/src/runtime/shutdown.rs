// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Cooperative ordinary shutdown and immediate terminal runtime failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use super::process_lock::ProcessLock;

/// Stop a runtime whose terminal fault leaves no supported continuation.
/// Logging is best-effort: an operator-supplied blocking subscriber can delay
/// this call. No worker drain or durable settlement precedes process abort.
pub(crate) fn abort_terminal(error: impl std::fmt::Display) -> ! {
    tracing::error!(error = %error, "terminal runtime failure; aborting process");
    std::process::abort()
}

/// Cooperative shutdown notification; carries no data-directory authority.
#[derive(Clone, Default)]
pub struct ShutdownSignal {
    is_shutting_down: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

/// Runtime lifetime: exclusive data-directory ownership plus ordinary shutdown.
/// Every clone retains the process lock, including detached blocking work.
#[derive(Clone)]
pub struct RuntimeScope {
    signal: ShutdownSignal,
    process_lock: ProcessLock,
}

impl RuntimeScope {
    pub(crate) fn new(process_lock: ProcessLock) -> Self {
        Self {
            signal: ShutdownSignal::default(),
            process_lock,
        }
    }

    pub(crate) fn signal(&self) -> ShutdownSignal {
        self.signal.clone()
    }
    pub(crate) fn process_lock(&self) -> ProcessLock {
        self.process_lock.clone()
    }
    pub fn request_shutdown(&self) {
        self.signal.request_shutdown();
    }
    pub fn is_shutdown_requested(&self) -> bool {
        self.signal.is_shutdown_requested()
    }
    pub async fn wait_for_shutdown(&self) {
        self.signal.wait_for_shutdown().await;
    }
}

#[cfg(test)]
impl Default for RuntimeScope {
    fn default() -> Self {
        Self::new(ProcessLock::test())
    }
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

/// Re-run one test in a child and require the real production abort. Returns
/// true only in that child, so callers keep their corruption fixture in place.
#[cfg(all(test, unix))]
pub(crate) fn abort_test_child(test_name: &str) -> bool {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const CHILD_ENV: &str = "SEQUENCER_ABORT_TEST";
    if std::env::var(CHILD_ENV).as_deref() == Ok(test_name) {
        return true;
    }
    let directory = tempfile::tempdir().expect("abort test directory");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD_ENV, test_name)
        .current_dir(directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn abort test");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll abort test") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{test_name} did not abort before the test deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        status.signal(),
        Some(6),
        "{test_name} must terminate with SIGABRT: {status}"
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ordinary_shutdown_notifies_current_and_later_waiters() {
        let scope = RuntimeScope::default();
        let waiter = tokio::spawn({
            let scope = scope.clone();
            async move { scope.wait_for_shutdown().await }
        });
        scope.request_shutdown();
        waiter.await.expect("shutdown waiter");
        scope.wait_for_shutdown().await;
        assert!(scope.is_shutdown_requested());
    }

    #[test]
    #[cfg(unix)]
    fn terminal_failure_aborts_with_runtime_owner_alive() {
        if !abort_test_child(
            "runtime::shutdown::tests::terminal_failure_aborts_with_runtime_owner_alive",
        ) {
            return;
        }
        let _scope = RuntimeScope::default();
        abort_terminal("terminal fault probe");
    }
}
