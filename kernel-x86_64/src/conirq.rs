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
use kernel_core::keymap::Keymap;

use crate::serial;

/// Bytes the device has handed us that the console has not read yet.
static mut RING: ConsoleRing = ConsoleRing::new();

/// Whether `init` ran. Until it has, IRQ4 is masked at the PIC, so the handler should never fire;
/// if it somehow does, there is nothing to drain and nothing to record.
static mut ARMED: bool = false;

/// Whether the i8042 came up. Separate from `ARMED` on purpose: a machine with no keyboard is a
/// normal machine, and its console must still work over the wire, so the two input sources are
/// armed independently and either one alone is a working console.
static mut KEYBOARD_ARMED: bool = false;

/// Scancode decoding state (REQ-CON-003, ADR-049). Lives beside the ring rather than inside the
/// keymap module because it is per-DEVICE state — modifiers are held by a particular keyboard — and
/// `kernel_core::keymap` is the arch-independent meaning of a code, not this machine's keyboard.
static mut KEYS: Keymap = Keymap::new();

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

    // The second input source (REQ-CON-003, ADR-049): the machine's own keyboard. Brought up BEFORE
    // `sti`, because the controller bring-up is a command/response conversation and an interrupt
    // taken in the middle of it would consume a reply the driver is waiting for. A failure here is
    // reported and survived — a console with one input source is the console this OS already had.
    match crate::ps2::init() {
        Ok(kb) => {
            crate::pic::unmask_keyboard();
            // SAFETY: as ARMED — written once, before IRQ1 is unmasked at the PIC.
            unsafe { KEYBOARD_ARMED = true };
            kprintln!(
                "[console] keyboard: i8042 up ({}), id {:02x?}, scancode set 1 via controller translation",
                crate::acpi::i8042_provenance(),
                &kb.identity[..kb.identity_len]
            );
        }
        Err(e) => kprintln!(
            "[console] keyboard: unavailable — {}; the serial line is still the input",
            crate::ps2::describe(e)
        ),
    }

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
/// Called from the IRQ1 handler: take every scancode the controller is holding, decode it, and push
/// whatever bytes fall out into the SAME ring COM1 feeds.
///
/// That sharing is the whole design. The line editor, its refusals and the ring's overflow policy
/// are written once against a byte stream; a keyboard that arrived through its own path would need
/// its own editor, and the two would drift. What is keyboard-specific ends here, at the keymap.
pub fn on_keyboard_irq() {
    // SAFETY: single-core; KEYBOARD_ARMED is written once before IRQ1 is unmasked.
    if !unsafe { KEYBOARD_ARMED } {
        // Drain anyway: a scancode left in the output buffer blocks every later one, so an
        // unexpected interrupt must not be able to wedge the controller.
        while crate::ps2::take_scancode().is_some() {}
        return;
    }
    while let Some(code) = crate::ps2::take_scancode() {
        // SAFETY: the handler is the only producer and cannot be re-entered (IF stays clear until
        // `iretq`), so no other reference to KEYS or RING exists here.
        let keys = unsafe { (*core::ptr::addr_of_mut!(KEYS)).feed(code) };
        // A navigation key is a SEQUENCE (REQ-CON-004, ADR-050), so it is offered to the ring as
        // one unit: `push_seq` admits all of it or none of it. A half-admitted arrow would leave
        // the editor's parser waiting for a final byte and eat the next real keystroke.
        if !keys.is_empty() {
            unsafe {
                (*core::ptr::addr_of_mut!(RING)).push_seq(keys.as_slice());
            }
        }
    }
}

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
