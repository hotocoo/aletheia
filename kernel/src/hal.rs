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
//! Per ADR-010 (no blind hardware code), the x86-64 and RISC-V backends are declared here as the
//! contract they must satisfy but are `cfg`-gated to their own targets — they are NOT compiled into
//! the VM-tested aarch64 build, so no untested bring-up code ships. They are implemented, behind
//! THIS SAME trait, when each is brought up in a VM.

/// The arch-independent primitives the Aletheia kernel needs from hardware. Every target backend
/// implements this; the kernel is written against the trait, never against a specific CPU. These
/// are associated functions (no `self`) so the active backend is zero-cost and statically selected.
pub trait Hal {
    /// Human-readable name of the active target backend.
    fn arch_name() -> &'static str;
    /// Monotonic hardware tick counter (for latency measurement).
    fn timer_ticks() -> u64;
    /// Tick frequency in Hz.
    fn timer_freq_hz() -> u64;
    /// Convert ticks to nanoseconds using the backend's own frequency.
    fn ticks_to_ns(ticks: u64) -> u64;
    /// Current CPU privilege level, as a small integer (backend-defined meaning).
    fn current_privilege() -> u64;
    /// Halt the machine with an exit status (VM/firmware-defined). Diverges.
    fn exit(code: i32) -> !;
}

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

// --- AMD64 / x86-64: FIRST-CLASS production target (declared; implemented in P4/P5, VM-tested) ---
// Brought up behind THIS trait: local-APIC/HPET/TSC timer, ring/CPL privilege, serial console, and a
// firmware `exit` (QEMU isa-debug-exit / ACPI). cfg-gated to x86_64 so nothing untested ships in the
// aarch64 build (ADR-010).
#[cfg(target_arch = "x86_64")]
pub struct Amd64Hal;
#[cfg(target_arch = "x86_64")]
pub type ActiveHal = Amd64Hal;

// --- RISC-V (riscv64): FIRST-CLASS production target (declared; implemented in P4/P5, VM-tested) ---
// Brought up behind THIS trait: `rdtime`/CLINT timer, S/M privilege (`mstatus`), SBI/UART console,
// and an SBI `exit`. cfg-gated to riscv64.
#[cfg(target_arch = "riscv64")]
pub struct Riscv64Hal;
#[cfg(target_arch = "riscv64")]
pub type ActiveHal = Riscv64Hal;
