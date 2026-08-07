//! NS16550A UART driver for the QEMU 'virt' machine (MMIO at 0x1000_0000, byte-wide registers).
//! Polled, blocking, single-core — the kernel's diagnostic output channel. Driven DIRECTLY rather
//! than through an SBI console call so output is robust regardless of which SBI console extensions
//! the firmware enables (OpenSBI has finished its own boot prints and released the UART by handoff).
//! The SBI path is still exercised — see `sbi::probe` — to prove the S->M firmware interface works.
use core::fmt::{self, Write};

const UART0_BASE: usize = 0x1000_0000;
const UART_THR: usize = 0x00; // transmit holding register (write)
const UART_RBR: usize = 0x00; // receive buffer register (read)
const UART_LSR: usize = 0x05; // line status register
const LSR_DR: u8 = 1 << 0; // receive data ready
const LSR_THRE: u8 = 1 << 5; // transmit holding register empty

/// One byte from the console, or `None` when nothing has been typed (REQ-CON-001).
///
/// Non-blocking: the interactive loop owns the waiting, so a gate can never wedge on input that
/// will not arrive. Read directly rather than through SBI's console extension for the same reason
/// `putc` is — the firmware has released the UART by handoff, and this works regardless of which
/// SBI extensions it chose to enable.
pub fn getc() -> Option<u8> {
    unsafe {
        let lsr = (UART0_BASE + UART_LSR) as *const u8;
        if core::ptr::read_volatile(lsr) & LSR_DR == 0 {
            return None;
        }
        Some(core::ptr::read_volatile(
            (UART0_BASE + UART_RBR) as *const u8,
        ))
    }
}

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

/// Interrupt-enable register: which UART conditions raise an interrupt.
#[cfg(feature = "interactive")]
const UART_IER: usize = 0x01;

/// Ask the UART to interrupt when input arrives (REQ-CON-002). Receive-data-available only: the
/// transmitter stays polled, because `puts` is synchronous and an interrupt there would buy nothing
/// but a second concurrency problem.
#[cfg(feature = "interactive")]
pub fn rx_interrupt_enable() {
    // SAFETY: IER is a byte-wide Device register in the identity-mapped UART window.
    unsafe { core::ptr::write_volatile((UART0_BASE + UART_IER) as *mut u8, 0x01) };
}

/// Take one byte if the receive register holds one. Same read as `getc`; named apart because the
/// interrupt handler drains a burst with it while `getc` is the polled fallback. Reading the byte is
/// also what deasserts the UART's interrupt — there is no separate acknowledge.
pub fn rx_take() -> Option<u8> {
    getc()
}
