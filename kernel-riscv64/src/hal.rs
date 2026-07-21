//! Aletheia HAL — the RISC-V (RV64GC) backend (ADR-019 first-class target).
//!
//! Implements the SAME arch-independent `Hal` contract the aarch64 bootstrap backend does
//! (`kernel/src/hal.rs`); the trait is duplicated here rather than shared to keep the kernel crates
//! independent while the aarch64 build stays untouched (the workspace/`kernel-core` extraction that
//! unifies this one trait across all three backends is the documented mechanical follow-up). RISC-V
//! realizes the primitives with `rdtime` (monotonic ticks), the SBI-guaranteed S-mode privilege, and
//! the SiFive-test firmware exit.

/// The active backend implements the shared `kernel_core::Hal` contract (defined once, not per crate).
pub use kernel_core::Hal;

pub struct Riscv64Hal;

impl Hal for Riscv64Hal {
    fn arch_name() -> &'static str {
        "riscv64 / RV64GC (S-mode; QEMU virt + OpenSBI)"
    }

    fn timer_ticks() -> u64 {
        crate::arch::rdtime()
    }

    fn timer_freq_hz() -> u64 {
        crate::arch::TIMEBASE_HZ
    }

    fn ticks_to_ns(ticks: u64) -> u64 {
        crate::arch::ticks_to_ns(ticks)
    }

    fn current_privilege() -> u64 {
        // RISC-V privilege encoding: U=0, S=1, M=3. The current mode is not directly readable from
        // S-mode (mstatus is M-mode only), but OpenSBI's handoff contract guarantees the kernel
        // enters and runs in S-mode (Supervisor) — the intended kernel privilege on this target.
        1
    }

    fn exit(code: i32) -> ! {
        crate::exit::exit(code)
    }
}

/// The backend selected for this build target; the kernel refers to `ActiveHal`, never a CPU.
pub type ActiveHal = Riscv64Hal;
