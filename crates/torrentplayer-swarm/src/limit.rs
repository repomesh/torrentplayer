// SPDX-License-Identifier: MIT OR Apache-2.0

//! Download rate limiters: one shared by the whole engine, and one per torrent whose rate can
//! change at any time (TorrentPlayer's bandwidth planner gives background torrents what playback
//! leaves).
//!
//! Peer readers wait here after each read, so TCP flow control slows the peers down. The burst
//! is kept small (a tenth of a second of the rate) so a limit behaves like a slower link rather
//! than a fast link that stalls once a second.

use std::num::NonZeroU32;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Longest single sleep while waiting, so a raised or removed limit takes effect promptly.
const MAX_SLEEP: Duration = Duration::from_millis(250);

/// A token bucket.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// `None` when unlimited.
    bytes_per_sec: Option<f64>,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub(crate) fn new(limit: NonZeroU32) -> Self {
        let limiter = Self::unlimited();
        limiter.set_rate(Some(limit));
        limiter
    }

    /// A limiter that lets everything through until [`RateLimiter::set_rate`] sets a rate.
    pub(crate) fn unlimited() -> Self {
        Self {
            state: Mutex::new(Bucket {
                bytes_per_sec: None,
                burst: 0.0,
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }

    /// Changes the rate; `None` removes the limit. Waiters see the change within
    /// [`MAX_SLEEP`].
    pub(crate) fn set_rate(&self, limit: Option<NonZeroU32>) {
        let mut b = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let rate = limit.map(|l| f64::from(l.get()));
        if rate == b.bytes_per_sec {
            return;
        }
        let now = Instant::now();
        let old = b.bytes_per_sec;
        b.bytes_per_sec = rate;
        b.burst = rate.map_or(0.0, |r| (r / 10.0).max(64.0 * 1024.0));
        b.tokens = match old {
            // A changed limit keeps the debt for what was already received (waiters stop
            // waiting for it, so new reads repay it); otherwise lowering the limit would let
            // through everything the readers were holding back.
            Some(old) => {
                let refill = now.saturating_duration_since(b.last).as_secs_f64() * old;
                (b.tokens + refill).min(b.burst)
            }
            // A new limit starts with a full burst.
            None => b.burst,
        };
        b.last = now;
    }

    /// The current rate, if limited.
    pub(crate) fn rate(&self) -> Option<f64> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bytes_per_sec
    }

    /// Takes `bytes` from the bucket and returns how long the caller should wait.
    pub(crate) fn take(&self, bytes: usize, now: Instant) -> Duration {
        let mut b = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(rate) = b.bytes_per_sec else {
            return Duration::ZERO;
        };
        let refill = now.saturating_duration_since(b.last).as_secs_f64() * rate;
        b.tokens = (b.tokens + refill).min(b.burst);
        b.last = now;
        #[allow(clippy::cast_precision_loss)]
        {
            b.tokens -= bytes as f64;
        }
        if b.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-b.tokens / rate)
        }
    }

    /// Waits until `bytes` may be consumed. Stops waiting early if the rate changes.
    pub(crate) async fn acquire(&self, bytes: usize) {
        let rate = self.rate();
        let wait = self.take(bytes, Instant::now());
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            tokio::time::sleep(left.min(MAX_SLEEP)).await;
            if self.rate() != rate {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_the_rate_after_a_small_burst() {
        let limiter = RateLimiter::new(NonZeroU32::new(1_000_000).unwrap());
        let t0 = Instant::now();
        // The burst (100 KB) is free.
        assert_eq!(limiter.take(100_000, t0), Duration::ZERO);
        // Another 500 KB at the same instant must wait 0.5 s.
        let wait = limiter.take(500_000, t0);
        assert!((wait.as_secs_f64() - 0.5).abs() < 0.01, "{wait:?}");
        // After that time has passed the debt is repaid.
        assert_eq!(
            limiter.take(0, t0 + Duration::from_millis(500)),
            Duration::ZERO
        );
    }

    #[test]
    fn the_rate_can_change_or_be_removed() {
        let limiter = RateLimiter::unlimited();
        let t0 = Instant::now();
        assert_eq!(limiter.take(100_000_000, t0), Duration::ZERO, "unlimited");
        limiter.set_rate(NonZeroU32::new(1_000_000));
        assert_eq!(limiter.rate(), Some(1_000_000.0));
        let t1 = Instant::now();
        assert!(limiter.take(600_000, t1) > Duration::ZERO, "limited");
        // Removing the limit forgives the debt.
        limiter.set_rate(None);
        assert_eq!(limiter.take(10_000_000, t1), Duration::ZERO);
    }

    #[test]
    fn a_changed_limit_keeps_the_debt() {
        let limiter = RateLimiter::new(NonZeroU32::new(2_000_000).unwrap());
        // 1 MB at once at 2 MB/s: about 0.4 s of debt after the 200 KB burst.
        let wait = limiter.take(1_000_000, Instant::now());
        assert!(wait > Duration::from_millis(350), "{wait:?}");
        // At half the rate the same debt takes twice as long to repay.
        limiter.set_rate(NonZeroU32::new(1_000_000));
        let wait = limiter.take(0, Instant::now());
        assert!(wait > Duration::from_millis(700), "{wait:?}");
        // Raising the limit repays it faster.
        limiter.set_rate(NonZeroU32::new(4_000_000));
        let wait = limiter.take(0, Instant::now());
        assert!(wait > Duration::from_millis(150), "{wait:?}");
        assert!(wait < Duration::from_millis(250), "{wait:?}");
    }

    #[tokio::test]
    async fn waiters_stop_early_when_the_limit_is_lifted() {
        let limiter = std::sync::Arc::new(RateLimiter::new(NonZeroU32::new(65_536).unwrap()));
        // About 15 s of debt at this rate.
        let waiter = {
            let limiter = std::sync::Arc::clone(&limiter);
            tokio::spawn(async move { limiter.acquire(1_000_000).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        limiter.set_rate(None);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("the waiter returns soon after the limit is lifted")
            .unwrap();
    }
}
