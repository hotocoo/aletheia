//! `counter` (ADR-210): a program with real mutable globals. It counts in `.data` and `.bss`,
//! reads an object into a global buffer, and exits with what its counters say, so a run proves the
//! writable page is there, is writable, and starts as the image declared it.
#![no_std]
#![no_main]

mod mem;
mod sys;

/// `.data`: a global the image carries a non-zero value for.
static mut TICKS: u64 = 40;
/// `.bss`: a global the image only reserves room for, and the kernel must hand over zeroed.
static mut SEEN: u64 = 0;
/// `.bss` again, and where a read of the namespace lands.
static mut BUF: [u8; 256] = [0; 256];

/// The entry point the kernel jumps to.
///
/// # Safety
/// Only the kernel calls this, with `args` pointing at `len` readable bytes (ADR-206).
#[no_mangle]
#[link_section = ".text._start"]
pub unsafe extern "C" fn _start(args: *const u8, len: usize) -> ! {
    // SAFETY: single-threaded program; nothing else touches these globals.
    let (ticks, seen, buf) = unsafe {
        (
            &mut *core::ptr::addr_of_mut!(TICKS),
            &mut *core::ptr::addr_of_mut!(SEEN),
            &mut *core::ptr::addr_of_mut!(BUF),
        )
    };
    if *seen != 0 {
        // A .bss the kernel did not zero: say so rather than reporting a number built on it.
        let _ = sys::write(b"counter: .bss was not zero\n");
        sys::exit(u64::MAX);
    }
    for _ in 0..15 {
        *ticks += 1;
        *seen += 1;
    }
    // SAFETY: the kernel placed `len` argument bytes at `args`, on this program's stack page.
    let name = unsafe { core::slice::from_raw_parts(args, len) };
    if !name.is_empty() {
        match sys::read(name, buf) {
            Ok(n) => {
                let _ = sys::write(b"counter read: ");
                let _ = sys::write(&buf[..n.min(buf.len())]);
                let _ = sys::write(b"\n");
                *ticks += n as u64;
            }
            Err(()) => {
                let _ = sys::write(b"counter: the namespace refused that name\n");
                sys::exit(u64::MAX);
            }
        }
    }
    sys::exit(*ticks)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys::exit(u64::MAX)
}
