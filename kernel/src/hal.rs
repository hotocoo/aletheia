//! Aletheia Hardware Abstraction Layer (HAL) — ADR-019.
//!
//! Aletheia is its OWN operating system. This layer abstracts *genuine hardware differences only*;
//! it does NOT import Linux, macOS/Darwin, POSIX, or any other OS's architecture — every
//! operating-system abstraction above the HAL belongs to Aletheia. Rust is the implementation
//! language; the CPU architecture is a hardware target.
//!
//! ```text
//! Aletheia Kernel
//!        │  (this HAL contract — arch-independent)
//! Aletheia HAL
//!        │
//! AMD64/x86-64  |  RISC-V   (first-class targets)   ·   aarch64 (bootstrap/dev target)
//! ```
//!
//! TARGET MATRIX:
//! - **AMD64 / x86-64** — first-class production target (P4/P5).
//! - **RISC-V (riscv64)** — first-class production target (P4/P5).
//! - **aarch64** — the bootstrap/dev target implemented today (QEMU `virt`); it exercises the HAL
//!   contract in a VM so the kernel above stays arch-independent.
//!
//! The `Hal` trait itself now lives ONCE in the `kernel-core` crate (this module re-exports it); each
//! target crate — `kernel/` (aarch64, here), `kernel-x86_64/`, `kernel-riscv64/` — provides its own
//! backend `impl Hal`. All three are executed and VM-tested (ADR-010: no blind hardware code). This
//! crate builds only for aarch64 (`.cargo/config.toml`); the x86-64/RISC-V backends live in theirs.

/// The active backend implements the shared `kernel_core::Hal` contract (defined once, not per crate).
pub use kernel_core::Hal;

// --- aarch64: the bootstrap/dev backend, implemented today and VM-tested ---
#[cfg(target_arch = "aarch64")]
pub struct Aarch64Hal;

#[cfg(target_arch = "aarch64")]
impl Hal for Aarch64Hal {
    fn arch_name() -> &'static str {
        "aarch64 (bootstrap/dev target — QEMU virt)"
    }
    fn timer_ticks() -> u64 {
        crate::arch::cntvct()
    }
    fn timer_freq_hz() -> u64 {
        crate::arch::cntfrq()
    }
    fn ticks_to_ns(ticks: u64) -> u64 {
        crate::arch::ticks_to_ns(ticks)
    }
    fn current_privilege() -> u64 {
        crate::arch::current_el()
    }
    fn exit(code: i32) -> ! {
        crate::semihosting::exit(code)
    }
}

/// The backend selected for the current build target. The kernel refers to `ActiveHal`, never a
/// concrete architecture — swapping the target swaps the backend with no change to the layers above.
#[cfg(target_arch = "aarch64")]
pub type ActiveHal = Aarch64Hal;

// The x86-64 and RISC-V backends now live in their own executed crates (`kernel-x86_64/`,
// `kernel-riscv64/`), each implementing this SAME `kernel_core::Hal` — no longer declared as stubs
// here. This crate builds only for aarch64 (see `.cargo/config.toml`).
