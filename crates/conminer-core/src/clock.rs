//! Time model (§14.4).
//!
//! Every line gets a **host** monotonic and a **host** wall timestamp at receipt.
//! Target-side timestamps (printk time, RTC lines, Zephyr log stamps) are
//! extracted fields stored alongside the raw line and are *never* trusted for
//! ordering — a board whose RTC steps mid-boot, or whose printk clock goes
//! backwards after NTP sync, must not be able to reorder the capture.
//!
//! The trait exists so replay and property tests can drive a deterministic clock
//! through the identical pipeline.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Unix milliseconds, UTC. Rendering in a local zone is the client's problem.
    fn now_wall_ms(&self) -> i64;
    /// Nanoseconds from an arbitrary origin. Only differences are meaningful, and
    /// it never goes backwards.
    fn now_mono_ns(&self) -> i64;
}

#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl SystemClock {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Clock for SystemClock {
    fn now_wall_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn now_mono_ns(&self) -> i64 {
        self.origin.elapsed().as_nanos() as i64
    }
}

/// Deterministic clock for replay and tests: every read advances by a fixed step,
/// so a corpus replayed twice produces identical timelines.
#[derive(Debug)]
pub struct StepClock {
    wall_ms: AtomicI64,
    mono_ns: AtomicI64,
    step_ms: i64,
}

impl StepClock {
    pub fn new(start_wall_ms: i64, step_ms: i64) -> Self {
        Self {
            wall_ms: AtomicI64::new(start_wall_ms),
            mono_ns: AtomicI64::new(0),
            step_ms,
        }
    }

    /// Advance without reading, e.g. to simulate a silence gap.
    pub fn advance_ms(&self, ms: i64) {
        self.wall_ms.fetch_add(ms, Ordering::SeqCst);
        self.mono_ns.fetch_add(ms * 1_000_000, Ordering::SeqCst);
    }
}

impl Default for StepClock {
    fn default() -> Self {
        // A fixed, obviously-synthetic origin: 2020-01-01T00:00:00Z.
        Self::new(1_577_836_800_000, 1)
    }
}

impl Clock for StepClock {
    fn now_wall_ms(&self) -> i64 {
        self.wall_ms.fetch_add(self.step_ms, Ordering::SeqCst)
    }

    fn now_mono_ns(&self) -> i64 {
        self.mono_ns
            .fetch_add(self.step_ms * 1_000_000, Ordering::SeqCst)
    }
}

pub type SharedClock = Arc<dyn Clock>;

pub fn system() -> SharedClock {
    Arc::new(SystemClock::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_never_goes_backwards() {
        let c = SystemClock::new();
        let a = c.now_mono_ns();
        let b = c.now_mono_ns();
        assert!(b >= a);
    }

    #[test]
    fn step_clock_is_deterministic() {
        let a = StepClock::default();
        let b = StepClock::default();
        for _ in 0..10 {
            assert_eq!(a.now_wall_ms(), b.now_wall_ms());
            assert_eq!(a.now_mono_ns(), b.now_mono_ns());
        }
    }

    #[test]
    fn step_clock_advance_simulates_silence() {
        let c = StepClock::new(1000, 0);
        assert_eq!(c.now_wall_ms(), 1000);
        c.advance_ms(30_000);
        assert_eq!(c.now_wall_ms(), 31_000);
    }
}
