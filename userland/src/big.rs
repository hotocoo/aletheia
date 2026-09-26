//! `big` (ADR-211): a program whose code does not fit in one page. It is padded past 4 KiB by a
//! large `const` table it actually sums, so the machine must map several code pages for it to run
//! at all, and it exits with a value only the whole table produces.
#![no_std]
#![no_main]

mod mem;
mod sys;

/// 2,048 words, 8 KiB of table in `.rodata` (which the linker places in the code segment), so the
/// program is three pages of code whatever the compiler does with the loop.
const TABLE: [u32; 2048] = {
    let mut t = [0u32; 2048];
    let mut i = 0;
    while i < 2048 {
        t[i] = (i as u32).wrapping_mul(2_654_435_761);
        i += 1;
    }
    t
};

/// The entry point the kernel jumps to.
///
/// # Safety
/// Only the kernel calls this, with `args` pointing at `len` readable bytes (ADR-206).
#[no_mangle]
#[link_section = ".text._start"]
pub unsafe extern "C" fn _start(_args: *const u8, _len: usize) -> ! {
    let mut sum = 0u32;
    let mut i = 0;
    while i < TABLE.len() {
        sum = sum.wrapping_add(TABLE[i]);
        i += 1;
    }
    let _ = sys::write(b"big ran\n");
    sys::exit(sum as u64)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys::exit(u64::MAX)
}
