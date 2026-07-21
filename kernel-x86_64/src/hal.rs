//! Aletheia HAL — the AMD64/x86-64 backend (ADR-019 first-class target).
//!
//! Implements the SAME arch-independent `Hal` contract the aarch64 bootstrap backend does
//! (`kernel/src/hal.rs`); the trait is duplicated here rather than shared to keep the two kernel
//! crates independent while the aarch64 build stays untouched (the workspace/`kernel-core`
//! extraction that unifies this one trait is the documented mechanical follow-up). x86-64 realizes
//! the primitives with `rdtsc` (monotonic ticks), the CS RPL (privilege), and the QEMU/firmware exit.

use core::arch::asm;

/// The active backend implements the shared `kernel_core::Hal` contract (defined once, not per crate).
pub use kernel_core::Hal;

pub struct Amd64Hal;

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
        0 // TSC is uncalibrated in M1; the periodic tick source is `pit::FREQ_HZ` (100 Hz).
    }

    fn ticks_to_ns(ticks: u64) -> u64 {
        ticks // Uncalibrated passthrough in M1 (TSC-frequency calibration is a P5 concern).
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
