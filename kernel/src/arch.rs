//! Architecture primitives: the generic timer (for latency measurement) and the current
//! exception level (determines syscall-floor benchmark plumbing). aarch64-specific.
use core::arch::asm;

/// Generic-timer frequency in Hz (`CNTFRQ_EL0`). On QEMU 'virt' this is the virtual timer
/// frequency, so timer-derived nanoseconds are consistent *within this emulated CPU*.
#[inline]
pub fn cntfrq() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack)) };
    v
}

/// Monotonic virtual counter (`CNTVCT_EL0`). `isb` orders the read against surrounding work.
#[inline]
pub fn cntvct() -> u64 {
    let v: u64;
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nostack)) };
    v
}

/// Convert timer ticks to nanoseconds using the emulated CPU's own timer frequency.
#[inline]
pub fn ticks_to_ns(ticks: u64) -> u64 {
    let f = cntfrq().max(1);
    ((ticks as u128 * 1_000_000_000u128) / f as u128) as u64
}

/// Current exception level (0..3), from `CurrentEL[3:2]`.
#[inline]
pub fn current_el() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, CurrentEL", out(reg) v, options(nomem, nostack)) };
    (v >> 2) & 0b11
}
