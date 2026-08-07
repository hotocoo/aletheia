//! Interrupt-driven console input on x86-64 (REQ-CON-002, ADR-045).
//!
//! The aarch64 sibling (`kernel/src/conirq.rs`) documents the model; the policy — a bounded ring
//! that drops the NEWEST byte and counts what it refused — lives once in `kernel_core::conring`.
//! What is this target's:
//!
//! * **COM1 raises IRQ4**, which the 8259A remaps to vector 0x24 (`idt::SERIAL_VECTOR`).
//! * **The FIFO trigger level is one byte** (`serial::init`), so a single keystroke raises the
//!   data-available interrupt immediately instead of waiting for the character-timeout.
//! * **EOI is the PIC's**, sent by the IDT stub after this module has drained the FIFO.
//!
//! **The PIT is why this is not a two-line change.** The boot leaves IRQ0 unmasked and the PIT
//! running, so the instant `sti` executes the timer fires — forever, thousands of times a second,
//! with a handler that exists for the ring-3 preemption suite and has nothing to do here. The
//! console would never make progress. So arming the console *masks IRQ0 first*: the suites that
//! need preemption have already finished by the time anyone types, and a console that shares its
//! core with a free-running timer it does not use is a console that spends its life in a handler.
//!
//! Concurrency: one core, two parties. The handler produces, the console loop consumes, and the loop
//! clears IF around its `pop` so the two never touch the ring at once. The handler cannot be
//! re-entered — the CPU clears IF on entry through an interrupt gate and nothing sets it before
//! `iretq`.
use kernel_core::conring::ConsoleRing;

use crate::serial;

/// Bytes the device has handed us that the console has not read yet.
static mut RING: ConsoleRing = ConsoleRing::new();

/// Whether `init` ran. Until it has, IRQ4 is masked at the PIC, so the handler should never fire;
/// if it somehow does, there is nothing to drain and nothing to record.
static mut ARMED: bool = false;

/// Turn on the console's interrupt: quiet the timer, enable the UART's receive bit, unmask IRQ4,
/// then set IF.
///
/// Order matters. Unmasking the console before silencing the PIT would still work; setting IF before
/// either would not — the timer would fire first and keep firing.
#[cfg(feature = "interactive")]
pub fn init() {
    crate::pic::mask_timer();
    serial::rx_interrupt_enable();
    crate::pic::unmask_serial();
    // SAFETY: single-core console; the vector is installed by `idt::init` before this runs.
    unsafe { ARMED = true };
    x86_64::instructions::interrupts::enable();
}

/// Take the oldest byte the interrupt collected, or `None`.
///
/// `without_interrupts` clears IF for the read and restores it after, so a byte arriving mid-`pop`
/// cannot race the indices being updated.
#[cfg(feature = "interactive")]
pub fn pop() -> Option<u8> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        // SAFETY: IF is clear and this is the only consumer, so no other reference to RING exists.
        unsafe { (*core::ptr::addr_of_mut!(RING)).pop() }
    })
}

/// How many bytes the ring refused because the console could not keep up.
#[cfg(feature = "interactive")]
pub fn dropped() -> u64 {
    x86_64::instructions::interrupts::without_interrupts(|| {
        // SAFETY: as `pop`.
        unsafe { (*core::ptr::addr_of!(RING)).dropped() }
    })
}

/// Called from the IRQ4 handler: move everything COM1 is holding into the ring.
///
/// Draining the whole FIFO matters because one interrupt can cover several bytes, and a handler that
/// took only the first would leave the rest until the next keystroke shook them loose. Unlike the
/// PL011 there is no separate condition to acknowledge: the interrupt is cleared by READING the
/// data, which is exactly what the drain does.
pub fn on_serial_irq() {
    // SAFETY: single-core; ARMED is written once before IRQ4 is unmasked.
    if !unsafe { ARMED } {
        return;
    }
    while let Some(b) = serial::rx_take() {
        // SAFETY: the handler is the only producer and cannot be re-entered (IF stays clear until
        // `iretq`), so no other reference to RING exists here.
        unsafe {
            (*core::ptr::addr_of_mut!(RING)).push(b);
        }
    }
}
