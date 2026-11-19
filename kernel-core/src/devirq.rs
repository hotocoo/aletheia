//! What a device completion interrupt is judged by (REQ-DRV-019, ADR-264): the counters a
//! platform's completion wait keeps for one interrupt source, and the one verdict function every
//! driver's boot invariants share, so NVMe and AHCI cannot disagree about what "interrupt-driven"
//! means and a silent fall back to polling cannot pass either.
//!
//! The platform owns the counting (its handler runs in interrupt context and can only bump an
//! atomic, ADR-080); the driver owns the other half of the evidence, how many commands it completed
//! through the platform's wait and how many by spinning. A run is proof only when the two agree.

use core::sync::atomic::{AtomicU64, Ordering};

/// One reading of a source's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqSample {
    /// Interrupts the handler took from this source.
    pub interrupts: u64,
    /// Times the completion wait really halted the CPU (`hlt` / `wfi`) because nothing was ready.
    pub sleeps: u64,
    /// Sleeps the bounding timer ended instead of the device (a dead or unarmed interrupt).
    pub bound_wakes: u64,
}

/// A platform's counters for one interrupt source. `const`-constructible, so a `static` owns it.
pub struct IrqStats {
    interrupts: AtomicU64,
    sleeps: AtomicU64,
    bound_wakes: AtomicU64,
}

impl IrqStats {
    pub const fn new() -> Self {
        IrqStats {
            interrupts: AtomicU64::new(0),
            sleeps: AtomicU64::new(0),
            bound_wakes: AtomicU64::new(0),
        }
    }
    /// The handler took one interrupt.
    pub fn interrupt(&self) {
        self.interrupts.fetch_add(1, Ordering::Relaxed);
    }
    /// The wait halted the CPU once.
    pub fn sleep(&self) {
        self.sleeps.fetch_add(1, Ordering::Relaxed);
    }
    /// The bounding timer, not the device, ended a sleep.
    pub fn bound_wake(&self) {
        self.bound_wakes.fetch_add(1, Ordering::Relaxed);
    }
    pub fn sample(&self) -> IrqSample {
        IrqSample {
            interrupts: self.interrupts.load(Ordering::Relaxed),
            sleeps: self.sleeps.load(Ordering::Relaxed),
            bound_wakes: self.bound_wakes.load(Ordering::Relaxed),
        }
    }
}

impl Default for IrqStats {
    fn default() -> Self {
        Self::new()
    }
}

/// How the suite reads its platform's counters. A platform's reading first lets any interrupt
/// that is already pending be taken, so a sample never misses one on the wire.
pub type IrqProbe<'a> = &'a dyn Fn() -> IrqSample;

/// Why a measured run does not prove interrupt-driven completions. Each is named so a failing
/// boot says which property broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunFault {
    /// Some command completed by spinning (or the driver never used the platform wait).
    SilentlyPolled,
    /// No interrupt arrived at all although every command completed: the source is not armed.
    NoInterrupt,
    /// Fewer interrupts than commands: a completion raised none (or its status was not cleared).
    LostInterrupt,
    /// More interrupts than commands: a status left set, or an interrupt storm.
    ExtraInterrupts,
    /// A sleep ended by the bounding timer: the device's answer did not wake the CPU.
    BoundWake,
    /// The CPU never halted: the wait returned without sleeping every time.
    NeverSlept,
}

impl RunFault {
    pub const fn name(self) -> &'static str {
        match self {
            RunFault::SilentlyPolled => "a command completed by polling, not through the wait",
            RunFault::NoInterrupt => "completions arrived but no interrupt did (source not armed)",
            RunFault::LostInterrupt => "fewer interrupts than completed commands (one was lost)",
            RunFault::ExtraInterrupts => {
                "more interrupts than completed commands (status not acknowledged)"
            }
            RunFault::BoundWake => "a sleep was ended by the bounding timer, not by the device",
            RunFault::NeverSlept => "the CPU never halted waiting for a completion",
        }
    }
}

/// Judge one measured run of `commands` commands: the driver's own counters over the run
/// (`slept` = commands completed through the platform wait, `polled` = by spinning) against the
/// platform's counters before and after. Exact equality, not a threshold: a thousand commands
/// with nine hundred interrupts is a bug, not noise.
pub fn judge_run(
    commands: u64,
    slept: u64,
    polled: u64,
    before: IrqSample,
    after: IrqSample,
) -> Result<(), RunFault> {
    let interrupts = after.interrupts.wrapping_sub(before.interrupts);
    if polled != 0 || slept != commands {
        return Err(RunFault::SilentlyPolled);
    }
    if interrupts == 0 && commands != 0 {
        return Err(RunFault::NoInterrupt);
    }
    if interrupts < commands {
        return Err(RunFault::LostInterrupt);
    }
    if interrupts > commands {
        return Err(RunFault::ExtraInterrupts);
    }
    if after.bound_wakes != before.bound_wakes {
        return Err(RunFault::BoundWake);
    }
    if commands != 0 && after.sleeps == before.sleeps {
        return Err(RunFault::NeverSlept);
    }
    Ok(())
}

/// The one boot-log line every platform and driver prints for how completions are waited for, so
/// gates can pin it and a fallback to polling is never silent. `reason` is `Some` only when the
/// source could not be armed.
pub fn completion_line(
    tag: &str,
    how: &str,
    reason: Option<&str>,
    s: IrqSample,
    out: &mut dyn FnMut(&str),
) {
    use core::fmt::Write;
    let mut line = alloc::string::String::new();
    let _ = match reason {
        None => write!(
            line,
            "[{}] completions: interrupt-driven ({}), {} interrupts, {} sleeps, {} bound wakes",
            tag, how, s.interrupts, s.sleeps, s.bound_wakes
        ),
        Some(why) => write!(line, "[{}] completions: POLLED, fallback 1 ({})", tag, why),
    };
    out(&line);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(interrupts: u64, sleeps: u64, bound_wakes: u64) -> IrqSample {
        IrqSample {
            interrupts,
            sleeps,
            bound_wakes,
        }
    }

    #[test]
    fn a_clean_run_is_accepted() {
        assert_eq!(judge_run(10, 10, 0, at(5, 1, 0), at(15, 11, 0)), Ok(()));
    }

    #[test]
    fn each_way_to_fail_is_named_and_distinct() {
        let z = at(0, 0, 0);
        assert_eq!(
            judge_run(10, 0, 10, z, at(10, 10, 0)),
            Err(RunFault::SilentlyPolled)
        );
        assert_eq!(
            judge_run(10, 10, 0, z, at(0, 10, 0)),
            Err(RunFault::NoInterrupt)
        );
        assert_eq!(
            judge_run(10, 10, 0, z, at(9, 10, 0)),
            Err(RunFault::LostInterrupt)
        );
        assert_eq!(
            judge_run(10, 10, 0, z, at(11, 10, 0)),
            Err(RunFault::ExtraInterrupts)
        );
        assert_eq!(
            judge_run(10, 10, 0, z, at(10, 10, 1)),
            Err(RunFault::BoundWake)
        );
        assert_eq!(
            judge_run(10, 10, 0, z, at(10, 0, 0)),
            Err(RunFault::NeverSlept)
        );
    }

    #[test]
    fn counters_count() {
        let s = IrqStats::new();
        s.interrupt();
        s.interrupt();
        s.sleep();
        s.bound_wake();
        assert_eq!(s.sample(), at(2, 1, 1));
    }
}
