// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

//! Shutdown signalling + terminal-fault containment inside a pre-armed run.
//!
//! Containment is classification-at-birth, in this order:
//!
//! - [`RuntimeScope::contain_storage_invariant_failure`]: elect the first
//!   reporter (CAS), set the sticky containment bit, arm the terminal abort
//!   watchdog, request shutdown, then invoke the durable recorder (the
//!   black box's terminal-cause row, installed at worker spawn). The watchdog
//!   precedes both cancellation and recording because either may block.
//! - [`ShutdownSignal::is_storage_invariant_contained`]: checked by
//!   externalization sites (acks, L1 sends, WS frames, snapshot stream
//!   starts) before emitting; set only by containment, so a missed check is
//!   bounded by the R4 exit contract (terminal exits are not restarted) and
//!   by the I15 freeze triggers on the tables they cover — partial
//!   backstops, not a barrier.
//!
//! Honest bounds: the black box's terminal-cause row is best-effort
//! telemetry (L2/L3: restart policy is the R4 exit contract, and a persistent fault
//! re-detects fail-loud on the next boot that reads it — there is no
//! database boot gate to keep durable).
//! In a process-lock-backed runtime, terminal containment gives the complete
//! runtime lifetime two seconds to drain before aborting the process. Ordinary
//! operator/recovery shutdown remains cooperatively unbounded.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::process_lock::{ProcessLock, ProcessLockWitness};

const TERMINAL_ABORT_TIMEOUT: Duration = Duration::from_secs(2);

/// Durable terminal-fault recorder installed at worker spawn (appends the
/// black box's terminal-cause row).
pub(crate) type FaultRecorder = Arc<dyn Fn(&str) + Send + Sync>;

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
/// nothing else (H2). Freely `Default`-constructible; carries no authority.
#[derive(Clone, Default)]
pub struct ShutdownSignal {
    is_shutting_down: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

/// The command/runtime-lifetime capability the ADR calls `RuntimeScope`:
/// exclusive data-directory ownership plus terminal-fault containment. Only
/// constructible from a held [`ProcessLock`] — installed at lock
/// acquisition, before any signal or worker exists — so a watchdog-less or
/// lock-less containment object is unrepresentable in production (D2/H2).
/// Workers that touch the data directory or externalize take a scope;
/// pure-notification consumers take its [`ShutdownSignal`].
#[derive(Clone)]
pub struct RuntimeScope {
    signal: ShutdownSignal,
    /// Single containment authority: winning this `OnceLock` *is* the sticky
    /// containment bit, so the bit and its cause become visible together —
    /// there is no window where containment reads true with no cause (D5).
    first_containment_cause: Arc<std::sync::OnceLock<String>>,
    /// Durable fault recorder (the black box's terminal-cause row), installed
    /// once during runtime preparation. Invoked only after the containment
    /// bit, watchdog, and shutdown request — recording can block, and no new
    /// externalization may be authorized while it runs. Best-effort
    /// telemetry: when it fails, the cause is still in the logs, the process
    /// still exits terminal, and a persistent fault re-detects on the next
    /// boot that reads it (L2).
    fault_recorder: Arc<std::sync::OnceLock<FaultRecorder>>,
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
            fault_recorder: Arc::default(),
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
            fault_recorder: Arc::default(),
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
    /// request shutdown, then invoke the durable recorder (the black box's
    /// terminal-cause row, best-effort). Sync — callable from any thread,
    /// async or blocking.
    ///
    /// This is containment, not recovery: the supervisor maps the contained
    /// state to the terminal exit class (30 — do not restart, page). A
    /// persistent fault re-detects fail-loud on any boot that reads it; the
    /// black-box row is the cause's telemetry, not a boot gate (L2/L3).
    pub(crate) fn contain_storage_invariant_failure(&self, cause: impl Into<String>) {
        // First-winner election: setting the cause is the containment bit,
        // so exactly one reporter proceeds and echoes return immediately
        // (their causes are already in the error logs).
        if self.first_containment_cause.set(cause.into()).is_err() {
            return;
        }
        // The independent watchdog must precede both cooperative cancellation
        // and audit recording: either may block while runtime work retains the
        // process-lifetime capability.
        self.terminal_abort_watchdog.arm();
        self.request_shutdown();
        if let Some(recorder) = self.fault_recorder.get() {
            recorder(
                self.first_containment_cause
                    .get()
                    .expect("cause was just installed by the elected reporter"),
            );
        }
    }

    /// Install the durable recorder (once; later installs are ignored).
    pub(crate) fn set_fault_recorder(&self, recorder: FaultRecorder) {
        let _ = self.fault_recorder.set(recorder);
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
    /// terminal fault is contained. The authority-bearing effect functions
    /// (acknowledge, L1 send, WS emit, snapshot-stream start) take an
    /// [`Authorized`], so a new externalization site cannot forget the check
    /// — the compile error replaces the convention (S-A). This is the honest
    /// bounded-lag consult, not a fence: a token minted before a concurrent
    /// containment may finish its already-authorized effect (ADR).
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
/// and found clear at this effect boundary. Obtainable only from
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
    use std::sync::Mutex;

    const TEST_WATCHDOG_TIMEOUT: Duration = Duration::from_millis(25);

    fn signal_with_test_watchdog(abort_action: AbortAction) -> RuntimeScope {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = ProcessLock::acquire(dir.path().to_str().expect("utf8 path")).expect("lock");
        // The held descriptor remains valid after the temporary directory is
        // removed; the path itself is irrelevant to the lifetime witness.
        RuntimeScope::with_test_terminal_abort_watchdog(lock, TEST_WATCHDOG_TIMEOUT, abort_action)
    }

    #[test]
    fn containment_elects_first_reporter_and_closes_before_recording() {
        let signal = RuntimeScope::default();
        let recorded: Arc<Mutex<Vec<String>>> = Arc::default();
        signal.set_fault_recorder({
            let signal_view = signal.clone();
            let recorded = recorded.clone();
            Arc::new(move |cause| {
                // The bit must already be visible while the (possibly slow)
                // durable recording runs: no new externalization is
                // authorized during the lifecycle recorder write.
                assert!(signal_view.is_storage_invariant_contained());
                assert!(signal_view.is_shutdown_requested());
                recorded.lock().unwrap().push(cause.to_string());
            })
        });

        assert!(!signal.is_storage_invariant_contained());
        signal.contain_storage_invariant_failure("first cause");
        signal.contain_storage_invariant_failure("second cause (echo)");

        assert!(signal.is_storage_invariant_contained());
        // The CAS elects exactly one reporter; echoes return immediately.
        assert_eq!(*recorded.lock().unwrap(), vec!["first cause".to_string()]);
    }

    #[test]
    fn plain_shutdown_is_not_containment() {
        let signal = RuntimeScope::default();
        signal.request_shutdown();
        assert!(signal.is_shutdown_requested());
        assert!(!signal.is_storage_invariant_contained());
    }

    #[test]
    fn watchdog_fires_while_the_fault_recorder_is_blocked() {
        let (abort_tx, abort_rx) = std::sync::mpsc::channel();
        let signal = signal_with_test_watchdog(Arc::new(move || {
            let _ = abort_tx.send(());
        }));
        let recorder_entered = Arc::new(std::sync::Barrier::new(2));
        let release_recorder = Arc::new(std::sync::Barrier::new(2));
        let recorder_entered_for_callback = recorder_entered.clone();
        let release_recorder_for_callback = release_recorder.clone();
        signal.set_fault_recorder(Arc::new(move |_| {
            recorder_entered_for_callback.wait();
            release_recorder_for_callback.wait();
        }));

        let reporter = {
            let signal = signal.clone();
            std::thread::spawn(move || signal.contain_storage_invariant_failure("terminal fault"))
        };
        recorder_entered.wait();
        abort_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("watchdog must fire while recorder remains blocked");
        assert!(signal.is_storage_invariant_contained());
        assert!(signal.is_shutdown_requested());

        release_recorder.wait();
        reporter.join().expect("reporter thread");
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
