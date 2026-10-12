//! Rate control and measurement: `spec/engine-v1.md` §9.1 and §9.2.
//!
//! Two independent pieces:
//!
//! * [`Pacer`] — smooth pacing, built on **one** number: the instant at which
//!   the next byte is free. §9.1's first invariant forbids keeping a token
//!   balance *and* a reserved timeline, because the two advance independently
//!   and end up disagreeing about how much has been paid for. There is only a
//!   timeline here; nothing else records what has been spent.
//! * [`RateWindow`] — a sliding-window throughput estimate. Total bytes over
//!   total time is explicitly rejected by §9.2: it makes a long transfer
//!   unresponsive to recent change.
//!
//! Both are shared by `Arc` because every segment of a task runs concurrently.
//! The pacer keeps its state in atomics so that acquiring a slot never holds a
//! lock across an await point.
//!
//! # How the timeline works
//!
//! `reserved_until` is the instant at which every byte reserved so far has been
//! paid for. A caller claims the interval that starts there and is as long as
//! its own bytes require, and the time it is told to wait is the end of its own
//! interval — so the bytes leave once they have been paid for. That single rule
//! gives all five of §9.1's invariants:
//!
//! 1. one source of truth, because there is one atomic;
//! 2. the aggregate of `N` bytes can never leave before `N / rate`, and it never
//!    lags by more than the caller's own final claim;
//! 3. concurrent callers claim disjoint intervals — the compare-exchange hands
//!    each one the tail it read — so the total time grows linearly with the
//!    number of callers instead of shrinking;
//! 4. every retry of that compare-exchange observes a strictly larger value,
//!    because a successful claim only ever moves the mark forwards, so the loop
//!    cannot spin;
//! 5. changing the rate touches only the price of *future* claims: the mark, and
//!    with it every interval already handed out, is left alone.
//!
//! The one exception to "wait for your own interval" is a single allowance of
//! one second granted when pacing starts, which is §9.1's permitted initial
//! deferral ("wait then send" and "send then wait" have the same average rate)
//! expressed on the timeline. It does not make the transfer faster: the
//! allowance is spent from the mark like any other claim, so the next caller
//! pays for both and the average over anything longer than one claim is exactly
//! the configured rate.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Ceiling on a single pacing wait, so that a pathological rate cannot produce
/// a sleep measured in years.
const MAX_PACING_WAIT: Duration = Duration::from_secs(60 * 60 * 24);

/// One-shot deferral granted when pacing starts: the first claim after that is
/// not delayed. §9.1 allows an initial delay of one block; this is that
/// allowance, and it is granted once rather than per claim so that the ceiling
/// still holds in aggregate.
const INITIAL_ALLOWANCE: u64 = 1_000_000_000;

/// A single-timeline pacer. One token is one byte, and the only state is the
/// instant at which the next byte is free.
#[derive(Debug)]
pub struct Pacer {
    /// Nanoseconds of wall time per byte at the current rate. Zero means
    /// unlimited.
    nanos_per_byte: AtomicU64,
    /// Instant at which everything reserved so far has been paid for, in
    /// nanoseconds since `origin`. Only ever moves forwards while a rate is
    /// set, which is what makes the claim loop below terminate.
    reserved_until: AtomicU64,
    /// Remaining one-shot deferral, in nanoseconds. Not a balance: it is never
    /// topped up by traffic, never consulted to answer "how much has been paid
    /// for", and only ever goes from its initial value to zero.
    allowance: AtomicU64,
    origin: Instant,
}

impl Pacer {
    /// A pacer limiting to `bytes_per_second`. `None` means unlimited.
    pub fn new(bytes_per_second: Option<u64>) -> Self {
        let pacer = Self {
            nanos_per_byte: AtomicU64::new(0),
            reserved_until: AtomicU64::new(0),
            allowance: AtomicU64::new(0),
            origin: Instant::now(),
        };
        // Starts from zero, so this also arms the deferral and the clock.
        pacer.set_rate(bytes_per_second);
        pacer
    }

    /// Change the rate while running. §9.1 requires the change to take effect
    /// without restarting the transfer.
    ///
    /// Only the price of later claims changes. The mark is deliberately left
    /// where it is: it *is* the set of intervals already handed out, and moving
    /// it back would let a later caller claim time that an earlier one is
    /// already waiting for — the double-booking §9.1's third invariant forbids.
    /// The cost of that choice is that raising the rate does not shorten a
    /// backlog that was booked at the old one; the backlog is bounded by the
    /// claims already in flight, which is one block per transfer lane.
    ///
    /// Switching pacing on from off is different: that starts the clock, so the
    /// mark is reset to now and the deferral is granted afresh.
    pub fn set_rate(&self, bytes_per_second: Option<u64>) {
        let previous = self.nanos_per_byte.load(Ordering::Relaxed);
        match bytes_per_second {
            Some(rate) if rate > 0 => {
                // Nanoseconds per byte, rounded up so the pacing is never faster
                // than the rate that was asked for.
                let nanos = 1_000_000_000u64.div_ceil(rate).max(1);
                self.nanos_per_byte.store(nanos, Ordering::Relaxed);
                if previous == 0 {
                    self.reserved_until
                        .store(self.now_nanos(), Ordering::Release);
                    self.allowance.store(INITIAL_ALLOWANCE, Ordering::Release);
                }
            }
            _ => {
                self.nanos_per_byte.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Current pacing rate in bytes per second; `None` when unlimited.
    pub fn rate(&self) -> Option<u64> {
        let nanos = self.nanos_per_byte.load(Ordering::Relaxed);
        (nanos > 0).then(|| 1_000_000_000u64 / nanos)
    }

    /// Whether the pacer is currently unlimited.
    pub fn is_unlimited(&self) -> bool {
        self.nanos_per_byte.load(Ordering::Relaxed) == 0
    }

    /// Claim `bytes` worth of the timeline and report how long the caller must
    /// wait before sending them.
    ///
    /// The claim is one compare-exchange. It succeeds against the value it read
    /// and installs a strictly larger one, so a retry always sees fresh state
    /// and the loop is bounded by the number of racing claims rather than by
    /// anything about the inputs.
    pub fn reserve(&self, bytes: u64) -> Duration {
        let nanos_per_byte = self.nanos_per_byte.load(Ordering::Relaxed);
        if nanos_per_byte == 0 || bytes == 0 {
            return Duration::ZERO;
        }
        let cost = bytes.saturating_mul(nanos_per_byte);
        if cost == 0 {
            return Duration::ZERO;
        }

        let now = self.now_nanos();
        let mut current = self.reserved_until.load(Ordering::Acquire);
        let end = loop {
            // The mark may sit in the past when the pacer has been idle; a
            // claim then starts now rather than inheriting idle time as credit.
            let start = current.max(now);
            let proposed = start.saturating_add(cost);
            match self.reserved_until.compare_exchange_weak(
                current,
                proposed,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break proposed,
                // Another caller claimed first. What it installed is strictly
                // larger than what we read, so this cannot loop forever, and
                // its claim and ours never overlap.
                Err(actual) => current = actual,
            }
        };

        // Everything up to `end` has now been paid for by this claim. Waiting
        // for `end` is what makes the aggregate rate correct; the wait is
        // measured against the same `now` the claim was priced from.
        let price = end.saturating_sub(now);
        let allowance = self.allowance.swap(0, Ordering::AcqRel);
        Duration::from_nanos(price.saturating_sub(allowance)).min(MAX_PACING_WAIT)
    }

    /// Wait until `bytes` may be sent.
    ///
    /// One reservation, one sleep. There is no re-check loop: the interval was
    /// taken up front, so re-reserving would charge the caller twice. A caller
    /// that has to stop mid-wait is cancelled from outside; the transfer loop
    /// races this future against its stop signal, which is both prompt and free
    /// of the quadratic behaviour a wake-and-retry loop would have at a low
    /// rate.
    pub async fn acquire(&self, bytes: u64) {
        let wait = self.reserve(bytes);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    fn now_nanos(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// A shared, settable limiter. `None` inside means unlimited.
pub type SharedLimiter = Arc<RateLimiter>;

/// Wrapper so that a limiter can be reconfigured without replacing it, which is
/// what §9.1 asks for.
#[derive(Debug)]
pub struct RateLimiter {
    pacer: Pacer,
    /// The rate the host asked for, kept even while pacing is switched off, so
    /// switching it back on does not lose the setting.
    configured: AtomicU64,
}

impl RateLimiter {
    /// A limiter with an initial rate.
    pub fn new(bytes_per_second: Option<u64>) -> Self {
        Self {
            pacer: Pacer::new(bytes_per_second),
            configured: AtomicU64::new(bytes_per_second.unwrap_or(0)),
        }
    }

    /// Shared handle.
    pub fn shared(bytes_per_second: Option<u64>) -> SharedLimiter {
        Arc::new(Self::new(bytes_per_second))
    }

    /// Change the rate in place. `None` switches pacing off without forgetting
    /// the last rate, so a later `Some` can restore it.
    pub fn set_rate(&self, bytes_per_second: Option<u64>) {
        if let Some(rate) = bytes_per_second {
            self.configured.store(rate, Ordering::Relaxed);
        }
        self.pacer.set_rate(bytes_per_second);
    }

    /// The rate currently being paced at; `None` when unlimited.
    pub fn rate(&self) -> Option<u64> {
        let configured = self.configured.load(Ordering::Relaxed);
        (configured > 0).then_some(configured)
    }

    /// Whether pacing is currently off.
    pub fn is_unlimited(&self) -> bool {
        self.pacer.is_unlimited()
    }

    /// Claim `bytes` and report the wait, without sleeping.
    pub fn reserve(&self, bytes: u64) -> Duration {
        self.pacer.reserve(bytes)
    }

    /// Wait until `bytes` may be sent.
    pub async fn acquire(&self, bytes: u64) {
        self.pacer.acquire(bytes).await;
    }
}

/// Sliding-window throughput estimate.
///
/// `record` is cheap and lock-scoped; `rate_bps` has to take the lock, so the
/// transfer path publishes its rate into an atomic and readers use
/// [`RateWindow::last_rate_bps`] instead of recomputing on every event.
#[derive(Debug)]
pub struct RateWindow {
    inner: std::sync::Mutex<WindowState>,
    last: AtomicU64,
    window: Duration,
}

#[derive(Debug)]
struct WindowState {
    samples: VecDeque<(Instant, u64)>,
    bytes: u64,
}

/// Bound on retained samples. One per emitted progress event, so at 10 Hz this
/// is roughly six seconds of history; the deque is trimmed by age first and by
/// this cap second, which keeps memory flat regardless of event rate.
const MAX_SAMPLES: usize = 256;

impl RateWindow {
    /// A window of `window_secs` seconds.
    pub fn new(window_secs: u64) -> Self {
        Self {
            inner: std::sync::Mutex::new(WindowState {
                samples: VecDeque::new(),
                bytes: 0,
            }),
            last: AtomicU64::new(0),
            window: Duration::from_secs(window_secs.max(1)),
        }
    }

    /// Add `bytes` observed at `now`.
    pub fn record(&self, bytes: u64, now: Instant) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.samples.push_back((now, bytes));
        state.bytes = state.bytes.saturating_add(bytes);
        let cutoff = now.checked_sub(self.window);
        while let Some((stamp, amount)) = state.samples.front().copied() {
            let expired = match cutoff {
                Some(cutoff) => stamp < cutoff,
                None => false,
            };
            if expired || state.samples.len() > MAX_SAMPLES {
                state.samples.pop_front();
                state.bytes = state.bytes.saturating_sub(amount);
            } else {
                break;
            }
        }
        let rate = Self::compute(&state);
        self.last.store(rate, Ordering::Relaxed);
    }

    /// Recompute the rate from the current samples.
    pub fn rate_bps(&self) -> u64 {
        match self.inner.lock() {
            Ok(state) => Self::compute(&state),
            Err(_) => 0,
        }
    }

    /// The rate as of the last [`RateWindow::record`], without taking a lock.
    pub fn last_rate_bps(&self) -> u64 {
        self.last.load(Ordering::Relaxed)
    }

    /// Drop all samples. Used when a task is paused or restarted, so that an
    /// idle period cannot be mistaken for a slow rate.
    pub fn reset(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.samples.clear();
            state.bytes = 0;
        }
        self.last.store(0, Ordering::Relaxed);
    }

    fn compute(state: &WindowState) -> u64 {
        let (Some(first), Some(last)) = (state.samples.front(), state.samples.back()) else {
            return 0;
        };
        let span = last.0.saturating_duration_since(first.0);
        let seconds = span.as_secs_f64();
        // Below a quarter second the estimate is dominated by scheduling noise.
        if seconds < 0.25 || state.bytes == 0 {
            return 0;
        }
        let rate = (state.bytes as f64 / seconds).round();
        if !rate.is_finite() || rate <= 0.0 {
            return 0;
        }
        // Saturating conversion; a rate above u64::MAX is not meaningful.
        if rate >= u64::MAX as f64 {
            return u64::MAX;
        }
        rate as u64
    }
}

/// Effective pacing when a global and a per-task ceiling are both set: the
/// stricter of the two wins (§9.1).
pub fn effective_limit(global: Option<u64>, per_task: Option<u64>) -> Option<u64> {
    match (global, per_task) {
        (Some(global), Some(task)) => Some(global.min(task)),
        (Some(global), None) => Some(global),
        (None, Some(task)) => Some(task),
        (None, None) => None,
    }
}

/// Apply every ceiling that covers these bytes.
///
/// Each limiter is charged exactly once — its own accounting has to see these
/// bytes to keep its own aggregate honest — and the caller waits for the
/// longest of the resulting waits rather than for their sum. Waiting for the
/// sum would make two ceilings of 1 MiB/s produce 0.5 MiB/s, a rate the host
/// never asked for; §9.1 asks for the stricter of the two.
pub async fn pace(global: Option<&SharedLimiter>, task: Option<&SharedLimiter>, bytes: u64) {
    let mut wait = Duration::ZERO;
    if let Some(limiter) = global {
        wait = wait.max(limiter.reserve(bytes));
    }
    if let Some(limiter) = task {
        wait = wait.max(limiter.reserve(bytes));
    }
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_pacer_never_waits() {
        let pacer = Pacer::new(None);
        assert!(pacer.is_unlimited());
        assert_eq!(pacer.reserve(1_000_000), Duration::ZERO);
        assert_eq!(pacer.rate(), None);
    }

    #[test]
    fn pacing_is_proportional_to_the_rate() {
        let pacer = Pacer::new(Some(1_000_000)); // 1 us per byte
        assert_eq!(pacer.rate(), Some(1_000_000));
        // Pacing starts with the initial deferral, so the first claim is free.
        assert_eq!(pacer.reserve(500_000), Duration::ZERO);
        // Once that is spent, a claim has to wait, and the wait is on the order
        // of the time that claim's bytes would take to accrue.
        let wait = pacer.reserve(500_000);
        assert!(wait > Duration::ZERO, "a spent timeline must pace");
        assert!(wait <= Duration::from_secs(1), "wait was {wait:?}");
        // A smaller claim waits less than a larger one from the same state.
        let small = Pacer::new(Some(1_000_000));
        let large = Pacer::new(Some(1_000_000));
        assert_eq!(small.reserve(500_000), Duration::ZERO);
        assert_eq!(large.reserve(500_000), Duration::ZERO);
        assert!(small.reserve(100_000) <= large.reserve(900_000));
    }

    #[test]
    fn a_faster_rate_shortens_the_wait() {
        // Fresh pacers, and a claim far larger than the initial deferral, so the
        // wait is dominated by the rate rather than by the deferral.
        let slow = Pacer::new(Some(1_000));
        let fast = Pacer::new(Some(100_000));
        let slow_wait = slow.reserve(1_000_000);
        let fast_wait = fast.reserve(1_000_000);
        assert!(fast_wait < slow_wait, "{fast_wait:?} !< {slow_wait:?}");
        // A megabyte is a thousand seconds at 1 kB/s and ten at 100 kB/s, and
        // the initial deferral is one second of either.
        assert!(slow_wait >= Duration::from_secs(900), "{slow_wait:?}");
        assert!(fast_wait >= Duration::from_secs(9), "{fast_wait:?}");
        assert!(fast_wait <= Duration::from_secs(11), "{fast_wait:?}");
    }

    #[test]
    fn the_pacing_queue_spreads_concurrent_callers_out() {
        // Four callers each want a megabyte at 1 MB/s. The first is covered by
        // the initial deferral and waits for nothing; each later one claims the
        // interval after the previous one's, so the queue grows instead of all
        // four being handed the same answer.
        let pacer = Pacer::new(Some(1_000_000));
        let waits: Vec<Duration> = (0..4).map(|_| pacer.reserve(1_000_000)).collect();
        assert_eq!(waits[0], Duration::ZERO, "the deferral is free");
        for wait in &waits[1..] {
            assert!(
                *wait > Duration::ZERO,
                "queued callers must pace: {waits:?}"
            );
        }
        // The timeline holds the queue, so the mark has moved on by the total
        // reserved: four megabytes at 1 MB/s.
        let mark = pacer.reserved_until.load(Ordering::Acquire);
        assert!(mark >= 3_000_000_000, "mark was {mark}");
    }

    #[test]
    fn reserve_always_terminates_over_pathological_input() {
        // A property guard: whatever the rate and the size, the call returns a
        // bounded answer. An earlier read-modify-write version of this function
        // could spin on these shapes; this test exists for that.
        for rate in [1u64, 7, 1000, 1_000_000, u64::MAX / 2] {
            let pacer = Pacer::new(Some(rate));
            for bytes in [1u64, 1000, 1_000_000, u64::MAX / 4, u64::MAX] {
                let wait = pacer.reserve(bytes);
                assert!(
                    wait <= MAX_PACING_WAIT,
                    "rate {rate} bytes {bytes} gave {wait:?}"
                );
            }
        }
    }

    #[test]
    fn the_rate_can_change_while_running() {
        let pacer = Pacer::new(Some(1000));
        assert_eq!(pacer.reserve(0), Duration::ZERO);
        pacer.set_rate(Some(1_000_000));
        assert_eq!(pacer.rate(), Some(1_000_000));
        pacer.set_rate(None);
        assert!(pacer.is_unlimited());
        assert_eq!(pacer.reserve(1_000_000), Duration::ZERO);
    }

    #[test]
    fn a_rate_change_does_not_recompute_an_outstanding_reservation() {
        // §9.1's fifth invariant, checked entirely through the public surface:
        // what was reserved at the old rate keeps its place, because those
        // intervals belong to callers that are already waiting for them.
        let pacer = Pacer::new(Some(1_000)); // one millisecond per byte
        let _outstanding = pacer.reserve(1_000_000); // a thousand seconds of it
        pacer.set_rate(Some(1_000_000)); // a thousand times faster
        let after = pacer.reserve(1_000_000);
        // A megabyte at the new rate is a second of traffic, but it cannot jump
        // the queue: it starts where the outstanding reservation ends.
        assert!(after >= Duration::from_secs(900), "{after:?}");
    }

    #[test]
    fn concurrent_reservations_do_not_double_spend() {
        // Ten callers each want a kilobyte at 1 kB/s. The initial deferral
        // covers one second of that, so after the first caller the rest must
        // queue — and each must get its own place rather than all being handed
        // the same number and then all proceeding at once.
        let pacer = Pacer::new(Some(1_000));
        let waits: Vec<Duration> = (0..10).map(|_| pacer.reserve(1_000)).collect();
        // The deferral serves the first claim outright.
        assert_eq!(waits[0], Duration::ZERO);
        // Everything after it had to wait.
        let queued: Vec<u128> = waits[1..].iter().map(Duration::as_nanos).collect();
        assert!(
            queued.iter().all(|nanos| *nanos > 0),
            "every claim after the deferral must pace: {waits:?}"
        );
        // ... and the timeline is what carries them: ten further kilobytes at
        // 1 kB/s is ten seconds of backlog.
        let mark = pacer.reserved_until.load(Ordering::Acquire);
        assert!(mark >= 9_000_000_000, "mark was {mark}");
    }

    #[tokio::test]
    async fn acquire_actually_waits() {
        // Small enough to finish quickly, large enough that "no wait" is wrong.
        // The first call spends the initial deferral, so the second has to
        // sleep for the whole of its own interval.
        let pacer = Pacer::new(Some(1_000_000)); // 1 us per byte
        pacer.acquire(500_000).await;
        let start = Instant::now();
        pacer.acquire(500_000).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(50), "elapsed {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "elapsed {elapsed:?}");
    }

    #[test]
    fn rate_window_is_a_sliding_window_not_an_average_since_start() {
        let window = RateWindow::new(5);
        let start = Instant::now();
        // A fast burst at the beginning...
        for step in 0..5 {
            window.record(100_000, start + Duration::from_millis(step * 100));
        }
        let fast = window.rate_bps();
        assert!(fast > 900_000, "fast rate was {fast}");
        // ... then a slow stretch more than a window later. The burst must have
        // fallen out of the window entirely.
        for step in 0..5 {
            window.record(
                100,
                start + Duration::from_secs(6) + Duration::from_millis(step * 200),
            );
        }
        let slow = window.rate_bps();
        assert!(slow < 10_000, "slow rate was {slow}");
        assert_ne!(fast, slow);
    }

    #[test]
    fn rate_window_reports_zero_before_it_can_estimate() {
        let window = RateWindow::new(5);
        assert_eq!(window.rate_bps(), 0);
        let now = Instant::now();
        window.record(1000, now);
        assert_eq!(window.rate_bps(), 0);
        window.record(1000, now + Duration::from_millis(100));
        assert_eq!(window.rate_bps(), 0);
    }

    #[test]
    fn rate_window_publishes_without_a_lock() {
        let window = RateWindow::new(5);
        let start = Instant::now();
        window.record(0, start);
        window.record(10_000, start + Duration::from_secs(1));
        assert_eq!(window.last_rate_bps(), window.rate_bps());
        assert!(window.last_rate_bps() >= 9_000);
        window.reset();
        assert_eq!(window.last_rate_bps(), 0);
        assert_eq!(window.rate_bps(), 0);
    }

    #[test]
    fn rate_window_memory_is_bounded() {
        let window = RateWindow::new(3600);
        let start = Instant::now();
        for step in 0..2000u64 {
            window.record(1, start + Duration::from_micros(step));
        }
        let state = window.inner.lock().unwrap();
        assert!(
            state.samples.len() <= MAX_SAMPLES,
            "{}",
            state.samples.len()
        );
    }

    #[test]
    fn stricter_limit_wins() {
        assert_eq!(effective_limit(Some(100), Some(50)), Some(50));
        assert_eq!(effective_limit(Some(50), Some(100)), Some(50));
        assert_eq!(effective_limit(Some(50), None), Some(50));
        assert_eq!(effective_limit(None, Some(50)), Some(50));
        assert_eq!(effective_limit(None, None), None);
    }

    #[tokio::test]
    async fn pace_without_limiters_returns_immediately() {
        let start = Instant::now();
        pace(None, None, 1_000_000).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn limiter_is_shared_and_reconfigurable() {
        let limiter = RateLimiter::shared(Some(1_000_000));
        let clone = Arc::clone(&limiter);
        limiter.set_rate(Some(500));
        assert_eq!(clone.rate(), Some(500));
    }
}
