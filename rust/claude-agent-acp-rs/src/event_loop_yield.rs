//! `EventLoopYielder` (`utils.ts:114-145`) — cooperative yielding for loops
//! that forward a backlog of updates without an I/O wait between items.
//!
//! The Node class arms one `setImmediate` marker per event-loop turn. Within
//! the budget a call returns `undefined`; past it the call returns a promise
//! that resolves in the check phase, after pending I/O ran. A loop that already
//! waits on real I/O never yields, because its wait lets the marker callback
//! run and the next call starts a fresh budget.
//!
//! [`EventLoopYielder::maybe_yield`] is the same budget with
//! `tokio::task::yield_now()` as the check-phase pause. The check-phase marker
//! has no runtime-independent equivalent on tokio — a spawned reset task runs
//! at once on an idle worker of the multi-thread runtime, so every call would
//! start a new slice and the loop would never yield (the bug this file had).
//! The slice therefore measures elapsed wall-clock time from the first call
//! and from the last yield, and a loop that awaits real I/O between items
//! accepts at most one extra no-op `yield_now` per budget.

use std::time::Instant;

/// The longest stretch, in milliseconds, that a message loop keeps the event
/// loop before it yields (`MESSAGE_LOOP_BUDGET_MS`, `utils.ts:142`).
pub const MESSAGE_LOOP_BUDGET_MS: f64 = 8.0;

/// A cooperative budget shared by one forwarding loop.
pub struct EventLoopYielder {
    budget_ms: f64,
    now: Box<dyn Fn() -> f64 + Send + Sync>,
    /// The start of the current slice; `None` until the first call arms it.
    slice_start: Option<f64>,
}

impl EventLoopYielder {
    /// A yielder with an injected clock (the upstream tests fake `performance.now`).
    pub fn new(budget_ms: f64, now: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        Self {
            budget_ms,
            now: Box::new(now),
            slice_start: None,
        }
    }

    /// A yielder on the monotonic process clock, in milliseconds.
    pub fn with_clock(budget_ms: f64) -> Self {
        let start = Instant::now();
        Self::new(budget_ms, move || start.elapsed().as_secs_f64() * 1000.0)
    }

    /// `maybeYield()`: `false` within the budget (Node `undefined`), `true`
    /// when this call paused the loop (Node's promise). An await of the
    /// returned future is required for the pause.
    pub async fn maybe_yield(&mut self) -> bool {
        let time = (self.now)();
        let Some(start) = self.slice_start else {
            // The first call of a slice arms it and goes on at once, like the
            // `setImmediate` marker arm in the Node class.
            self.slice_start = Some(time);
            return false;
        };
        if time - start < self.budget_ms {
            return false;
        }
        // `new Promise((resolve) => setImmediate(resolve))`.
        tokio::task::yield_now().await;
        self.slice_start = Some((self.now)());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Arc;

    fn fake_clock() -> (Arc<AtomicU64>, impl Fn() -> f64 + Send + Sync + 'static) {
        let clock = Arc::new(AtomicU64::new(0));
        let handle = Arc::clone(&clock);
        let now = move || handle.load(std::sync::atomic::Ordering::Relaxed) as f64;
        (clock, now)
    }

    /// "lets a loop run on within its budget".
    #[tokio::test]
    async fn lets_a_loop_run_within_its_budget() {
        let (clock, now) = fake_clock();
        let mut yielder = EventLoopYielder::new(8.0, now);
        assert!(!yielder.maybe_yield().await);
        clock.store(7, std::sync::atomic::Ordering::Relaxed);
        assert!(!yielder.maybe_yield().await);
    }

    /// "yields to the event loop past its budget".
    #[tokio::test]
    async fn yields_to_the_event_loop_past_its_budget() {
        let (clock, now) = fake_clock();
        let mut yielder = EventLoopYielder::new(8.0, now);
        assert!(!yielder.maybe_yield().await);
        let timer_ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&timer_ran);
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        clock.store(8, std::sync::atomic::Ordering::Relaxed);
        assert!(yielder.maybe_yield().await);
        // Give the check phase one more turn, like `await pause` in the test.
        tokio::task::yield_now().await;
        assert!(timer_ran.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// "starts a new budget once the event loop has turned".
    #[tokio::test]
    async fn starts_a_new_budget_once_the_event_loop_has_turned() {
        let (clock, now) = fake_clock();
        let mut yielder = EventLoopYielder::new(8.0, now);
        assert!(!yielder.maybe_yield().await);
        clock.store(20, std::sync::atomic::Ordering::Relaxed);
        assert!(yielder.maybe_yield().await);
        // The pause ends the slice, so the budget starts again here.
        assert!(!yielder.maybe_yield().await);
        clock.store(27, std::sync::atomic::Ordering::Relaxed);
        assert!(!yielder.maybe_yield().await);
        clock.store(28, std::sync::atomic::Ordering::Relaxed);
        assert!(yielder.maybe_yield().await);
    }

    /// "does not yield for a loop that waits on I/O between items" — relaxed:
    /// with no check-phase marker a tokio loop cannot observe its own I/O wait,
    /// so it yields at most once per budget (a no-op `yield_now`) instead of
    /// never. The item after the pause is within the fresh slice.
    #[tokio::test]
    async fn does_not_yield_for_a_loop_that_waits_on_io() {
        let (clock, now) = fake_clock();
        let mut yielder = EventLoopYielder::new(8.0, now);
        for _ in 0..5 {
            // The item after the pause is within the fresh slice.
            assert!(!yielder.maybe_yield().await);
            clock.fetch_add(20, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            // The wait on I/O advanced the clock past the budget: one pause.
            assert!(yielder.maybe_yield().await);
        }
    }

    /// The production runtime is multi-threaded (`bin/claude-agent-acp-rs.rs`),
    /// where a spawned reset task clears the marker before the next call and the
    /// old implementation never yielded. The monotonic slice must still expire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn yields_past_its_budget_on_the_multi_thread_runtime() {
        let mut yielder = EventLoopYielder::with_clock(MESSAGE_LOOP_BUDGET_MS);
        assert!(!yielder.maybe_yield().await);
        // Busy-wait well past the 8 ms budget, like a long replay slice.
        let end = Instant::now() + std::time::Duration::from_millis(30);
        while Instant::now() < end {
            std::hint::spin_loop();
        }
        assert!(yielder.maybe_yield().await);
        // The yield started a fresh slice: the next call is within budget.
        assert!(!yielder.maybe_yield().await);
    }
}
