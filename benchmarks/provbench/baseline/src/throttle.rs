//! Sliding-window input-token-per-minute throttle for the live LLM run.
//!
//! Anthropic enforces a per-org input-tokens-per-minute (ITPM) cap
//! (450k on `claude-sonnet-4-6` at the time of writing). At sequential
//! `max_concurrency=1` the baseline runner can still burst over this
//! cap when individual batches are small but issued back-to-back, which
//! turns into 429s that exhaust the client's bounded transient-retry
//! budget and abort the run.
//!
//! [`InputTokenMeter`] is a pure synchronous struct: the caller asks
//! "may I send `n` tokens at time `now`?" and gets back either
//! [`ThrottleDecision::Proceed`] or [`ThrottleDecision::SleepFor`] with
//! the duration the caller should `tokio::time::sleep`. After the call
//! the caller can replace the conservative estimate with the actual
//! `usage.input_tokens` via [`InputTokenMeter::correct`].
//!
//! Keeping the meter synchronous (no `tokio::time::pause`) keeps the
//! unit tests trivial: feed [`std::time::Instant`] values directly.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Sliding window length. Anthropic's ITPM cap is per-minute.
const WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleDecision {
    /// Caller may dispatch the batch immediately.
    Proceed,
    /// Caller must sleep at least this long before dispatching the batch.
    SleepFor(Duration),
}

/// Sliding 60-second window of `(observed_at, input_tokens)` records.
///
/// Cap of `0` disables throttling entirely (the meter never sleeps and
/// recording is a no-op) so the CLI can keep the throttle off by
/// passing `--max-input-tokens-per-minute 0`.
#[derive(Debug)]
pub struct InputTokenMeter {
    cap_per_minute: usize,
    entries: VecDeque<(Instant, usize)>,
    /// Sum of `entries` token counts. Maintained incrementally so
    /// `would_sleep` is O(window-evict).
    current_sum: usize,
}

impl InputTokenMeter {
    pub fn new(cap_per_minute: usize) -> Self {
        Self {
            cap_per_minute,
            entries: VecDeque::new(),
            current_sum: 0,
        }
    }

    /// Disabled meters cap == 0 — convenience predicate so the runner
    /// can skip the whole code path cleanly.
    pub fn is_disabled(&self) -> bool {
        self.cap_per_minute == 0
    }

    /// Drop entries older than `now - WINDOW` from the front of the
    /// queue. Called by every public method that depends on a fresh
    /// `current_sum`.
    fn evict_expired(&mut self, now: Instant) {
        let cutoff = now.checked_sub(WINDOW);
        while let Some(&(ts, n)) = self.entries.front() {
            let expired = match cutoff {
                Some(c) => ts < c,
                // `Instant` is monotonic in practice; on the rare host
                // where `now` is earlier than `WINDOW` past the epoch
                // we just keep everything in-window.
                None => false,
            };
            if expired {
                self.entries.pop_front();
                self.current_sum = self.current_sum.saturating_sub(n);
            } else {
                break;
            }
        }
    }

    /// Decide whether to proceed immediately or sleep first, given the
    /// estimated input-token cost of the upcoming batch.
    ///
    /// Does NOT record the estimate — call [`Self::record_estimate`]
    /// after the sleep (if any) and right before dispatch.
    pub fn would_sleep(&mut self, now: Instant, est_tokens: usize) -> ThrottleDecision {
        if self.is_disabled() {
            return ThrottleDecision::Proceed;
        }
        self.evict_expired(now);
        let projected = self.current_sum.saturating_add(est_tokens);
        if projected <= self.cap_per_minute {
            return ThrottleDecision::Proceed;
        }
        // Need to free `projected - cap` tokens by aging out the
        // oldest entries. Compute the earliest in-window timestamp
        // that, once expired, would bring the projected sum below the
        // cap.
        let need_to_free = projected - self.cap_per_minute;
        let mut freed = 0usize;
        for &(ts, n) in self.entries.iter() {
            freed = freed.saturating_add(n);
            if freed >= need_to_free {
                // Sleep until this entry ages out (one tick past WINDOW).
                let wakeup = ts + WINDOW + Duration::from_millis(1);
                let dur = wakeup.saturating_duration_since(now);
                return ThrottleDecision::SleepFor(dur);
            }
        }
        // The window can't free enough even after fully draining — the
        // estimate alone exceeds the cap. Sleep the full window so the
        // queue empties and the caller can try again.
        ThrottleDecision::SleepFor(WINDOW)
    }

    /// Record a pre-dispatch token estimate. Call after any throttle
    /// sleep, immediately before issuing the API call.
    pub fn record_estimate(&mut self, now: Instant, est_tokens: usize) {
        if self.is_disabled() || est_tokens == 0 {
            return;
        }
        self.entries.push_back((now, est_tokens));
        self.current_sum = self.current_sum.saturating_add(est_tokens);
    }

    /// Replace the last estimate with the post-call actual token count
    /// (from `usage.input_tokens`). No-op if the meter is disabled or
    /// there is no estimate to correct.
    pub fn correct(&mut self, actual_tokens: usize) {
        if self.is_disabled() {
            return;
        }
        if let Some(last) = self.entries.back_mut() {
            self.current_sum = self.current_sum.saturating_sub(last.1);
            last.1 = actual_tokens;
            self.current_sum = self.current_sum.saturating_add(actual_tokens);
        }
    }

    /// Current in-window sum — exposed for diagnostics/tests.
    pub fn current_sum(&self) -> usize {
        self.current_sum
    }
}

/// Cheap input-token estimator. Anthropic tokenizers run ~3.5–4.0
/// chars/token; using 4.0 gives a conservative under-estimate, but
/// after the API call we replace it with the actual `usage.input_tokens`
/// so the meter converges fast.
pub fn estimate_input_tokens_from_chars(total_chars: usize) -> usize {
    // ceil-div by 4.
    total_chars.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Instant {
        // Build a base instant once and offset from it for tests.
        // `Instant::now()` is fine here because the meter only cares
        // about deltas.
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let base = *BASE.get_or_init(Instant::now);
        base + Duration::from_secs(secs)
    }

    #[test]
    fn empty_meter_never_sleeps() {
        let mut m = InputTokenMeter::new(300_000);
        assert_eq!(m.would_sleep(t(0), 0), ThrottleDecision::Proceed);
        assert_eq!(m.would_sleep(t(0), 1_000), ThrottleDecision::Proceed);
        assert_eq!(m.would_sleep(t(0), 300_000), ThrottleDecision::Proceed);
    }

    #[test]
    fn under_cap_never_sleeps() {
        let mut m = InputTokenMeter::new(300_000);
        m.record_estimate(t(0), 100_000);
        m.record_estimate(t(1), 100_000);
        assert_eq!(m.would_sleep(t(2), 50_000), ThrottleDecision::Proceed);
        assert_eq!(m.current_sum(), 200_000);
    }

    #[test]
    fn at_exact_cap_proceeds() {
        let mut m = InputTokenMeter::new(300_000);
        m.record_estimate(t(0), 200_000);
        assert_eq!(m.would_sleep(t(1), 100_000), ThrottleDecision::Proceed);
    }

    #[test]
    fn over_cap_sleeps_until_oldest_ages_out() {
        let mut m = InputTokenMeter::new(300_000);
        m.record_estimate(t(0), 200_000);
        m.record_estimate(t(10), 50_000);
        // Now at 250k; asking for 100k → 350k > 300k.
        // Need to free 50k. Oldest is 200k at t(0), so freeing it
        // releases 200k (≥50k needed) and wakeup = t(0) + 60s + 1ms.
        let decision = m.would_sleep(t(20), 100_000);
        match decision {
            ThrottleDecision::SleepFor(d) => {
                // Wakeup is at t(60.001), now is at t(20), so ~40.001s.
                let expected = Duration::from_secs(40) + Duration::from_millis(1);
                let delta = d.as_millis().abs_diff(expected.as_millis());
                assert!(delta < 5, "expected ~{expected:?}, got {d:?}");
            }
            other => panic!("expected SleepFor, got {other:?}"),
        }
    }

    #[test]
    fn expired_tokens_no_longer_count() {
        let mut m = InputTokenMeter::new(300_000);
        m.record_estimate(t(0), 200_000);
        m.record_estimate(t(5), 100_000);
        // At t=65, the first entry (t=0) is past the 60s window.
        // Window sum should drop to 100k after eviction.
        assert_eq!(m.would_sleep(t(65), 100_000), ThrottleDecision::Proceed);
        assert_eq!(m.current_sum(), 100_000);
    }

    #[test]
    fn correct_replaces_last_estimate() {
        let mut m = InputTokenMeter::new(300_000);
        m.record_estimate(t(0), 100_000);
        m.correct(50_000);
        assert_eq!(m.current_sum(), 50_000);
        // After correction we should still be well under cap.
        assert_eq!(m.would_sleep(t(1), 200_000), ThrottleDecision::Proceed);
    }

    #[test]
    fn correct_is_noop_on_empty_meter() {
        let mut m = InputTokenMeter::new(300_000);
        m.correct(50_000);
        assert_eq!(m.current_sum(), 0);
    }

    #[test]
    fn estimate_alone_over_cap_sleeps_full_window() {
        let mut m = InputTokenMeter::new(100_000);
        let d = m.would_sleep(t(0), 500_000);
        assert_eq!(d, ThrottleDecision::SleepFor(WINDOW));
    }

    #[test]
    fn disabled_meter_never_sleeps_or_records() {
        let mut m = InputTokenMeter::new(0);
        assert!(m.is_disabled());
        m.record_estimate(t(0), 1_000_000);
        assert_eq!(m.current_sum(), 0);
        assert_eq!(
            m.would_sleep(t(0), 1_000_000_000),
            ThrottleDecision::Proceed
        );
    }

    #[test]
    fn estimate_input_tokens_ceil_div() {
        assert_eq!(estimate_input_tokens_from_chars(0), 0);
        assert_eq!(estimate_input_tokens_from_chars(1), 1);
        assert_eq!(estimate_input_tokens_from_chars(4), 1);
        assert_eq!(estimate_input_tokens_from_chars(5), 2);
        assert_eq!(estimate_input_tokens_from_chars(12_000), 3_000);
    }
}
