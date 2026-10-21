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

use tokio::sync::Notify;

/// A token bucket.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    state: Mutex<Bucket>,
    /// Wakes waiters when the rate changes, so they rescale their waits at once.
    changed: Notify,
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
            changed: Notify::new(),
        }
    }

    /// Changes the rate; `None` removes the limit. Waiters rescale what is left of their waits
    /// to the new rate at once (see [`RateLimiter::acquire`]).
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
            // A changed limit keeps the debt for what was already received, now repaid at the
            // new rate, as the waiters' rescaled waits are.
            Some(old) => {
                let refill = now.saturating_duration_since(b.last).as_secs_f64() * old;
                (b.tokens + refill).min(b.burst)
            }
            // A new limit starts with a full burst.
            None => b.burst,
        };
        b.last = now;
        drop(b);
        self.changed.notify_waiters();
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

    /// Waits until `bytes` may be consumed. If the rate changes meanwhile, what is left of the
    /// wait is rescaled to the new rate: a lower limit stretches it, a higher one shortens it,
    /// and removing the limit ends it. Waiters never all return at once when the limit is
    /// lowered, which would let every reader through with whatever its connection had buffered.
    pub(crate) async fn acquire(&self, bytes: usize) {
        let mut rate = self.rate();
        let mut deadline = Instant::now() + self.take(bytes, Instant::now());
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Registered before the rate is read, so no change is missed.
            changed.as_mut().enable();
            let now = Instant::now();
            let current = self.rate();
            if current != rate {
                deadline = match (rate, current) {
                    (_, None) => return,
                    (Some(old), Some(new)) => {
                        now + deadline.saturating_duration_since(now).mul_f64(old / new)
                    }
                    // Unlimited before: nothing was owed.
                    (None, Some(_)) => deadline,
                };
                rate = current;
            }
            if now >= deadline {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline.into()) => {}
                () = &mut changed => {}
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

    /// Starts a waiter for `bytes` and returns it with the time it started.
    fn waiter(
        limiter: &std::sync::Arc<RateLimiter>,
        bytes: usize,
    ) -> (tokio::task::JoinHandle<()>, Instant) {
        let limiter = std::sync::Arc::clone(limiter);
        let started = Instant::now();
        (
            tokio::spawn(async move { limiter.acquire(bytes).await }),
            started,
        )
    }

    #[tokio::test]
    async fn lowering_the_limit_stretches_waits_instead_of_releasing_them() {
        let limiter = std::sync::Arc::new(RateLimiter::new(NonZeroU32::new(1_000_000).unwrap()));
        // 600 KB at 1 MB/s after the 100 KB burst: 0.5 s.
        let (waiter, started) = waiter(&limiter, 600_000);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // At half the rate the 0.4 s left take 0.8 s: done about 0.9 s after the start.
        limiter.set_rate(NonZeroU32::new(500_000));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !waiter.is_finished(),
            "the waiter was released when the limit was lowered"
        );
        waiter.await.unwrap();
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(850), "{took:?}");
        // The pause is bounded by the debt at the new rate, not stretched further.
        assert!(took < Duration::from_millis(1300), "{took:?}");
    }

    #[tokio::test]
    async fn raising_the_limit_shortens_waits_at_once() {
        let limiter = std::sync::Arc::new(RateLimiter::new(NonZeroU32::new(1_000_000).unwrap()));
        // About 1 s of debt at 1 MB/s.
        let (waiter, started) = waiter(&limiter, 1_100_000);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // At 4 MB/s the 0.9 s left take about 0.23 s.
        limiter.set_rate(NonZeroU32::new(4_000_000));
        waiter.await.unwrap();
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(250), "{took:?}");
        assert!(took < Duration::from_millis(600), "{took:?}");
    }
}
