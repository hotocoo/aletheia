//! Interrupt-driven console input on RISC-V (REQ-CON-002, ADR-045).
//!
//! The aarch64 sibling documents the model; the policy — a bounded ring that drops the NEWEST byte
//! and counts what it refused — lives once in `kernel_core::conring`. What is this target's:
//!
//! * **A PLIC, which had no driver before this.** QEMU `virt` puts it at `0x0C00_0000` and wires the
//!   NS16550A to source **10**. Unlike a GIC the PLIC is per-*context*: hart 0's S-mode is context 1,
//!   and enabling a source means setting its bit in that context's enable block, giving it a nonzero
//!   priority, and dropping that context's threshold below it. Miss any one and the line is silent.
//! * **Claim/complete instead of ack/EOI.** Reading the claim register returns the pending source
//!   AND deasserts it; writing the same number back says the handler is done. A claim that is never
//!   completed silently stops all further interrupts on that context.
//! * **Two enable bits above the controller**: `sie.SEIE` (supervisor external) and `sstatus.SIE`.
//!
//! **The UART must be drained before the source is completed, and completed unconditionally.** The
//! NS16550A asserts its line for as long as unread data sits in the receive register, so completing
//! a claim while a byte remains re-asserts immediately — which is fine — but *failing* to complete
//! leaves the context permanently deaf. Every path out of the handler writes the claim back.
//!
//! Concurrency: one core, two parties — the handler produces, the console loop consumes, and the
//! loop clears `sstatus.SIE` around its `pop`. The handler cannot be re-entered: the hardware clears
//! `SIE` on trap entry and nothing sets it before `sret`.
#[cfg(feature = "interactive")]
use core::arch::asm;

use kernel_core::conring::ConsoleRing;

use crate::console;

/// QEMU `virt`: the NS16550A is PLIC source 10.
const UART_SOURCE: u32 = 10;
/// This hart's S-mode PLIC context.
///
/// **Not a constant, and that is the whole point.** QEMU `virt` lays contexts out per hart —
/// `2N` is hart N's M-mode (OpenSBI's) and `2N+1` is its S-mode (ours) — and OpenSBI's boot-hart
/// lottery may hand us ANY hartid, which `boot.s` records in `BOOT_HART`. Hardcoding context 1
/// configured hart 0's context no matter which hart was actually running, so the console worked or
/// went deaf depending on a coin flip inside the firmware: the symptom was a session that answered a
/// couple of commands on one run and nothing at all on the next.
fn context() -> usize {
    extern "C" {
        static BOOT_HART: u64;
    }
    // SAFETY: `BOOT_HART` is written by `_start` before Rust runs and never again.
    let hart = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(BOOT_HART)) } as usize;
    2 * hart + 1
}

const PLIC_BASE: usize = 0x0C00_0000;
#[cfg(feature = "interactive")]
const PLIC_PRIORITY: usize = PLIC_BASE; // + 4 * source
#[cfg(feature = "interactive")]
const PLIC_ENABLE: usize = PLIC_BASE + 0x0000_2000; // + 0x80 * context
const PLIC_THRESHOLD: usize = PLIC_BASE + 0x0020_0000; // + 0x1000 * context
const PLIC_CLAIM: usize = PLIC_THRESHOLD + 4; // same stride as threshold

/// `scause` for a supervisor external interrupt: the interrupt bit plus cause 9.
pub const SCAUSE_S_EXTERNAL: usize = (1 << 63) | 9;

/// Bytes the device has handed us that the console has not read yet.
static mut RING: ConsoleRing = ConsoleRing::new();

/// Whether `init` ran. Until it has, the PLIC never routes source 10 here.
static mut ARMED: bool = false;

#[inline]
fn plic_w32(addr: usize, v: u32) {
    // SAFETY: the PLIC window is identity-mapped Device memory (vm.rs maps the peripheral region).
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) };
}

#[inline]
fn plic_r32(addr: usize) -> u32 {
    // SAFETY: as `plic_w32`.
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// Route the UART's PLIC source to this hart's S-mode context and open the interrupt path above it.
#[cfg(feature = "interactive")]
pub fn init() {
    // A source with priority 0 is never delivered, whatever else is configured.
    plic_w32(PLIC_PRIORITY + 4 * UART_SOURCE as usize, 1);
    let ctx = context();
    let enable = PLIC_ENABLE + 0x80 * ctx + (UART_SOURCE as usize / 32) * 4;
    plic_w32(enable, plic_r32(enable) | (1 << (UART_SOURCE % 32)));
    // Threshold is a strict floor: a source is delivered only if its priority is GREATER than it.
    plic_w32(PLIC_THRESHOLD + 0x1000 * ctx, 0);
    console::rx_interrupt_enable();
    // SAFETY: single-core console; the trap vector is installed before this runs.
    unsafe { ARMED = true };
    // SAFETY: setting sie.SEIE and sstatus.SIE only affects this hart's interrupt enables.
    unsafe {
        asm!("csrs sie, {}", in(reg) 1usize << 9, options(nomem, nostack));
        asm!("csrsi sstatus, 2", options(nomem, nostack));
    }
}

/// Clear `sstatus.SIE`, returning whether it had been set.
#[cfg(feature = "interactive")]
#[inline]
fn mask_irqs() -> bool {
    let prev: usize;
    // SAFETY: `csrrci` atomically clears the bit and returns the old value; hart-local.
    unsafe { asm!("csrrci {}, sstatus, 2", out(reg) prev, options(nomem, nostack)) };
    prev & 2 != 0
}

#[cfg(feature = "interactive")]
#[inline]
fn restore_irqs(were_enabled: bool) {
    if were_enabled {
        // SAFETY: hart-local interrupt enable.
        unsafe { asm!("csrsi sstatus, 2", options(nomem, nostack)) };
    }
}

/// Take the oldest byte the interrupt collected, or `None`.
#[cfg(feature = "interactive")]
pub fn pop() -> Option<u8> {
    let were = mask_irqs();
    // SAFETY: interrupts are masked and this is the only consumer, so no other reference exists.
    let byte = unsafe { (*core::ptr::addr_of_mut!(RING)).pop() };
    restore_irqs(were);
    byte
}

/// How many bytes the ring refused because the console could not keep up.
#[cfg(feature = "interactive")]
pub fn dropped() -> u64 {
    let were = mask_irqs();
    // SAFETY: as `pop`.
    let n = unsafe { (*core::ptr::addr_of!(RING)).dropped() };
    restore_irqs(were);
    n
}

/// Handle a supervisor external interrupt. Returns `true` if it was the console's — the trap handler
/// then returns to what it interrupted; `false` means nobody here claims it and the trap stays fatal.
pub fn on_external_irq() -> bool {
    // SAFETY: single-core; ARMED is written once before interrupts are enabled.
    if !unsafe { ARMED } {
        return false;
    }
    let claim_reg = PLIC_CLAIM + 0x1000 * context();
    let source = plic_r32(claim_reg);
    if source == 0 {
        // Nothing pending. Claiming zero owes no completion, and treating it as handled is right:
        // a spurious external interrupt is not a reason to kill the machine.
        return true;
    }
    if source != UART_SOURCE {
        // Complete it so the context is not wedged, but do not pretend it was handled.
        plic_w32(claim_reg, source);
        return false;
    }
    while let Some(b) = console::rx_take() {
        // SAFETY: the handler is the only producer and cannot be re-entered (SIE stays clear until
        // `sret`), so no other reference to RING exists here.
        unsafe {
            (*core::ptr::addr_of_mut!(RING)).push(b);
        }
    }
    plic_w32(claim_reg, source);
    true
}
