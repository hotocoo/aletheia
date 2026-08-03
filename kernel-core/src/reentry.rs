//! Re-entrancy detection for state a trap handler shares with the code it interrupted
//! (REQ-FAULT-002, ADR-039).
//!
//! A trap handler runs on top of whatever it interrupted. If the interrupted code was midway through
//! updating state the handler also touches — a scheduler's run queue, a saved register context, a
//! device's ring — then the handler sees a **half-updated** structure. The failure is not a crash at
//! the moment of re-entry; it is silent corruption discovered much later, which is the worst kind.
//!
//! Aletheia's answer is not "be careful": it is to make re-entry **detectable and fatal**. A section a
//! handler must not re-enter is wrapped in a [`ReentryGuard`]; entering while already entered returns
//! `None`, and the caller's contract is to treat that as a bug in the kernel — not to retry, not to
//! proceed. Detection is the point: an undetected nested entry is what the old code could not rule out.
//!
//! The counter is atomic, so this also detects a second CPU entering the same section — which is a
//! different bug (a missing lock) with the same consequence. `docs/INVARIANT-CONTRACTS.md` §INV-REENTRY
//! states the contract; `kernel-core/tests/reentry.rs` proves it, including that a guard whose token is
//! leaked stays closed forever rather than silently reopening.
use core::sync::atomic::{AtomicUsize, Ordering};

/// A section that must never be entered while it is already active.
pub struct ReentryGuard {
    depth: AtomicUsize,
    /// Total refusals since creation — a nonzero count is a kernel bug the boot log can report even if
    /// the immediate caller chose to continue.
    refusals: AtomicUsize,
}

/// Proof that the section was entered. Dropping it leaves the section.
pub struct Entered<'a> {
    guard: &'a ReentryGuard,
}

impl ReentryGuard {
    pub const fn new() -> Self {
        ReentryGuard {
            depth: AtomicUsize::new(0),
            refusals: AtomicUsize::new(0),
        }
    }

    /// Enter the section, or `None` if it is already active (a re-entry, or a second CPU).
    ///
    /// The transition is a compare-exchange, so two entrants racing cannot both win: exactly one gets
    /// the token and the other is refused. A refused attempt is COUNTED, so a handler that swallows the
    /// `None` still leaves evidence.
    pub fn enter(&self) -> Option<Entered<'_>> {
        match self
            .depth
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Some(Entered { guard: self }),
            Err(_) => {
                self.refusals.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Is the section active right now?
    pub fn active(&self) -> bool {
        self.depth.load(Ordering::Acquire) != 0
    }

    /// How many entries have been refused since creation. Nonzero ⇒ a re-entry really happened.
    pub fn refusals(&self) -> usize {
        self.refusals.load(Ordering::Relaxed)
    }
}

impl Default for ReentryGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.guard.depth.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_entry_is_refused_and_counted() {
        let g = ReentryGuard::new();
        let first = g.enter().expect("first entry");
        assert!(g.active());
        assert!(g.enter().is_none(), "re-entry must be refused");
        assert!(g.enter().is_none());
        assert_eq!(g.refusals(), 2);
        drop(first);
        assert!(!g.active());
        assert!(g.enter().is_some(), "the section reopens after leaving");
    }
}
