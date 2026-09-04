// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shutdown signalling + terminal-fault containment inside a pre-armed run.
//!
//! Containment is classification-at-birth, in this order:
//!
//! - [`RuntimeScope::contain_storage_invariant_failure`]: elect the first
//!   reporter (CAS), set the sticky containment bit, arm the terminal abort
//!   watchdog, then request shutdown. The watchdog precedes cancellation
//!   because a drain may block. Containment writes nothing durable: the
//!   black box's terminal-cause row is written once, by the command bracket
//!   at settlement, from the verdict `finish` returns.
//! - [`RuntimeScope::is_storage_invariant_contained`]: checked by
//!   externalization sites (acks, L1 sends, WS frames, snapshot stream
//!   starts) before emitting; set only by containment, so a missed check is
//!   bounded by the exit contract (terminal exits are not restarted) and
//!   by the I15 freeze triggers on the tables they cover — partial
//!   backstops, not a barrier.
//!
//! Honest bounds: the black box's terminal-cause row is best-effort
//! telemetry (restart policy is the exit contract, and a persistent fault
//! re-detects fail-loud on the next boot that reads it).
//! In a process-lock-backed runtime, terminal containment gives the complete
//! runtime lifetime two seconds to drain before aborting the process. Ordinary
//! operator/recovery shutdown remains cooperatively unbounded.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::process_lock::{ProcessLock, ProcessLockWitness};

const TERMINAL_ABORT_TIMEOUT: Duration = Duration::from_secs(2);

type AbortAction = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
struct TerminalAbortWatchdog {
    runtime_lifetime: ProcessLockWitness,
    timeout: Duration,
    abort_action: AbortAction,
}

impl TerminalAbortWatchdog {
    fn production(runtime_lifetime: ProcessLockWitness) -> Self {
        Self {
            runtime_lifetime,
            timeout: TERMINAL_ABORT_TIMEOUT,
            abort_action: Arc::new(|| std::process::abort()),
        }
    }

    fn arm(&self) {
        let runtime_lifetime = self.runtime_lifetime.clone();
        let deadline = Instant::now() + self.timeout;
        let abort_action = self.abort_action.clone();
        let spawn_failure_abort = self.abort_action.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("sequencer-terminal-abort".into())
            .spawn(move || {
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                if runtime_lifetime.is_held() {
                    abort_action();
                }
            })
        {
            // Losing the independent timer would silently discard the hard
            // terminal-shutdown bound. Abort before attempting synchronous
            // logging: a blocked subscriber must not defeat fail-closed
            // behavior under the same resource pressure that prevented the
            // watchdog thread from starting. Production's action never
            // returns; test actions may, so retain the diagnostic afterward.
            spawn_failure_abort();
            tracing::error!(%error, "failed to arm terminal abort watchdog");
        }
    }
}

/// Cooperative shutdown notification — exactly what the name says, and
/// nothing else. Freely `Default`-constructible; carries no authority.
#[derive(Clone, Default)]
pub struct ShutdownSignal {
    is_shutting_down: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

/// The command/runtime-lifetime capability the ADR calls `RuntimeScope`:
/// exclusive data-directory ownership plus terminal-fault containment. Only
/// constructible from a held [`ProcessLock`] — installed at lock
/// acquisition, before any signal or worker exists — so a watchdog-less or
/// lock-less containment object is unrepresentable in production.
/// Workers that externalize or contain faults take a scope (the lane, the
/// HTTP server, the submitter); workers that only need to stop take its
/// [`ShutdownSignal`] (the reader, the detector, the fee oracle) and hold
/// their data-directory ownership as a construction-required
/// [`ProcessLock`] instead.
#[derive(Clone)]
pub struct RuntimeScope {
    signal: ShutdownSignal,
    /// Single containment authority: winning this `OnceLock` *is* the sticky
    /// containment bit, so the bit and its cause become visible together —
    /// there is no window where containment reads true with no cause.
    first_containment_cause: Arc<std::sync::OnceLock<String>>,
    terminal_abort_watchdog: TerminalAbortWatchdog,
    /// Runtime-lifetime ownership. Every scope clone retains the lock;
    /// nested blocking tasks retain their own clone through
    /// [`crate::runtime::process_lock::spawn_blocking_with_lock`], so
    /// data-directory exclusivity outlives detached work.
    process_lock: ProcessLock,
}

impl RuntimeScope {
    /// Create the scope that owns a command/runtime lifetime. The process
    /// lock is shared by every clone and released only after the final owner
    /// drops it, including on partial startup failure or caller cancellation.
    pub(crate) fn new(process_lock: ProcessLock) -> Self {
        Self {
            signal: ShutdownSignal::default(),
            first_containment_cause: Arc::default(),
            terminal_abort_watchdog: TerminalAbortWatchdog::production(process_lock.witness()),
            process_lock,
        }
    }

    #[cfg(test)]
    fn with_test_terminal_abort_watchdog(
        process_lock: ProcessLock,
        timeout: Duration,
        abort_action: AbortAction,
    ) -> Self {
        Self {
            signal: ShutdownSignal::default(),
            first_containment_cause: Arc::default(),
            terminal_abort_watchdog: TerminalAbortWatchdog {
                runtime_lifetime: process_lock.witness(),
                timeout,
                abort_action,
            },
            process_lock,
        }
    }

    /// The pure notification half, for consumers that only wait for stop.
    pub(crate) fn signal(&self) -> ShutdownSignal {
        self.signal.clone()
    }

    /// The held data-directory lock, for nested blocking work.
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

    /// Contain a persistent storage invariant failure: CAS-elect the first
    /// reporter, set the sticky containment bit, arm the terminal watchdog,
    /// then request shutdown. Sync — callable from any thread, async or
    /// blocking — and it touches no storage: the cause reaches the black box
    /// through the command bracket's settlement write, not from here.
    ///
    /// This is containment, not recovery: the supervisor maps the contained
    /// state to the terminal exit class (30 — do not restart, page). A
    /// persistent fault re-detects fail-loud on any boot that reads it; the
    /// black-box row is the cause's telemetry, not a boot gate.
    pub(crate) fn contain_storage_invariant_failure(&self, cause: impl Into<String>) {
        // First-winner election: setting the cause is the containment bit,
        // so exactly one reporter proceeds and echoes return immediately
        // (their causes are already in the error logs).
        if self.first_containment_cause.set(cause.into()).is_err() {
            return;
        }
        // The independent watchdog must precede cooperative cancellation: a
        // drain may block while runtime work retains the process-lifetime
        // capability.
        self.terminal_abort_watchdog.arm();
        self.request_shutdown();
    }

    /// Whether a terminal fault has been contained. Externalization sites
    /// (acks, sends, frames, streams) check this before emitting — through
    /// [`Self::authorize`], whose token their effect functions require. True
    /// iff [`Self::containment_cause`] is present — one authority, no window.
    pub(crate) fn is_storage_invariant_contained(&self) -> bool {
        self.first_containment_cause.get().is_some()
    }

    pub(crate) fn containment_cause(&self) -> Option<&str> {
        self.first_containment_cause.get().map(String::as_str)
    }

    /// Consult containment and mint the externalization token: `None` once a
    /// terminal fault is contained. Three effect functions require an
    /// [`Authorized`] in their signature — the user-op acknowledgement, the
    /// L1 send, and the WS emit — so at those sites forgetting the check is
    /// a compile error, not a convention. The remaining consults are
    /// hand-placed and bounded by the exit contract: the snapshot-stream
    /// start, the `POST /tx` success body, and the lane's batch-close and
    /// reconciliation commits. This is the honest bounded-lag consult, not a
    /// fence: a token minted before a concurrent containment may finish its
    /// already-authorized effect (ADR).
    pub(crate) fn authorize(&self) -> Option<Authorized<'_>> {
        if self.is_storage_invariant_contained() {
            None
        } else {
            Some(Authorized {
                _scope: std::marker::PhantomData,
            })
        }
    }
}

/// Zero-sized, borrow-scoped proof that terminal containment was consulted
/// and found clear at some point in this borrow — not at the instant of the
/// effect: the token is `Copy` and lives as long as the scope borrow, so a
/// consumer that awaits between minting and effect carries the ADR's
/// bounded lag. Obtainable only from
/// [`RuntimeScope::authorize`]; carries no runtime state — this is the
/// type-level obligation the ADR's rejected `EffectGate` was not (no mutex,
/// no actor, no second state machine).
#[derive(Clone, Copy)]
pub(crate) struct Authorized<'scope> {
    _scope: std::marker::PhantomData<&'scope RuntimeScope>,
}

/// Test scope: a leaked temp-dir lock (bounded by test count) plus a no-op
/// abort watchdog, so containing a fault in a component test neither needs a
/// data directory nor risks aborting the test binary at the 2s deadline.
#[cfg(test)]
impl Default for RuntimeScope {
    fn default() -> Self {
        let dir = tempfile::tempdir().expect("test scope tempdir");
        let lock =
            ProcessLock::acquire(dir.path().to_str().expect("utf8 path")).expect("test scope lock");
        std::mem::forget(dir);
        Self::with_test_terminal_abort_watchdog(lock, TERMINAL_ABORT_TIMEOUT, Arc::new(|| {}))
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

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_WATCHDOG_TIMEOUT: Duration = Duration::from_millis(25);

    fn signal_with_test_watchdog(abort_action: AbortAction) -> RuntimeScope {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = ProcessLock::acquire(dir.path().to_str().expect("utf8 path")).expect("lock");
        // The held descriptor remains valid after the temporary directory is
        // removed; the path itself is irrelevant to the lifetime witness.
        RuntimeScope::with_test_terminal_abort_watchdog(lock, TEST_WATCHDOG_TIMEOUT, abort_action)
    }

    #[test]
    fn containment_elects_first_reporter_and_requests_shutdown() {
        let signal = RuntimeScope::default();

        assert!(!signal.is_storage_invariant_contained());
        assert!(signal.authorize().is_some());
        signal.contain_storage_invariant_failure("first cause");
        signal.contain_storage_invariant_failure("second cause (echo)");

        // The CAS elects exactly one reporter; echoes return immediately,
        // and the bit, its cause, and the shutdown request are one step.
        assert!(signal.is_storage_invariant_contained());
        assert!(signal.is_shutdown_requested());
        assert!(signal.authorize().is_none());
        assert_eq!(signal.containment_cause(), Some("first cause"));
    }

    #[test]
    fn plain_shutdown_is_not_containment() {
        let signal = RuntimeScope::default();
        signal.request_shutdown();
        assert!(signal.is_shutdown_requested());
        assert!(!signal.is_storage_invariant_contained());
    }

    #[test]
    fn watchdog_noops_after_every_runtime_owner_drops() {
        let (abort_tx, abort_rx) = std::sync::mpsc::channel();
        let signal = signal_with_test_watchdog(Arc::new(move || {
            let _ = abort_tx.send(());
        }));

        signal.contain_storage_invariant_failure("terminal fault");
        drop(signal);

        assert!(
            abort_rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "a completed runtime drain must suppress the abort"
        );
    }

    #[test]
    fn retained_lock_clone_keeps_the_watchdog_live_after_signal_drops() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = ProcessLock::acquire(dir.path().to_str().expect("utf8 path")).expect("lock");
        let nested_work_lock = lock.clone();
        let (abort_tx, abort_rx) = std::sync::mpsc::channel();
        let signal = RuntimeScope::with_test_terminal_abort_watchdog(
            lock,
            TEST_WATCHDOG_TIMEOUT,
            Arc::new(move || {
                let _ = abort_tx.send(());
            }),
        );

        signal.contain_storage_invariant_failure("terminal fault");
        drop(signal);

        abort_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("nested work retaining the process lock must trigger abort");
        drop(nested_work_lock);
    }

    #[test]
    fn first_reporter_arms_exactly_one_watchdog() {
        let (abort_tx, abort_rx) = std::sync::mpsc::channel();
        let signal = signal_with_test_watchdog(Arc::new(move || {
            let _ = abort_tx.send(());
        }));

        signal.contain_storage_invariant_failure("first cause");
        signal.contain_storage_invariant_failure("echo");
        abort_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first watchdog action");
        assert!(
            abort_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "an echo must not arm another deadline"
        );
        assert_eq!(signal.containment_cause(), Some("first cause"));
    }

    #[test]
    fn ordinary_shutdown_does_not_arm_the_terminal_watchdog() {
        let (abort_tx, abort_rx) = std::sync::mpsc::channel();
        let signal = signal_with_test_watchdog(Arc::new(move || {
            let _ = abort_tx.send(());
        }));

        signal.request_shutdown();

        assert!(
            abort_rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "ordinary operator shutdown remains unbounded and graceful"
        );
    }
}
