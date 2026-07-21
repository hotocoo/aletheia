//! NS16550A UART driver for the QEMU 'virt' machine (MMIO at 0x1000_0000, byte-wide registers).
//! Polled, blocking, single-core — the kernel's diagnostic output channel. Driven DIRECTLY rather
//! than through an SBI console call so output is robust regardless of which SBI console extensions
//! the firmware enables (OpenSBI has finished its own boot prints and released the UART by handoff).
//! The SBI path is still exercised — see `sbi::probe` — to prove the S->M firmware interface works.
use core::fmt::{self, Write};

const UART0_BASE: usize = 0x1000_0000;
const UART_THR: usize = 0x00; // transmit holding register (write)
const UART_LSR: usize = 0x05; // line status register
const LSR_THRE: u8 = 1 << 5; // transmit holding register empty

pub fn putc(byte: u8) {
    unsafe {
        let lsr = (UART0_BASE + UART_LSR) as *const u8;
        while core::ptr::read_volatile(lsr) & LSR_THRE == 0 {}
        core::ptr::write_volatile((UART0_BASE + UART_THR) as *mut u8, byte);
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

struct Console;
impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    let _ = Console.write_fmt(args);
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => ($crate::console::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! kprintln {
    () => ($crate::console::puts("\n"));
    ($($arg:tt)*) => ({ $crate::console::_print(format_args!($($arg)*)); $crate::console::puts("\n"); });
}
