//! Time, injectable: the rate limiters wait on a clock, and tests wait on
//! one that only moves when told to.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub trait Clock: Send + Sync {
    /// Time since an arbitrary fixed origin. Only differences mean anything.
    fn now(&self) -> Duration;
    /// Blocks for `duration` (a test clock advances instead).
    fn sleep(&self, duration: Duration);
}

/// The real clock.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A clock that moves only when slept on or advanced. Records every sleep,
/// so a test can assert how long a limiter waited.
#[derive(Debug, Default)]
pub struct ManualClock {
    state: Mutex<ManualState>,
}

#[derive(Debug, Default)]
struct ManualState {
    now: Duration,
    sleeps: Vec<Duration>,
}

impl ManualClock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&self, by: Duration) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.now += by;
    }

    /// Every duration slept so far, in order.
    #[must_use]
    pub fn sleeps(&self) -> Vec<Duration> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sleeps.clone()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).now
    }

    fn sleep(&self, duration: Duration) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.now += duration;
        state.sleeps.push(duration);
    }
}
