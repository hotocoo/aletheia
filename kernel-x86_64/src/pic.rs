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

/// End-of-interrupt. For slave-line vectors (>= 0x28) both PICs must be acknowledged.
pub fn eoi(vector: u8) {
    unsafe {
        if vector >= 0x28 {
            Port::<u8>::new(PIC2_CMD).write(EOI);
        }
        Port::<u8>::new(PIC1_CMD).write(EOI);
    }
}
