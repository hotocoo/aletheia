//! Where boot time goes (ADR-162).
//!
//! Every target proves its contracts at boot, suite after suite, before the console or the desktop
//! is offered. That is the design; what it COSTS was never measured on the machine itself, and a
//! claim about speed that is not measured is an adjective. This module is a stopwatch with one
//! hand: `start` when the suites begin, `lap` as each family reports, `summary` when they are done.
//! The kernel prints each lap under the family's own marker line and one summary line, so a gate
//! log answers "which suite is slow on which CPU" without a profiler.
//!
//! Nothing here allocates or depends on interrupts: the platform's monotonic counter, read
//! through `Hal`, is the only input. The state is a handful of atomics so that a target with the
//! suites spread across functions needs to pass nothing around.

use crate::Hal;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

static START: AtomicU64 = AtomicU64::new(0);
static LAST: AtomicU64 = AtomicU64::new(0);
static LAPS: AtomicU32 = AtomicU32::new(0);
static SLOWEST_NS: AtomicU64 = AtomicU64::new(0);
static SLOWEST_PTR: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static SLOWEST_LEN: AtomicU64 = AtomicU64::new(0);
/// `total_ms` of the last [`summary`], or `u64::MAX` before one was taken (ADR-185's `boot`).
static RECORDED_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// The stopwatch's reading after the suites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Families that reported a lap.
    pub laps: u32,
    /// Milliseconds from `start` to `summary`, including anything between laps.
    pub total_ms: u64,
    /// The family whose lap was longest, and its milliseconds.
    pub slowest: &'static str,
    pub slowest_ms: u64,
}

/// Begin timing: the suites start now.
pub fn start<H: Hal>() {
    let t = H::timer_ticks();
    START.store(t, Ordering::SeqCst);
    LAST.store(t, Ordering::SeqCst);
    LAPS.store(0, Ordering::SeqCst);
    SLOWEST_NS.store(0, Ordering::SeqCst);
    SLOWEST_PTR.store(core::ptr::null_mut(), Ordering::SeqCst);
    SLOWEST_LEN.store(0, Ordering::SeqCst);
}

/// One family has reported: return the milliseconds since the previous lap (or `start`), and
/// remember it if it is the longest so far.
pub fn lap<H: Hal>(family: &'static str) -> u64 {
    let now = H::timer_ticks();
    let prev = LAST.swap(now, Ordering::SeqCst);
    let ns = H::ticks_to_ns(now.saturating_sub(prev));
    LAPS.fetch_add(1, Ordering::SeqCst);
    if ns > SLOWEST_NS.load(Ordering::SeqCst) {
        SLOWEST_NS.store(ns, Ordering::SeqCst);
        SLOWEST_PTR.store(family.as_ptr() as *mut u8, Ordering::SeqCst);
        SLOWEST_LEN.store(family.len() as u64, Ordering::SeqCst);
    }
    ns / 1_000_000
}

/// The reading: total since `start`, laps counted, the slowest family named.
pub fn summary<H: Hal>() -> Summary {
    let now = H::timer_ticks();
    let total_ns = H::ticks_to_ns(now.saturating_sub(START.load(Ordering::SeqCst)));
    let slowest = slowest_family();
    RECORDED_MS.store(total_ns / 1_000_000, Ordering::SeqCst);
    Summary {
        laps: LAPS.load(Ordering::SeqCst),
        total_ms: total_ns / 1_000_000,
        slowest,
        slowest_ms: SLOWEST_NS.load(Ordering::SeqCst) / 1_000_000,
    }
}

/// The name of the longest lap's family, or `"none"`.
fn slowest_family() -> &'static str {
    let ptr = SLOWEST_PTR.load(Ordering::SeqCst);
    let len = SLOWEST_LEN.load(Ordering::SeqCst) as usize;
    if ptr.is_null() {
        return "none";
    }
    // SAFETY: `ptr`/`len` were taken from a `&'static str` in `lap` and never modified; a
    // 'static str's bytes stay valid UTF-8 at that address for the program's lifetime.
    unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len)) }
}

/// The boot's reading as it was taken, without a clock: what the console's `boot` prints. `None`
/// until the boot called [`summary`].
pub fn recorded() -> Option<Summary> {
    let total_ms = RECORDED_MS.load(Ordering::SeqCst);
    if total_ms == u64::MAX {
        return None;
    }
    Some(Summary {
        laps: LAPS.load(Ordering::SeqCst),
        total_ms,
        slowest: slowest_family(),
        slowest_ms: SLOWEST_NS.load(Ordering::SeqCst) / 1_000_000,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU64 as Clock;

    static NOW: Clock = Clock::new(0);

    struct FakeHal;
    impl Hal for FakeHal {
        fn arch_name() -> &'static str {
            "fake"
        }
        fn timer_ticks() -> u64 {
            NOW.load(Ordering::SeqCst)
        }
        fn timer_freq_hz() -> u64 {
            1_000_000_000
        }
        fn ticks_to_ns(ticks: u64) -> u64 {
            ticks
        }
        fn current_privilege() -> u64 {
            0
        }
        fn exit(_code: i32) -> ! {
            panic!("exit")
        }
    }

    #[test]
    fn laps_are_measured_from_the_previous_lap_and_the_slowest_is_named() {
        NOW.store(1_000_000_000, Ordering::SeqCst);
        start::<FakeHal>();
        NOW.fetch_add(5_000_000, Ordering::SeqCst); // 5 ms
        assert_eq!(lap::<FakeHal>("quick"), 5);
        NOW.fetch_add(120_000_000, Ordering::SeqCst); // 120 ms
        assert_eq!(lap::<FakeHal>("slow"), 120);
        NOW.fetch_add(7_000_000, Ordering::SeqCst); // 7 ms
        assert_eq!(lap::<FakeHal>("quick2"), 7);
        NOW.fetch_add(3_000_000, Ordering::SeqCst); // 3 ms between the last lap and the summary
        let s = summary::<FakeHal>();
        assert_eq!(
            s,
            Summary {
                laps: 3,
                total_ms: 135,
                slowest: "slow",
                slowest_ms: 120,
            }
        );
        // The stopwatch is one static; the second scenario runs in the same test so the two
        // never race each other's clock.
        a_counter_that_ran_backwards_never_underflows();
    }

    fn a_counter_that_ran_backwards_never_underflows() {
        NOW.store(50, Ordering::SeqCst);
        start::<FakeHal>();
        NOW.store(10, Ordering::SeqCst);
        assert_eq!(lap::<FakeHal>("back"), 0);
        assert_eq!(summary::<FakeHal>().total_ms, 0);
    }
}
