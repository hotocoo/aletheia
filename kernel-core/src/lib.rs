//! Aletheia kernel core — the arch-independent contracts every bare-metal kernel target shares.
//!
//! Aletheia is its OWN operating system. This crate holds what is identical across CPU targets and
//! carries NO architecture, IO, or OS-import baggage:
//!
//! * the `Hal` trait (ADR-019) — the hardware primitives each backend provides;
//! * the capability-secure [`spine`] — the content-addressed store, unforgeable capability engine,
//!   intent→action pipeline, and secure IPC — a pure `no_std`+`alloc` reification of the M1 System
//!   Core with NO architecture dependency; and
//! * the [`selftest`] invariant suite — the M1 acceptance criteria as an arch-independent function
//!   that reports each check through a caller-supplied logger (so the core owns the invariant logic
//!   and its naming, while each target owns only the console presentation).
//!
//! Each target crate (`kernel/` aarch64, `kernel-x86_64/`, `kernel-riscv64/`) depends on this crate
//! and provides its OWN backend `impl Hal` plus its own console — so the trait, the spine, and the
//! invariant suite are each defined exactly once instead of being `#[path]`-copied per target
//! (gap-register Issue 1: core kernel abstractions are not duplicated across architecture crates).
//! It imports no Linux/macOS/Darwin/POSIX; the CPU architecture is a hardware target, Rust is the
//! implementation language. Because the spine is arch-independent, its invariants are also proved on
//! the host in this crate's own `cargo test` (`tests/invariants.rs`) — fast, no QEMU required —
//! complementing the per-target QEMU VM gates (Issue 1 acceptance: arch-independent invariants run
//! in hosted tests).
#![no_std]

extern crate alloc;

pub mod grant;
pub mod ipc;
pub mod priosched;
pub mod sched;
pub mod selftest;
pub mod spine;

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
