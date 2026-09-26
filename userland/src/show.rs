//! `show NAME` (ADR-207): read an object from the namespace that started it and print it.
#![no_std]
#![no_main]

mod mem;
mod sys;

/// The entry point the kernel jumps to.
///
/// # Safety
/// Only the kernel calls this, with `args` pointing at `len` readable bytes (ADR-206).
#[no_mangle]
#[link_section = ".text._start"]
pub unsafe extern "C" fn _start(args: *const u8, len: usize) -> ! {
    // SAFETY: the kernel placed `len` argument bytes at `args`, on this program's stack page.
    let name = unsafe { core::slice::from_raw_parts(args, len) };
    if name.is_empty() {
        let _ = sys::write(b"usage: show NAME\n");
        sys::exit(1);
    }
    let mut buf = [0u8; 1024];
    match sys::read(name, &mut buf) {
        Ok(n) => {
            let _ = sys::write(&buf[..n.min(buf.len())]);
            if n > buf.len() {
                let _ = sys::write(b"\n(the rest did not fit)");
            }
            let _ = sys::write(b"\n");
            sys::exit(n as u64)
        }
        Err(()) => {
            let _ = sys::write(b"show: the namespace refused that name\n");
            sys::exit(2)
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys::exit(u64::MAX)
}
