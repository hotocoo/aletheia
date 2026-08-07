//! PL011 UART driver for the QEMU 'virt' machine (MMIO at 0x0900_0000).
//! Polled, blocking, single-core. This is the kernel's diagnostic output channel —
//! every boot/spine/benchmark line the e2e harness asserts on is emitted here.
use core::fmt::{self, Write};

const UART0_BASE: usize = 0x0900_0000;
const UART_DR: usize = 0x00; // data register
const UART_FR: usize = 0x18; // flag register
#[cfg(feature = "interactive")]
const FR_RXFE: u32 = 1 << 4; // receive FIFO empty
const FR_TXFF: u32 = 1 << 5; // transmit FIFO full

/// One byte from the console, or `None` when nothing has been typed (REQ-CON-001).
///
/// Non-blocking on purpose: the interactive loop owns the waiting, so a target can interleave other
/// work with reading, and a gate can never wedge on an input that will not arrive.
#[cfg(feature = "interactive")]
pub fn getc() -> Option<u8> {
    unsafe {
        let fr = (UART0_BASE + UART_FR) as *const u32;
        if core::ptr::read_volatile(fr) & FR_RXFE != 0 {
            return None;
        }
        // The low byte is the character; the upper bits are framing/parity/break error flags, which
        // a polled console reads past rather than treating as data.
        Some((core::ptr::read_volatile((UART0_BASE + UART_DR) as *const u32) & 0xff) as u8)
    }
}

pub fn putc(byte: u8) {
    unsafe {
        let fr = (UART0_BASE + UART_FR) as *const u32;
        while core::ptr::read_volatile(fr) & FR_TXFF != 0 {}
        core::ptr::write_volatile((UART0_BASE + UART_DR) as *mut u32, byte as u32);
    }
}

pub fn puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

struct Uart;
impl Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    let _ = Uart.write_fmt(args);
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => ($crate::uart::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! kprintln {
    () => ($crate::uart::puts("\n"));
    ($($arg:tt)*) => ({ $crate::uart::_print(format_args!($($arg)*)); $crate::uart::puts("\n"); });
}
