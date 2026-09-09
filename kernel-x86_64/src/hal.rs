//! Aletheia HAL — the AMD64/x86-64 backend (ADR-019 first-class target).
//!
//! Implements the SAME arch-independent `Hal` contract the aarch64 bootstrap backend does
//! (`kernel/src/hal.rs`); the trait is duplicated here rather than shared to keep the two kernel
//! crates independent while the aarch64 build stays untouched (the workspace/`kernel-core`
//! extraction that unifies this one trait is the documented mechanical follow-up). x86-64 realizes
//! the primitives with `rdtsc` (monotonic ticks), the CS RPL (privilege), and the QEMU/firmware exit.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// The active backend implements the shared `kernel_core::Hal` contract (defined once, not per crate).
pub use kernel_core::Hal;

pub struct Amd64Hal;

static TSC_HZ: AtomicU64 = AtomicU64::new(0);

impl Hal for Amd64Hal {
    fn arch_name() -> &'static str {
        "x86_64 / AMD64 (UEFI; QEMU q35 + OVMF, VMware)"
    }

    fn timer_ticks() -> u64 {
        let lo: u32;
        let hi: u32;
        // SAFETY: rdtsc has no memory effects; reads the 64-bit timestamp counter into edx:eax.
        unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
        ((hi as u64) << 32) | (lo as u64)
    }

    fn timer_freq_hz() -> u64 {
        TSC_HZ.load(Ordering::Relaxed)
    }

    fn ticks_to_ns(ticks: u64) -> u64 {
        let hz = Self::timer_freq_hz();
        if hz == 0 {
            ticks
        } else {
            ticks.saturating_mul(1_000_000_000).saturating_div(hz)
        }
    }

    fn current_privilege() -> u64 {
        let cs: u16;
        // SAFETY: reads the CS selector; its low two bits are the current privilege level (CPL).
        unsafe { asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack)) };
        (cs & 0b11) as u64 // 0 = ring 0 (kernel)
    }

    fn exit(code: i32) -> ! {
        crate::exit::exit(code)
    }
}

/// The backend selected for this build target; the kernel refers to `ActiveHal`, never a CPU.
pub type ActiveHal = Amd64Hal;

/// Calibrate TSC against the already-live PIT interrupt source. QEMU and real x86 hardware both
/// expose a TSC but do not guarantee its frequency through this kernel's existing contract; using
/// the PIT gives the benchmark an observed frequency instead of inventing one. Called once after
/// IRQ0 has been proved live. A short multi-tick window keeps interrupt jitter below the resolution
/// of the resulting frequency estimate while adding negligible boot cost.
pub fn calibrate_tsc() -> Option<u64> {
    // Prefer architectural CPUID.15 when firmware exposes it: waiting for ten PIT ticks costs
    // ~100 ms of boot latency solely to measure a counter whose ratio the CPU already reports.
    // Keep the timed path as a conservative fallback because older firmware/virtual CPUs may
    // advertise CPUID.15 without a usable crystal frequency.
    let leaf = core::arch::x86_64::__cpuid_count(0x15, 0);
    if leaf.eax != 0 && leaf.ebx != 0 && leaf.ecx != 0 {
        let hz = (leaf.ecx as u64)
            .saturating_mul(leaf.ebx as u64)
            .checked_div(leaf.eax as u64)
            .unwrap_or(0);
        if hz != 0 {
            TSC_HZ.store(hz, Ordering::Relaxed);
            return Some(hz);
        }
    }

    let start_tick = crate::pit::ticks();
    let start = ActiveHal::timer_ticks();
    // Two ticks are enough for the fallback's coarse monotonic conversion while keeping the
    // boot penalty around 20 ms instead of the previous ~100 ms. The benchmark does not use this
    // estimate to manufacture hardware performance claims; it only needs a local time base.
    let target = start_tick.saturating_add(2);
    while crate::pit::ticks() < target {
        x86_64::instructions::hlt();
    }
    let end = ActiveHal::timer_ticks();
    let delta = end.wrapping_sub(start);
    let hz = delta.saturating_mul(crate::pit::FREQ_HZ as u64) / 2;
    if hz == 0 {
        return None;
    }
    TSC_HZ.store(hz, Ordering::Relaxed);
    Some(hz)
}
