//! `hello`, in Rust (ADR-205): says so (greeting its arguments, ADR-206), sums 1..=10, exits with
//! the sum.
#![no_std]
#![no_main]

mod sys;

#[no_mangle]
#[link_section = ".text._start"]
/// The entry point the kernel jumps to.
///
/// # Safety
/// Only the kernel calls this, with `args` pointing at `len` readable bytes (ADR-206).
pub unsafe extern "C" fn _start(args: *const u8, len: usize) -> ! {
    let _ = sys::write(b"hello from user mode");
    if len > 0 {
        // SAFETY: the kernel placed `len` argument bytes at `args`, at the top of this program's
        // own stack page, before entering it.
        let args = unsafe { core::slice::from_raw_parts(args, len) };
        let _ = sys::write(b": ");
        let _ = sys::write(args);
    }
    let _ = sys::write(b"\n");
    let mut sum = 0u64;
    let mut i = core::hint::black_box(10u64);
    while i > 0 {
        sum += i;
        i -= 1;
    }
    sys::exit(sum)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys::exit(u64::MAX)
}
