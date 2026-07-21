//! Aletheia kernel core — the arch-independent contracts every bare-metal kernel target shares.
//!
//! Aletheia is its OWN operating system. This crate holds what is identical across CPU targets and
//! carries NO architecture, IO, or OS-import baggage: today that is the `Hal` trait (ADR-019). Each
//! target crate (`kernel/` aarch64, `kernel-x86_64/`, `kernel-riscv64/`) depends on this crate and
//! provides its OWN backend `impl Hal` — so the trait is defined exactly once instead of being
//! copied per target. It imports no Linux/macOS/Darwin/POSIX; the CPU architecture is a hardware
//! target, Rust is the implementation language.
#![no_std]

/// The arch-independent hardware primitives the Aletheia kernel needs from a target backend. Every
/// target implements this for its own `…Hal` struct; the kernel is written against the trait, never
/// against a specific CPU. Associated functions (no `self`) so the active backend is statically
/// selected and zero-cost.
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
