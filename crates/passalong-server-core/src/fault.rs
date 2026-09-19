//! Named places where the kill harness makes the process die.
//!
//! With the `fault-injection` feature, [`point`] kills the process when the
//! environment names the point it was called with: `PASSALONG_FAULT` holds
//! the name, and `PASSALONG_FAULT_SKIP` how many times to pass it first.
//! Without the feature, which is how the server is built, `point` is empty
//! and the environment is never read.

/// Every fault point, so that the harness can tell a point that never fired
/// from one that does not exist.
pub const POINTS: &[&str] = &[
    "shelf: before the rename",
    "shelf: inside a removal",
    "upload: after the publish",
    "ledger: before the commit",
    "engine: after the commit",
    "engine: between clean-ups",
];

/// Dies here, if the harness asked for it.
#[cfg(feature = "fault-injection")]
pub fn point(name: &'static str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static PASSED: AtomicU64 = AtomicU64::new(0);
    debug_assert!(POINTS.contains(&name), "unknown fault point `{name}`");
    if std::env::var("PASSALONG_FAULT").as_deref() != Ok(name) {
        return;
    }
    let skip = std::env::var("PASSALONG_FAULT_SKIP")
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0);
    if PASSED.fetch_add(1, Ordering::SeqCst) == skip {
        die();
    }
}

/// Ends the process by SIGKILL, as `kill -9` would: no unwinding, no
/// destructors, no flushing, and nothing the process can do about it. Not
/// `abort`, whose SIGABRT dumps core: the harness kills hundreds of times a
/// run, and a desktop that announces crashes would announce every one.
/// `std` cannot send a signal, and `unsafe` is not allowed here, so `kill`
/// does it. Should that fail, `abort` still ends the process, and the
/// harness, which expects SIGKILL, says so.
#[cfg(feature = "fault-injection")]
fn die() -> ! {
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &std::process::id().to_string()])
        .status();
    // SIGKILL is delivered before `kill` has returned; this is not reached.
    std::process::abort();
}

/// Does nothing: this build has no fault injection.
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub fn point(_name: &'static str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_that_nobody_asked_for_is_passed() {
        for name in POINTS {
            // The test process sets no PASSALONG_FAULT, so this returns.
            point(name);
        }
        let unique: std::collections::BTreeSet<_> = POINTS.iter().collect();
        assert_eq!(unique.len(), POINTS.len());
    }
}
