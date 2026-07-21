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
        Port::<u8>::new(COM1 + 2).write(0xC7u8); // enable + clear FIFO, 14-byte threshold
        Port::<u8>::new(COM1 + 4).write(0x0Bu8); // RTS/DSR set, OUT2 (IRQ line) enabled
    }
}

fn transmit_empty() -> bool {
    // Line Status Register bit 5 (THR empty).
    unsafe { Port::<u8>::new(COM1 + 5).read() & 0x20 != 0 }
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
