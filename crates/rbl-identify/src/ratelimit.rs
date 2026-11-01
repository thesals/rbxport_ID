//! Per-service rate limits, enforced in code, and the retry policy every
//! client sends through.
//!
//! The limiter is strict: it never lets more than `count` requests start in
//! any window of length `per`. A classic token bucket of capacity `count`
//! refilling `count` per `per` lets almost `2 × count` through in one window
//! (a full bucket, then the refill), which would break "≤ 3 req/s" read
//! literally. So each limiter remembers the start times of the last `count`
//! requests and holds a new one until the oldest of them is `per` old. Its
//! burst-then-wait behaviour is the bucket's; only the edge case differs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::clock::Clock;
use crate::error::{IdentifyError, Result};
use crate::transport::{Request, Response, Transport};

/// At most `count` requests per `per`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub count: u32,
    pub per: Duration,
}

impl Rate {
    /// AcoustID: "Do not make more than 3 requests per second."
    pub const ACOUSTID: Rate = Rate { count: 3, per: Duration::from_secs(1) };
    /// MusicBrainz: one request per second per client.
    pub const MUSICBRAINZ: Rate = Rate { count: 1, per: Duration::from_secs(1) };
    /// Discogs, authenticated: 60 requests per minute.
    pub const DISCOGS: Rate = Rate { count: 60, per: Duration::from_secs(60) };
}

/// Retries after a 429 or 503 before giving up.
pub const MAX_RETRIES: u32 = 3;

/// The first backoff when the service sends no `Retry-After`; it doubles
/// on each retry (1 s, 2 s, 4 s).
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Holds callers so that no more than `rate.count` requests start in any
/// window of `rate.per`. Thread-safe; callers are served in the order they
/// reserve a slot.
pub struct RateLimiter {
    rate: Rate,
    clock: Arc<dyn Clock>,
    /// Start times of the most recent requests, oldest first; never longer
    /// than `rate.count`.
    recent: Mutex<VecDeque<Duration>>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter").field("rate", &self.rate).finish_non_exhaustive()
    }
}

impl RateLimiter {
    pub fn new(rate: Rate, clock: Arc<dyn Clock>) -> Self {
        // A zero count would never admit anything; treat it as one.
        let rate = Rate { count: rate.count.max(1), per: rate.per };
        let capacity = usize::try_from(rate.count).unwrap_or(usize::MAX).min(1024);
        Self { rate, clock, recent: Mutex::new(VecDeque::with_capacity(capacity)) }
    }

    pub fn rate(&self) -> Rate {
        self.rate
    }

    /// Blocks, by sleeping on the clock, until a request may start.
    pub fn acquire(&self) {
        let wait = {
            let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
            let now = self.clock.now();
            let full = u32::try_from(recent.len()).map_or(true, |len| len >= self.rate.count);
            // Reserve a slot now, even if it lies in the future, so a
            // concurrent caller queues behind it rather than racing for it.
            let slot = if full {
                let oldest = recent.pop_front().unwrap_or(now);
                now.max(oldest + self.rate.per)
            } else {
                now
            };
            recent.push_back(slot);
            slot.saturating_sub(now)
        };
        if !wait.is_zero() {
            self.clock.sleep(wait);
        }
    }
}

/// Sends `request`, waiting on `limiter` before every attempt.
///
/// A 2xx response is returned. A 429 or 503 is retried up to
/// [`MAX_RETRIES`] times, after the service's `Retry-After` or else an
/// exponential backoff from [`BACKOFF_BASE`]; after that, and for every other
/// status, the result is [`IdentifyError::Status`]. Transport failures are
/// returned as they are, without a retry.
pub fn send(
    transport: &dyn Transport,
    limiter: &RateLimiter,
    clock: &dyn Clock,
    service: &'static str,
    request: &Request,
) -> Result<Response> {
    let mut retries = 0u32;
    loop {
        limiter.acquire();
        let response = transport.send(service, request)?;
        match response.status {
            200..=299 => return Ok(response),
            status @ (429 | 503) if retries < MAX_RETRIES => {
                let wait = response.retry_after.unwrap_or(BACKOFF_BASE * 2u32.pow(retries));
                retries += 1;
                // No URL here: it can carry a key.
                tracing::debug!(service, status, retry = retries, wait_ms = wait.as_millis(), "throttled; retrying");
                clock.sleep(wait);
            }
            status => return Err(IdentifyError::Status { service, status }),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::transport::Request;

    fn limiter(rate: Rate) -> (Arc<ManualClock>, RateLimiter) {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::new(rate, clock.clone());
        (clock, limiter)
    }

    /// Start times of `n` acquisitions.
    fn starts(clock: &ManualClock, limiter: &RateLimiter, n: usize) -> Vec<Duration> {
        (0..n)
            .map(|_| {
                limiter.acquire();
                clock.now()
            })
            .collect()
    }

    /// No window of length `rate.per` holds more than `rate.count` starts.
    fn assert_within(rate: Rate, starts: &[Duration]) {
        let count = usize::try_from(rate.count).unwrap();
        for (i, start) in starts.iter().enumerate() {
            let in_window = starts[i..].iter().take_while(|s| **s < *start + rate.per).count();
            assert!(in_window <= count, "{in_window} starts within {:?} of {start:?}", rate.per);
        }
    }

    #[test]
    fn acoustid_bursts_three_then_waits_a_second() {
        let (clock, limiter) = limiter(Rate::ACOUSTID);
        let starts = starts(&clock, &limiter, 7);
        let ms = |n| Duration::from_millis(n);
        assert_eq!(starts, [ms(0), ms(0), ms(0), ms(1000), ms(1000), ms(1000), ms(2000)]);
        assert_eq!(clock.sleeps(), [ms(1000), ms(1000)]);
        assert_within(Rate::ACOUSTID, &starts);
    }

    #[test]
    fn musicbrainz_spaces_requests_a_second_apart() {
        let (clock, limiter) = limiter(Rate::MUSICBRAINZ);
        let starts = starts(&clock, &limiter, 4);
        let s = Duration::from_secs;
        assert_eq!(starts, [s(0), s(1), s(2), s(3)]);
        assert!(clock.sleeps().iter().sum::<Duration>() >= s(3));
    }

    #[test]
    fn discogs_bursts_sixty_then_waits_a_minute() {
        let (clock, limiter) = limiter(Rate::DISCOGS);
        let starts = starts(&clock, &limiter, 121);
        assert!(starts[..60].iter().all(Duration::is_zero));
        assert!(starts[60..120].iter().all(|s| *s == Duration::from_secs(60)));
        assert_eq!(starts[120], Duration::from_secs(120));
        assert_within(Rate::DISCOGS, &starts);
    }

    #[test]
    fn time_spent_elsewhere_counts_toward_the_wait() {
        let (clock, limiter) = limiter(Rate::MUSICBRAINZ);
        limiter.acquire();
        clock.advance(Duration::from_millis(400));
        limiter.acquire();
        assert_eq!(clock.sleeps(), [Duration::from_millis(600)]);
        clock.advance(Duration::from_secs(5));
        limiter.acquire();
        assert_eq!(clock.sleeps().len(), 1, "an idle limiter does not wait");
    }

    #[test]
    fn concurrent_callers_still_respect_the_rate() {
        let clock = Arc::new(ManualClock::new());
        let limiter = Arc::new(RateLimiter::new(Rate::ACOUSTID, clock.clone()));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let limiter = limiter.clone();
                std::thread::spawn(move || {
                    for _ in 0..3 {
                        limiter.acquire();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        // 12 requests at 3 per second: the last three cannot start before
        // 3 s. (The manual clock adds up every thread's sleeps, so later is
        // possible here; earlier never is.)
        let reserved: Vec<Duration> = limiter.recent.lock().unwrap().iter().copied().collect();
        assert_eq!(reserved.len(), 3);
        assert!(reserved.iter().all(|s| *s >= Duration::from_secs(3)), "{reserved:?}");
    }

    /// Answers with the given statuses in turn, then 200.
    struct Scripted {
        replies: Mutex<VecDeque<(u16, Option<Duration>)>>,
        sent: Mutex<u32>,
    }

    impl Scripted {
        fn new(replies: &[(u16, Option<Duration>)]) -> Self {
            Self { replies: Mutex::new(replies.iter().copied().collect()), sent: Mutex::new(0) }
        }
        fn sent(&self) -> u32 {
            *self.sent.lock().unwrap()
        }
    }

    impl Transport for Scripted {
        fn send(&self, _: &'static str, _: &Request) -> Result<Response> {
            *self.sent.lock().unwrap() += 1;
            let (status, retry_after) = self.replies.lock().unwrap().pop_front().unwrap_or((200, None));
            Ok(Response { status, body: Vec::new(), retry_after })
        }
    }

    fn run(replies: &[(u16, Option<Duration>)]) -> (Result<Response>, Vec<Duration>, u32) {
        let clock = Arc::new(ManualClock::new());
        // A generous rate, so every recorded sleep is a backoff.
        let limiter = RateLimiter::new(Rate { count: 100, per: Duration::from_secs(1) }, clock.clone());
        let transport = Scripted::new(replies);
        let result = send(&transport, &limiter, &*clock, "test", &Request::get("https://example.invalid/"));
        (result, clock.sleeps(), transport.sent())
    }

    #[test]
    fn retry_after_is_honoured() {
        let (result, sleeps, sent) = run(&[(429, Some(Duration::from_secs(7)))]);
        assert_eq!(result.unwrap().status, 200);
        assert_eq!(sleeps, [Duration::from_secs(7)]);
        assert_eq!(sent, 2);
    }

    #[test]
    fn without_retry_after_the_backoff_doubles() {
        let (result, sleeps, sent) = run(&[(503, None), (429, None), (503, None)]);
        assert_eq!(result.unwrap().status, 200);
        let s = Duration::from_secs;
        assert_eq!(sleeps, [s(1), s(2), s(4)]);
        assert_eq!(sent, 4);
    }

    #[test]
    fn it_gives_up_after_three_retries() {
        let (result, sleeps, sent) = run(&[(429, None); 5]);
        assert!(matches!(result, Err(IdentifyError::Status { service: "test", status: 429 })));
        assert_eq!(sleeps.len(), 3);
        assert_eq!(sent, 4);
    }

    #[test]
    fn other_errors_are_not_retried() {
        let (result, sleeps, sent) = run(&[(404, None)]);
        assert!(matches!(result, Err(IdentifyError::Status { status: 404, .. })));
        assert_eq!(sleeps, Vec::<Duration>::new());
        assert_eq!(sent, 1);
    }

    #[test]
    fn every_attempt_waits_on_the_limiter() {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::new(Rate::MUSICBRAINZ, clock.clone());
        let transport = Scripted::new(&[(503, Some(Duration::ZERO))]);
        send(&transport, &limiter, &*clock, "test", &Request::get("https://example.invalid/")).unwrap();
        // The retry came 0 s after the 503, so the limiter held it for 1 s.
        assert_eq!(clock.sleeps(), [Duration::ZERO, Duration::from_secs(1)]);
    }
}
