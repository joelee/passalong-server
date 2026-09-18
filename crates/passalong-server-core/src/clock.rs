//! Time, injected so that leases and expiry are testable.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A source of the current time, in whole seconds since the Unix epoch.
pub trait Clock: Send + Sync {
    /// Now.
    fn now(&self) -> u64;
}

/// The operating system's clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs())
    }
}

/// A clock for tests: it moves only when told. Clones share the time.
#[derive(Debug, Clone, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    /// A clock showing `secs`.
    pub fn at(secs: u64) -> Self {
        Self(Arc::new(AtomicU64::new(secs)))
    }

    /// Moves the clock forward.
    pub fn advance(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_clock_moves_only_when_told() {
        let clock = ManualClock::at(100);
        assert_eq!(clock.now(), 100);
        clock.advance(5);
        assert_eq!(clock.now(), 105);
        let shared = clock.clone();
        shared.advance(1);
        assert_eq!(clock.now(), 106);
    }

    #[test]
    fn the_system_clock_is_past_2026() {
        assert!(SystemClock.now() > 1_767_225_600);
    }
}
