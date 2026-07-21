//! Architecture primitives: the S-mode time counter (for latency measurement) and its frequency.
//! riscv64-specific.
use core::arch::asm;

/// QEMU 'virt' timebase frequency (the `timebase-frequency` DTB property): 10 MHz. OpenSBI enables
/// `time` reads from S-mode (scounteren.TM), so `rdtime` below is legal and consistent within this
/// emulated CPU — the same "consistent within the emulated CPU" property the aarch64 CNTFRQ has.
pub const TIMEBASE_HZ: u64 = 10_000_000;

/// Monotonic time counter (`rdtime` == `csrr rd, time`). Readable from S-mode on QEMU/OpenSBI.
#[inline]
pub fn rdtime() -> u64 {
    let t: u64;
    // SAFETY: `rdtime` has no memory effects; it reads the 64-bit `time` CSR into a GPR.
    unsafe { asm!("rdtime {}", out(reg) t, options(nomem, nostack)) };
    t
}

/// Convert timer ticks to nanoseconds using the emulated CPU's own timebase.
#[inline]
pub fn ticks_to_ns(ticks: u64) -> u64 {
    ((ticks as u128 * 1_000_000_000u128) / TIMEBASE_HZ as u128) as u64
}
