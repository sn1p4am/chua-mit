//! Rate control and measurement: `spec/engine-v1.md` §9.1 and §9.2.
//!
//! Two independent pieces:
//!
//! * [`TokenBucket`] — smooth pacing. A bucket, not a burst-then-sleep, because
//!   §9.1 forbids the pulse that long sleeps produce.
//! * [`RateWindow`] — a sliding-window throughput estimate. Total bytes over
//!   total time is explicitly rejected by §9.2: it makes a long transfer
//!   unresponsive to recent change.
//!
//! Both are shared by `Arc` because every segment of a task runs concurrently.
//! The bucket keeps its state in atomics so that acquiring a token never holds a
//! lock across an await point.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Shortest sleep between refill checks. Caps how much a low rate can
/// overshoot, at the cost of a few wakeups per second at large token sizes.
const MAX_REFILL_SLEEP: Duration = Duration::from_millis(50);

/// Smallest interval a caller may ask for; guards against a busy loop.
const MIN_WAIT: Duration = Duration::from_millis(1);

/// A token bucket that refills linearly. One token is one byte, so a bucket
/// sized to half a second of traffic absorbs normal jitter without allowing a
/// visible burst.
#[derive(Debug)]
pub struct TokenBucket {
    /// Nanoseconds of wall time per byte at the configured rate. Zero means
    /// unlimited.
    nanos_per_byte: AtomicU64,
    /// Tokens available right now. Only ever written to `capacity`.
    tokens: AtomicU64,
    /// Instant the token count was last reconciled, in nanoseconds since
    /// `origin`.
    token_stamp: AtomicU64,
    /// When the next unreserved byte becomes available, in nanoseconds since
    /// `origin`. A caller claims a slot by pushing this forward and then waiting
    /// for it, which is what spreads concurrent callers out in arrival order.
    next_slot_nanos: AtomicU64,
    /// Burst capacity in bytes.
    capacity: AtomicU64,
    origin: Instant,
}

impl TokenBucket {
    /// A bucket pacing `bytes_per_second`. `None` means unlimited.
    pub fn new(bytes_per_second: Option<u64>) -> Self {
        let bucket = Self {
            nanos_per_byte: AtomicU64::new(0),
            tokens: AtomicU64::new(0),
            token_stamp: AtomicU64::new(0),
            next_slot_nanos: AtomicU64::new(0),
            capacity: AtomicU64::new(0),
            origin: Instant::now(),
        };
        bucket.set_rate(bytes_per_second);
        // Start full, so the first chunk is not delayed.
        bucket.tokens.store(bucket.capacity.load(Ordering::Relaxed), Ordering::Relaxed);
        bucket
    }

    /// Change the rate while running. §9.1 requires the change to take effect
    /// without restarting the transfer, so this is a plain store.
    pub fn set_rate(&self, bytes_per_second: Option<u64>) {
        match bytes_per_second {
            Some(rate) if rate > 0 => {
                // Nanoseconds per byte, rounded up so the pacing is never
                // faster than requested.
                let nanos = 1_000_000_000u64.div_ceil(rate);
                // Half a second of traffic, and never below one byte.
                self.capacity.store((rate / 2).max(1), Ordering::Relaxed);
                self.nanos_per_byte.store(nanos.max(1), Ordering::Relaxed);
            }
            _ => {
                self.nanos_per_byte.store(0, Ordering::Relaxed);
                self.capacity.store(0, Ordering::Relaxed);
                self.tokens.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Current pacing rate in bytes per second; `None` when unlimited.
    pub fn rate(&self) -> Option<u64> {
        let nanos = self.nanos_per_byte.load(Ordering::Relaxed);
        (nanos > 0).then(|| 1_000_000_000u64 / nanos)
    }

    /// Whether the bucket is currently unlimited.
    pub fn is_unlimited(&self) -> bool {
        self.nanos_per_byte.load(Ordering::Relaxed) == 0
    }

    /// Wait until `bytes` may be sent.
    pub async fn acquire(&self, bytes: u64) {
        loop {
            let wait = self.reserve(bytes);
            if wait.is_zero() {
                return;
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// Claim `bytes` worth of pacing and report how long to wait.
    ///
    /// Split out from [`TokenBucket::acquire`] so the arithmetic is testable
    /// without a runtime. The wait is capped so that a long queue never turns
    /// into one enormous sleep, which is what would produce the traffic pulse
    /// §9.1 forbids.
    pub fn reserve(&self, bytes: u64) -> Duration {
        let nanos_per_byte = self.nanos_per_byte.load(Ordering::Relaxed);
        if nanos_per_byte == 0 || bytes == 0 {
            return Duration::ZERO;
        }
        let now = self.now_nanos();
        let capacity = self.capacity.load(Ordering::Relaxed);
        self.refill(now, capacity, nanos_per_byte);
        let wait = self.claim(bytes, nanos_per_byte, now);
        wait.min(MAX_REFILL_SLEEP)
    }

    /// Add the tokens that have accrued since the last reconciliation.
    fn refill(&self, now: u64, capacity: u64, nanos_per_byte: u64) {
        let stamp = self.token_stamp.load(Ordering::Acquire);
        if now <= stamp {
            return;
        }
        let elapsed = now - stamp;
        let accrued = elapsed / nanos_per_byte;
        if accrued > 0 {
            let minted = accrued.saturating_mul(nanos_per_byte);
            self.tokens
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |tokens| {
                    Some(tokens.saturating_add(accrued).min(capacity))
                })
                .ok();
            // Leave the fraction of a token that has not been minted yet, so a
            // slow rate does not round down to nothing forever.
            self.token_stamp
                .store(now.saturating_sub(elapsed - minted), Ordering::Release);
        }
    }

    /// Take `bytes` from the bucket if they are there, otherwise queue behind
    /// whatever has already been claimed.
    fn claim(&self, bytes: u64, nanos_per_byte: u64, now: u64) -> Duration {
        loop {
            let tokens = self.tokens.load(Ordering::Acquire);
            if tokens >= bytes
                && self
                    .tokens
                    .compare_exchange_weak(
                        tokens,
                        tokens - bytes,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                return Duration::ZERO;
            }
            // Not enough on hand. Claim a slot at the end of the current queue
            // and wait for it, so N concurrent callers are spread over N
            // intervals instead of all waking for the same tokens.
            let mut slot = self.next_slot_nanos.load(Ordering::Acquire).max(now);
            loop {
                let cost = (bytes - tokens.min(bytes)).saturating_mul(nanos_per_byte);
                let proposed = slot.saturating_add(cost);
                match self.next_slot_nanos.compare_exchange_weak(
                    slot,
                    proposed,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Duration::from_nanos(proposed.saturating_sub(now)),
                    Err(actual) => slot = actual.max(now),
                }
            }
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
    bucket: TokenBucket,
}

impl RateLimiter {
    /// A limiter with an initial rate.
    pub fn new(bytes_per_second: Option<u64>) -> Self {
        Self {
            bucket: TokenBucket::new(bytes_per_second),
        }
    }

    /// Shared handle.
    pub fn shared(bytes_per_second: Option<u64>) -> SharedLimiter {
        Arc::new(Self::new(bytes_per_second))
    }

    /// Change the rate in place.
    pub fn set_rate(&self, bytes_per_second: Option<u64>) {
        self.bucket.set_rate(bytes_per_second);
    }

    /// Current rate.
    pub fn rate(&self) -> Option<u64> {
        self.bucket.rate()
    }

    /// Wait until `bytes` may be sent.
    pub async fn acquire(&self, bytes: u64) {
        self.bucket.acquire(bytes).await;
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

/// Apply both limiters to one chunk. Both must release before the chunk may be
/// sent, which is what makes the stricter of the two effective.
pub async fn pace(global: Option<&SharedLimiter>, task: Option<&SharedLimiter>, bytes: u64) {
    if let Some(limiter) = global
        && !limiter.bucket.is_unlimited()
    {
        limiter.acquire(bytes).await;
    }
    if let Some(limiter) = task
        && !limiter.bucket.is_unlimited()
    {
        limiter.acquire(bytes).await;
    }
}

/// Convenience alias so callers do not have to name the bucket.
pub type SharedBucket = Arc<TokenBucket>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_bucket_never_waits() {
        let bucket = TokenBucket::new(None);
        assert!(bucket.is_unlimited());
        assert_eq!(bucket.reserve(1_000_000), Duration::ZERO);
        assert_eq!(bucket.rate(), None);
    }

    #[test]
    fn pacing_is_proportional_to_the_rate() {
        let bucket = TokenBucket::new(Some(1000)); // 1000 B/s -> 1 ms per byte
        assert_eq!(bucket.rate(), Some(1000));
        // The first chunk gets an idle allowance and costs nothing.
        assert_eq!(bucket.reserve(10), Duration::ZERO);
        // The next chunk is paced at the configured rate.
        let wait = bucket.reserve(1000);
        assert!(
            wait >= Duration::from_millis(900) && wait <= Duration::from_millis(1100),
            "wait was {wait:?}"
        );
    }

    #[test]
    fn a_faster_rate_shortens_the_wait() {
        let slow = TokenBucket::new(Some(1000));
        let fast = TokenBucket::new(Some(100_000));
        let _ = slow.reserve(100);
        let _ = fast.reserve(100);
        let slow_wait = slow.reserve(10_000);
        let fast_wait = fast.reserve(10_000);
        assert!(fast_wait < slow_wait, "{fast_wait:?} !< {slow_wait:?}");
    }

    #[test]
    fn the_rate_can_change_while_running() {
        let bucket = TokenBucket::new(Some(1000));
        let _ = bucket.reserve(0);
        bucket.set_rate(Some(1_000_000));
        assert_eq!(bucket.rate(), Some(1_000_000));
        bucket.set_rate(None);
        assert!(bucket.is_unlimited());
    }

    #[test]
    fn concurrent_reservations_do_not_double_spend() {
        // Ten callers each reserve a second of traffic at 1000 B/s. The waits
        // must be distinct and increasing, not all identical.
        let bucket = TokenBucket::new(Some(1000));
        let mut waits = Vec::new();
        for _ in 0..10 {
            waits.push(bucket.reserve(1000));
        }
        let mut sorted = waits.clone();
        sorted.sort();
        assert_eq!(waits, sorted, "reservations must be handed out in order");
        assert!(sorted[0] < sorted[9]);
    }

    #[tokio::test]
    async fn acquire_actually_waits() {
        let bucket = TokenBucket::new(Some(10_000)); // 10 kB/s
        // Burn the idle allowance.
        bucket.acquire(0).await;
        let start = Instant::now();
        bucket.acquire(10_000).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(500), "elapsed {elapsed:?}");
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
        assert!(state.samples.len() <= MAX_SAMPLES, "{}", state.samples.len());
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
