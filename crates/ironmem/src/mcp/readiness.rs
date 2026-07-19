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
//! Read-shaped tools branch on [`ReadinessGate::snapshot`], which
//! distinguishes `Pending` (retry shortly) from `Failed` (this server is not
//! coming up). The lock-free [`ReadinessGate::is_ready`] remains the "may I
//! touch the embedder" check, but it collapses those two and so must not be
//! what a client is told. Write-shaped tools — the set in
//! `tools::WRITE_SHAPED_TOOLS` — block via [`ReadinessGate::wait_for_write`].
//!
//! The gate exposes two waits over the same terminal state.
//! [`ReadinessGate::wait_for_write_async`] is the PRODUCTION path:
//! `server::dispatch_request` awaits it before dispatch on both transports, so
//! a waiter costs a `Notify` registration rather than an OS thread and never
//! occupies the single thread that owns the `App`.
//! [`ReadinessGate::wait_for_write`] is the synchronous fallback, reached via
//! `tools::call_tool` for callers that enter `dispatch`/`call_tool` directly.
//! No tool handler calls either one — see `tools::WRITE_SHAPED_TOOLS`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::error::MemoryError;

/// Client-facing reason recorded when server startup fails.
///
/// A `Failed` reason is NOT an internal-only breadcrumb: [`ReadinessGate::wait_for_write`]
/// embeds it in `MemoryError::NotReady`, and the MCP server forwards a
/// `NotReady` message to the client verbatim. So a reason built from the
/// underlying error would publish internal paths and OS/driver error text to
/// every MCP client. Startup call sites log the full error via `tracing` and
/// hand the gate this constant instead.
pub const STARTUP_FAILURE_CLIENT_REASON: &str =
    "server memory initialization failed at startup; writes are unavailable until the \
     server is restarted (see server logs for details)";

/// Largest timeout this gate will honor, and the ceiling every caller-supplied
/// `Duration` is clamped to.
///
/// A day is already indistinguishable from "wait forever" for a warm-up guard,
/// and clamping keeps `Instant::now() + timeout` representable — that addition
/// panics on overflow, and in a shared daemon one such panic takes down every
/// connected client, not just the caller.
const MAX_REPRESENTABLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Terminal-resolution state guarded by the gate's `Mutex`.
///
/// `Pending` is the only non-terminal state. Once the state transitions to
/// `Ready` or `Failed`, it never changes again (first resolution wins — see
/// [`ReadinessGate::resolve_ready`] / [`ReadinessGate::resolve_failed`]).
///
/// Read-shaped callers observe this through [`ReadinessGate::snapshot`] rather
/// than the boolean [`ReadinessGate::is_ready`], because `Pending` and
/// `Failed` are both "not ready" but mean opposite things to a client:
/// `Pending` means poll again, `Failed` means this server will never come up.
/// Collapsing them into one bool is what let a dead server report itself as a
/// slow one indefinitely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessState {
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
    /// Wakes blocking waiters (`wait_for_write`).
    condvar: Condvar,
    /// Wakes async waiters (`wait_for_write_async`). Kept alongside the
    /// condvar rather than replacing it: both waiter kinds read the same
    /// `state`, and every resolution signals both.
    notify: Notify,
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
            notify: Notify::new(),
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
            notify: Notify::new(),
        }
    }

    /// Lock-free readiness check. Preserves today's `is_warming_up()`
    /// semantics (`!memory_ready.load(Relaxed)`): a plain `Relaxed` load of
    /// the underlying `AtomicBool`, no stronger ordering introduced.
    ///
    /// Returns `false` for BOTH `Pending` and `Failed`. Callers that report
    /// state to a client must use [`ReadinessGate::snapshot`] instead — see
    /// [`ReadinessState`].
    pub fn is_ready(&self) -> bool {
        self.ready_flag.load(Ordering::Relaxed)
    }

    /// Full tri-state snapshot, for callers that have to tell a client the
    /// difference between "still warming up" and "startup failed".
    ///
    /// Takes the mutex (unlike [`ReadinessGate::is_ready`]) because the
    /// failure reason lives behind it. That is fine for the read-shaped tool
    /// handlers that call this — they hold it only long enough to clone —
    /// but it is not the lock-free fast path.
    pub fn snapshot(&self) -> ReadinessState {
        match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
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
        self.notify.notify_waiters();
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
        self.notify.notify_waiters();
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
        // Clamped for the same reason as the async variant: `pub`, takes an
        // arbitrary `Duration`, and the platform condvar computes a deadline
        // internally from it.
        let timeout = timeout.min(MAX_REPRESENTABLE_TIMEOUT);
        // Fast path: read the terminal state under a brief lock and return.
        // The mutex IS taken (the reason lives behind it); what this avoids is
        // ever parking on the condvar.
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

    /// Async counterpart to [`ReadinessGate::wait_for_write`], with identical
    /// return semantics: `Ok(())` on `Ready`, `Err(NotReady)` on `Failed` or
    /// on `timeout` expiry.
    ///
    /// Callers that are already inside an async context must use this rather
    /// than wrapping `wait_for_write` in `spawn_blocking`. The blocking pool
    /// is bounded (512 threads by default) and shared with every other
    /// `spawn_blocking` user in the process, so one thread per parked waiter
    /// lets a warm-up window with many concurrent writes starve unrelated
    /// work for the entire readiness timeout. Here a waiter costs a `Notify`
    /// registration instead.
    pub async fn wait_for_write_async(&self, timeout: Duration) -> Result<(), MemoryError> {
        // `Instant + Duration` PANICS on overflow, and `timeout` ultimately
        // comes from an operator-supplied env var. `Config` clamps that to a
        // day, but this method is `pub` and takes an arbitrary `Duration`, so
        // it defends itself rather than trusting a bound enforced in another
        // module.
        //
        // Clamped, NOT degraded to an unbounded wait: this method's contract is
        // a bounded fail-safe, and a caller that waits forever would occupy an
        // in-flight slot and never answer its client — the hang the timeout
        // exists to prevent.
        let timeout = timeout.min(MAX_REPRESENTABLE_TIMEOUT);
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(|| Instant::now() + MAX_REPRESENTABLE_TIMEOUT);
        loop {
            // Register with `Notify` BEFORE reading the state. `notify_waiters`
            // only wakes already-registered waiters, and resolution publishes
            // the terminal state before notifying — so registering first is
            // what closes the window where a resolution lands between the
            // state read and the wait, which would otherwise park this waiter
            // until its timeout despite the gate being resolved.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(result) = self.peek_terminal() {
                return result;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
                return Err(MemoryError::NotReady(format!(
                    "timed out after {timeout:?} waiting for server readiness"
                )));
            }
            // Woken: loop back to re-read the state. `notify_waiters` carries
            // no payload, so the state — not the wakeup — is the authority.
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

    #[tokio::test]
    async fn async_waiter_returns_immediately_when_already_terminal() {
        let ready = ReadinessGate::new_ready();
        assert!(ready
            .wait_for_write_async(Duration::from_secs(5))
            .await
            .is_ok());

        let failed = ReadinessGate::new_pending();
        failed.resolve_failed("model load exploded".to_string());
        let err = failed
            .wait_for_write_async(Duration::from_secs(5))
            .await
            .expect_err("expected Err after resolve_failed");
        assert!(err.to_string().contains("model load exploded"));
    }

    #[tokio::test]
    async fn async_waiter_stays_parked_until_resolution() {
        let gate = Arc::new(ReadinessGate::new_pending());

        let waiter_gate = Arc::clone(&gate);
        let mut waiter = Box::pin(async move {
            waiter_gate
                .wait_for_write_async(Duration::from_secs(30))
                .await
        });

        // Drive the waiter to its registration point; it must not resolve.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "waiter must stay parked while the gate is Pending"
        );

        gate.resolve_ready();
        let result = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter must wake once the gate resolves");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn async_waiter_times_out_while_pending() {
        let gate = ReadinessGate::new_pending();
        let timeout = Duration::from_millis(50);

        let start = Instant::now();
        let result = gate.wait_for_write_async(timeout).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected Err on timeout expiry");
        assert!(
            elapsed >= timeout,
            "wait returned before the timeout elapsed: {elapsed:?} < {timeout:?}"
        );
    }

    /// Many concurrent async waiters must all wake from a single resolution.
    ///
    /// Scope note: running on a `current_thread` runtime proves waiting is not
    /// INLINE blocking (that would deadlock here). It does NOT prove the
    /// absence of a per-waiter blocking-pool thread — a
    /// `spawn_blocking(wait_for_write)` implementation would pass this too, at
    /// 256 waiters against a 512-thread default pool. That property is pinned
    /// by `many_readiness_waiters_do_not_starve_the_blocking_pool` in
    /// `mcp::server`, which caps the pool at one thread.
    #[test]
    fn many_async_waiters_all_wake_on_a_single_resolution() {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let gate = Arc::new(ReadinessGate::new_pending());
            let mut waiters: FuturesUnordered<_> = (0..256)
                .map(|_| gate.wait_for_write_async(Duration::from_secs(30)))
                .collect();

            // Drive every waiter to its registration point.
            assert!(
                tokio::time::timeout(Duration::from_millis(100), waiters.next())
                    .await
                    .is_err(),
                "no waiter may resolve while the gate is Pending"
            );

            gate.resolve_ready();

            let mut woken = 0;
            while let Some(result) = tokio::time::timeout(Duration::from_secs(5), waiters.next())
                .await
                .expect("all waiters must wake from the single resolution")
            {
                assert!(result.is_ok(), "resolution was Ready: {result:?}");
                woken += 1;
            }
            assert_eq!(woken, 256);
        });
    }

    /// `Instant::now() + timeout` panics on overflow, and in a shared daemon
    /// that panic takes down every connected client, not just the caller. Both
    /// waits are `pub` and take an arbitrary `Duration`, so neither may rely on
    /// `Config` having clamped first — this passes the worst possible value
    /// straight in, against a PENDING gate so the deadline is really built.
    ///
    /// Asymmetry worth knowing: the async path is where the panic actually
    /// lives, and removing its guard makes this test fail (verified by
    /// mutation). The synchronous path's clamp is defense-in-depth — this
    /// platform's `Condvar::wait_timeout_while` saturates rather than
    /// panicking, so removing it changes nothing observable here. It stays for
    /// the platforms and future callers that offer no such guarantee.
    #[test]
    fn an_absurd_timeout_is_clamped_rather_than_panicking() {
        // A PENDING gate, so the call reaches the condvar and actually builds
        // a deadline from the absurd duration — an already-ready gate returns
        // via the terminal fast path and never does the arithmetic at all.
        let gate = Arc::new(ReadinessGate::new_pending());
        let resolver = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            resolver.resolve_ready();
        });
        assert!(gate.wait_for_write(Duration::MAX).is_ok());
        handle.join().expect("resolver thread");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        runtime.block_on(async {
            let gate = ReadinessGate::new_pending();
            gate.resolve_failed("startup died".to_string());
            // Reaches the deadline arithmetic before the terminal peek returns.
            assert!(gate.wait_for_write_async(Duration::MAX).await.is_err());
        });
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
        // First-wins applies to the REASON too: a second resolution that
        // overwrote it would otherwise pass this test unnoticed.
        assert_eq!(
            gate2.snapshot(),
            ReadinessState::Failed("x".to_string()),
            "the first failure reason must survive a second resolve_failed"
        );
    }
}
