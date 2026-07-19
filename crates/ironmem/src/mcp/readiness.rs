//! Terminal-resolution readiness gate for the MCP server's warm-up window.
//!
//! During server startup, background memory init (model load + bootstrap)
//! can take long enough that write-shaped tool handlers would otherwise have
//! to choose between racing ahead unsafely or no-op'ing with a
//! success-shaped body (losing writes). `ReadinessGate` gives those
//! handlers something to block on instead: a bounded wait that returns as
//! soon as readiness resolves, or a bounded timeout as a fail-safe crash
//! guard so a handler never hangs forever.
//!
//! Read-shaped tools keep using the existing lock-free fast path via
//! [`ReadinessGate::is_ready`] (mirrors today's `is_warming_up()` check).
//! Write-shaped tools (wired up in a later task) will instead call
//! [`ReadinessGate::wait_for_write`].
//!
//! This module is intentionally `std`-only (no `tokio`/async): call sites
//! are synchronous tool handlers invoked from inside `block_in_place`, per
//! this crate's existing daemon architecture.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::error::MemoryError;

/// Internal terminal-resolution state guarded by the gate's `Mutex`.
///
/// `Pending` is the only non-terminal state. Once the state transitions to
/// `Ready` or `Failed`, it never changes again (first resolution wins — see
/// [`ReadinessGate::resolve_ready`] / [`ReadinessGate::resolve_failed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadinessState {
    Pending,
    Ready,
    Failed(String),
}

impl ReadinessState {
    fn is_terminal(&self) -> bool {
        !matches!(self, ReadinessState::Pending)
    }
}

/// A readiness gate write-shaped MCP tool handlers can block on (bounded)
/// instead of silently no-op'ing during server warm-up.
///
/// Internally this is an `AtomicBool` (for the lock-free `is_ready()` fast
/// path, matching today's `is_warming_up()` semantics) plus a
/// `Mutex<ReadinessState>` + `Condvar` for the blocking wait path used by
/// [`ReadinessGate::wait_for_write`].
///
/// # Idempotency policy
///
/// [`ReadinessGate::resolve_ready`] and [`ReadinessGate::resolve_failed`] are
/// both idempotent: the **first** call that moves the state out of
/// `Pending` wins, and every subsequent call (including a call to the other
/// method) is a safe no-op. The state never downgrades `Ready` -> `Failed`
/// or vice versa, and neither method ever panics on a redundant call.
pub struct ReadinessGate {
    /// Lock-free fast path for `is_ready()`. Set `true` only when the gate
    /// resolves to `Ready` (never set on `Failed`).
    ready_flag: AtomicBool,
    state: Mutex<ReadinessState>,
    condvar: Condvar,
}

impl ReadinessGate {
    /// Constructs a gate that starts unresolved (`Pending`). Mirrors
    /// `App::new_server_ready`'s `memory_ready = false` — used when the
    /// caller wants callers of `wait_for_write` to actually block until
    /// something later calls `resolve_ready`/`resolve_failed`.
    pub fn new_pending() -> Self {
        Self {
            ready_flag: AtomicBool::new(false),
            state: Mutex::new(ReadinessState::Pending),
            condvar: Condvar::new(),
        }
    }

    /// Constructs a gate that starts already resolved-ready. Mirrors
    /// `App::new`/`open_for_test`'s `memory_ready = true` — used when there
    /// is no warm-up window and readiness is immediate.
    pub fn new_ready() -> Self {
        Self {
            ready_flag: AtomicBool::new(true),
            state: Mutex::new(ReadinessState::Ready),
            condvar: Condvar::new(),
        }
    }

    /// Lock-free readiness check. Preserves today's `is_warming_up()`
    /// semantics (`!memory_ready.load(Relaxed)`): a plain `Relaxed` load of
    /// the underlying `AtomicBool`, no stronger ordering introduced.
    pub fn is_ready(&self) -> bool {
        self.ready_flag.load(Ordering::Relaxed)
    }

    /// Resolves the gate to `Ready`, waking all current/future waiters.
    ///
    /// Idempotent: if the state is already terminal (`Ready` or `Failed`),
    /// this is a safe no-op — the first resolution always wins.
    pub fn resolve_ready(&self) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.is_terminal() {
            return;
        }
        *state = ReadinessState::Ready;
        self.ready_flag.store(true, Ordering::Relaxed);
        drop(state);
        self.condvar.notify_all();
    }

    /// Resolves the gate to `Failed(reason)`, waking all current/future
    /// waiters.
    ///
    /// Idempotent: if the state is already terminal (`Ready` or `Failed`),
    /// this is a safe no-op — the first resolution always wins.
    pub fn resolve_failed(&self, reason: String) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.is_terminal() {
            return;
        }
        *state = ReadinessState::Failed(reason);
        drop(state);
        self.condvar.notify_all();
    }

    /// Blocks the calling thread until the gate resolves, or `timeout`
    /// elapses, whichever comes first.
    ///
    /// - Returns `Ok(())` immediately if already `Ready` — a brief mutex
    ///   lock to read the terminal state, but the condvar is never touched
    ///   and the calling thread never blocks/parks.
    /// - Returns `Err` immediately if already `Failed`, same fast path.
    /// - Otherwise blocks on the condvar (no busy-spin, no
    ///   `thread::sleep`); returns `Ok(())` if woken by `resolve_ready`,
    ///   `Err` if woken by `resolve_failed`, or `Err` if `timeout` elapses
    ///   while still `Pending`.
    ///
    /// The timeout is a fail-safe crash guard (bounding how long a write
    /// handler can block), not a synchronization mechanism — callers
    /// should source it from config with a default generous enough for a
    /// real model load.
    pub fn wait_for_write(&self, timeout: Duration) -> Result<(), MemoryError> {
        // Fast path: check without ever touching the mutex/condvar if
        // already resolved.
        match self.peek_terminal() {
            Some(Ok(())) => return Ok(()),
            Some(Err(e)) => return Err(e),
            None => {}
        }

        let guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (final_state, timed_out) =
            match self
                .condvar
                .wait_timeout_while(guard, timeout, |state| !state.is_terminal())
            {
                Ok((guard, wait_result)) => (guard.clone(), wait_result.timed_out()),
                Err(poisoned) => {
                    let (guard, wait_result) = poisoned.into_inner();
                    (guard.clone(), wait_result.timed_out())
                }
            };

        match final_state {
            ReadinessState::Ready => Ok(()),
            ReadinessState::Failed(reason) => Err(MemoryError::NotReady(format!(
                "readiness resolved as failed: {reason}"
            ))),
            ReadinessState::Pending => {
                debug_assert!(
                    timed_out,
                    "wait_timeout_while returned Pending without timing out"
                );
                Err(MemoryError::NotReady(format!(
                    "timed out after {timeout:?} waiting for server readiness"
                )))
            }
        }
    }

    /// Returns `Some(Ok(()))` / `Some(Err(..))` if already terminal, without
    /// touching the condvar; `None` if still `Pending`.
    fn peek_terminal(&self) -> Option<Result<(), MemoryError>> {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match &*state {
            ReadinessState::Ready => Some(Ok(())),
            ReadinessState::Failed(reason) => Some(Err(MemoryError::NotReady(format!(
                "readiness resolved as failed: {reason}"
            )))),
            ReadinessState::Pending => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn already_ready_returns_immediately() {
        let gate = ReadinessGate::new_ready();
        assert!(gate.is_ready());

        let start = Instant::now();
        let result = gate.wait_for_write(Duration::from_secs(5));
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(
            elapsed < Duration::from_millis(200),
            "already-ready wait should return near-instantly, took {elapsed:?}"
        );
    }

    #[test]
    fn already_failed_returns_err_immediately() {
        let gate = ReadinessGate::new_pending();
        gate.resolve_failed("model load exploded".to_string());
        assert!(!gate.is_ready());

        let start = Instant::now();
        let result = gate.wait_for_write(Duration::from_secs(5));
        let elapsed = start.elapsed();

        let err = result.expect_err("expected Err after resolve_failed");
        assert!(err.to_string().contains("model load exploded"));
        assert!(
            elapsed < Duration::from_millis(200),
            "already-failed wait should return near-instantly, took {elapsed:?}"
        );
    }

    /// Spawns a waiter thread and returns `(handle, finished)` where
    /// `finished` flips to `true` only once `wait_for_write` has returned.
    ///
    /// The flag is what makes the two tests below able to fail for the reason
    /// they exist: without it, a gate whose `wait_for_write` returned straight
    /// away without ever blocking would still satisfy the final
    /// `Ok`/`Err` assertion, so the "waits until resolution, then wakes"
    /// contract would go untested.
    fn spawn_waiter(
        gate: &Arc<ReadinessGate>,
    ) -> (thread::JoinHandle<Result<(), MemoryError>>, Arc<AtomicBool>) {
        let finished = Arc::new(AtomicBool::new(false));
        let waiter_gate = Arc::clone(gate);
        let waiter_finished = Arc::clone(&finished);
        let handle = thread::spawn(move || {
            let result = waiter_gate.wait_for_write(Duration::from_secs(5));
            waiter_finished.store(true, Ordering::SeqCst);
            result
        });
        (handle, finished)
    }

    #[test]
    fn waiter_wakes_on_resolve_ready() {
        let gate = Arc::new(ReadinessGate::new_pending());
        assert!(!gate.is_ready());

        let (waiter, finished) = spawn_waiter(&gate);

        thread::sleep(Duration::from_millis(50));
        assert!(
            !finished.load(Ordering::SeqCst),
            "waiter must still be blocked while the gate is Pending"
        );

        gate.resolve_ready();

        let result = waiter.join().expect("waiter thread panicked");
        assert!(result.is_ok());
        assert!(gate.is_ready());
    }

    #[test]
    fn waiter_wakes_on_resolve_failed() {
        let gate = Arc::new(ReadinessGate::new_pending());

        let (waiter, finished) = spawn_waiter(&gate);

        thread::sleep(Duration::from_millis(50));
        assert!(
            !finished.load(Ordering::SeqCst),
            "waiter must still be blocked while the gate is Pending"
        );

        gate.resolve_failed("embedder init failed".to_string());

        let result = waiter.join().expect("waiter thread panicked");
        let err = result.expect_err("expected Err after resolve_failed");
        assert!(err.to_string().contains("embedder init failed"));
        assert!(!gate.is_ready(), "is_ready() must stay false on Failed");
    }

    #[test]
    fn timeout_returns_err_and_is_bounded() {
        let gate = ReadinessGate::new_pending();
        let timeout = Duration::from_millis(50);

        let start = Instant::now();
        let result = gate.wait_for_write(timeout);
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected Err on timeout expiry");
        assert!(
            elapsed >= timeout,
            "wait returned before the timeout elapsed: {elapsed:?} < {timeout:?}"
        );
        // Bounded: proves this isn't a hang or a busy-spin — generous slack
        // for scheduler jitter under CI load.
        assert!(
            elapsed < timeout + Duration::from_secs(2),
            "wait took far longer than the configured timeout: {elapsed:?}"
        );
    }

    #[test]
    fn resolve_ready_is_idempotent_first_wins() {
        let gate = ReadinessGate::new_pending();
        gate.resolve_failed("first".to_string());
        // Second, contradicting resolution must be a no-op: first wins.
        gate.resolve_ready();

        assert!(
            !gate.is_ready(),
            "is_ready() must not flip after Failed won"
        );
        let err = gate
            .wait_for_write(Duration::from_secs(1))
            .expect_err("state should remain Failed");
        assert!(err.to_string().contains("first"));
    }

    #[test]
    fn resolve_failed_is_idempotent_first_wins() {
        let gate = ReadinessGate::new_pending();
        gate.resolve_ready();
        // Second, contradicting resolution must be a no-op: first wins.
        gate.resolve_failed("second".to_string());

        assert!(gate.is_ready(), "is_ready() must not flip after Ready won");
        assert!(gate.wait_for_write(Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn redundant_resolve_calls_do_not_panic() {
        let gate = ReadinessGate::new_pending();
        gate.resolve_ready();
        gate.resolve_ready();
        gate.resolve_ready();
        assert!(gate.is_ready());

        let gate2 = ReadinessGate::new_pending();
        gate2.resolve_failed("x".to_string());
        gate2.resolve_failed("y".to_string());
        assert!(!gate2.is_ready());
    }
}
