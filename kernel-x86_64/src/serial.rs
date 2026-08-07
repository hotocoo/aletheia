//! 16550 UART driver for COM1 (port 0x3F8) — the machine-checkable diagnostic channel.
//!
//! Direct port I/O, so it works BEFORE and AFTER `ExitBootServices` (independent of firmware
//! boot services). The x86-64 smoke test asserts on the lines emitted here; the framebuffer is
//! the human-visible mirror (what you watch boot in VMware). Polled, blocking, single-core.

use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

/// Program COM1 for 38400 baud, 8N1, FIFO enabled, interrupts off (we poll).
pub fn init() {
    unsafe {
        Port::<u8>::new(COM1 + 1).write(0x00u8); // disable UART interrupts
        Port::<u8>::new(COM1 + 3).write(0x80u8); // enable DLAB (set baud divisor)
        Port::<u8>::new(COM1).write(0x03u8); // divisor low  = 3 => 38400 baud
        Port::<u8>::new(COM1 + 1).write(0x00u8); // divisor high = 0
        Port::<u8>::new(COM1 + 3).write(0x03u8); // 8 bits, no parity, 1 stop; DLAB off
                                                 // Enable + clear the FIFOs, trigger at ONE byte: a console must react to a single
                                                 // keystroke, and a 14-byte threshold would make every short line wait for the
                                                 // character-timeout interrupt instead of the data-available one.
        Port::<u8>::new(COM1 + 2).write(0x07u8);
        Port::<u8>::new(COM1 + 4).write(0x0Bu8); // RTS/DSR set, OUT2 (IRQ line) enabled
    }
}

fn transmit_empty() -> bool {
    // Line Status Register bit 5 (THR empty).
    unsafe { Port::<u8>::new(COM1 + 5).read() & 0x20 != 0 }
}

/// One byte from the console, or `None` when nothing has been typed (REQ-CON-001).
///
/// Non-blocking: the interactive loop owns the waiting, so a gate can never wedge on input that
/// will not arrive. Line Status Register bit 0 is "data ready"; the byte is then in the receive
/// buffer at the base port.
pub fn getc() -> Option<u8> {
    // SAFETY: COM1's LSR and RBR are the ports `init` already programmed; reading them is
    // side-effect-free apart from consuming the received byte.
    unsafe {
        if Port::<u8>::new(COM1 + 5).read() & 0x01 == 0 {
            return None;
        }
        Some(Port::<u8>::new(COM1).read())
    }
}

/// Ask COM1 to raise IRQ4 when a byte arrives (REQ-CON-002). Only the receive-data-available bit:
/// the transmitter is still polled, because `puts` is synchronous and an interrupt there would buy
/// nothing but a second concurrency problem.
#[cfg(feature = "interactive")]
pub fn rx_interrupt_enable() {
    // SAFETY: IER is the port `init` already programmed; writing it only changes interrupt masking.
    unsafe { Port::<u8>::new(COM1 + 1).write(0x01u8) };
}

/// Take one byte if the receive FIFO holds one. Same read as `getc`; named apart because the
/// interrupt handler drains a burst with it while `getc` is the polled fallback.
pub fn rx_take() -> Option<u8> {
    getc()
}

pub fn putc(byte: u8) {
    while !transmit_empty() {}
    unsafe { Port::<u8>::new(COM1).write(byte) }
}

pub fn puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}
