//! `hello`, in Rust (ADR-205): says so, sums 1..=10, exits with the sum.
#![no_std]
#![no_main]

mod sys;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    let _ = sys::write(b"hello from user mode\n");
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
