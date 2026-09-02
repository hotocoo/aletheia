//! 8259A PIC — remapped so hardware IRQs land on vectors 0x20..0x2F instead of colliding with the
//! CPU exception vectors (0x00..0x1F). Only IRQ0 (the PIT timer) is unmasked; everything else is
//! masked for this boot-run-exit reference kernel. `io_wait` gives the legacy PICs settle time.

use x86_64::instructions::port::Port;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;
const EOI: u8 = 0x20;

fn io_wait() {
    // Write to an unused port (0x80) to burn a short, bus-bounded delay.
    unsafe { Port::<u8>::new(0x80).write(0u8) }
}

pub fn init() {
    unsafe {
        // ICW1: begin init, expect ICW4.
        Port::<u8>::new(PIC1_CMD).write(0x11u8);
        io_wait();
        Port::<u8>::new(PIC2_CMD).write(0x11u8);
        io_wait();
        // ICW2: vector offsets — master 0x20, slave 0x28.
        Port::<u8>::new(PIC1_DATA).write(0x20u8);
        io_wait();
        Port::<u8>::new(PIC2_DATA).write(0x28u8);
        io_wait();
        // ICW3: master has a slave on IRQ2; slave cascade identity 2.
        Port::<u8>::new(PIC1_DATA).write(0x04u8);
        io_wait();
        Port::<u8>::new(PIC2_DATA).write(0x02u8);
        io_wait();
        // ICW4: 8086/88 mode.
        Port::<u8>::new(PIC1_DATA).write(0x01u8);
        io_wait();
        Port::<u8>::new(PIC2_DATA).write(0x01u8);
        io_wait();
        // Masks: unmask only IRQ0 (timer) on the master; mask all slave lines.
        Port::<u8>::new(PIC1_DATA).write(0xFEu8);
        Port::<u8>::new(PIC2_DATA).write(0xFFu8);
    }
}

/// Mask IRQ0 (the PIT) on the master PIC (REQ-CON-002).
///
/// The boot leaves the PIT free-running for the ring-3 preemption suite. By the time a human is at
/// the console those suites are long finished, and an unmasked timer would fire thousands of times a
/// second into a handler the console has no use for — the session would spend its life in interrupt
/// entry instead of reading the line. Masking is read-modify-write for the same reason `unmask_serial`
/// is: neither may clobber the other's bit.
#[cfg(feature = "interactive")]
pub fn mask_timer() {
    unsafe {
        let cur = Port::<u8>::new(PIC1_DATA).read();
        Port::<u8>::new(PIC1_DATA).write(cur | 1);
    }
}

/// Unmask IRQ0 (PIT) on the master PIC, keeping every other bit (ADR-080). The live desktop is
/// pumped from the timer tick, so a machine that installed one KEEPS its tick through the console
/// instead of quieting it; read-modify-write for the same reason `mask_timer` is.
#[cfg(feature = "interactive")]
pub fn unmask_timer() {
    unsafe {
        let cur = Port::<u8>::new(PIC1_DATA).read();
        Port::<u8>::new(PIC1_DATA).write(cur & !1);
    }
}

/// Unmask IRQ4 (COM1) on the master PIC, keeping whatever else is already unmasked (REQ-CON-002).
/// Read-modify-write rather than a fresh mask byte: clobbering the timer's IRQ0 here would stop
/// preemption as a side effect of turning on the console.
#[cfg(feature = "interactive")]
pub fn unmask_serial() {
    unsafe {
        let cur = Port::<u8>::new(PIC1_DATA).read();
        Port::<u8>::new(PIC1_DATA).write(cur & !(1 << 4));
    }
}

/// Unmask IRQ1 (the i8042 keyboard) on the master PIC, keeping whatever else is already unmasked
/// (REQ-CON-003, ADR-049). Read-modify-write for the same reason `unmask_serial` is: the console has
/// two input sources now, and arming one must not disarm the other.
#[cfg(feature = "interactive")]
pub fn unmask_keyboard() {
    unsafe {
        let cur = Port::<u8>::new(PIC1_DATA).read();
        Port::<u8>::new(PIC1_DATA).write(cur & !(1 << 1));
    }
}

/// Is master-PIC line `irq` currently masked? Read rather than remembered: the mask register is the
/// authority on whether a line can reach the CPU, and a suite asserting its own bookkeeping instead
/// would pass while the hardware disagreed.
pub fn irq_masked(irq: u8) -> bool {
    unsafe { Port::<u8>::new(PIC1_DATA).read() & (1 << irq) != 0 }
}

/// End-of-interrupt. For slave-line vectors (>= 0x28) both PICs must be acknowledged.
pub fn eoi(vector: u8) {
    unsafe {
        if vector >= 0x28 {
            Port::<u8>::new(PIC2_CMD).write(EOI);
        }
        Port::<u8>::new(PIC1_CMD).write(EOI);
    }
}
