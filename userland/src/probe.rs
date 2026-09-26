//! `probe CASE` (ADR-207): calls `SYS_FS_READ` the way its argument names and exits with what the
//! kernel returned, so the boot suite can check each refusal. Not seeded; embedded by the boot
//! suite only.
#![no_std]
#![no_main]

mod mem;
mod sys;

static NOTE: &[u8] = b"note";

/// The entry point the kernel jumps to.
///
/// # Safety
/// Only the kernel calls this, with `args` pointing at `len` readable bytes (ADR-206).
#[no_mangle]
#[link_section = ".text._start"]
pub unsafe extern "C" fn _start(args: *const u8, len: usize) -> ! {
    // SAFETY: the kernel placed `len` argument bytes at `args`, on this program's stack page.
    let case = unsafe { core::slice::from_raw_parts(args, len) };
    let code_page = (_start as *const () as u64) & !0xfff;
    let mut buf = [0u8; 64];
    let stack_buf = buf.as_mut_ptr() as u64;
    let name = NOTE.as_ptr() as u64;
    let r = match case {
        // A good read: `note` into a stack buffer; prints what arrived.
        b"read" => {
            let r = sys::read_raw(name, 4, stack_buf, 64);
            if r != u64::MAX {
                let _ = sys::write(&buf[..(r as usize).min(64)]);
            }
            r
        }
        // A buffer in the program's own read+execute code page.
        b"codebuf" => sys::read_raw(name, 4, code_page + 0x800, 16),
        // A name straddling the code and stack pages.
        b"straddle" => sys::read_raw(code_page + 0xffe, 4, stack_buf, 16),
        // A name outside both pages.
        b"outside" => sys::read_raw(0x1000, 4, stack_buf, 16),
        // No name at all.
        b"noname" => sys::read_raw(name, 0, stack_buf, 16),
        // A buffer running past the stack page's end.
        b"pastend" => sys::read_raw(name, 4, stack_buf, 0x2000),
        // The served path allocates nothing per read (ADR-086 storm discipline).
        b"loop16" => reads(16, name, stack_buf),
        b"loop256" => reads(256, name, stack_buf),
        _ => 7,
    };
    sys::exit(r)
}

/// `count` good reads of `note`; the last one's result.
fn reads(count: u32, name: u64, buf: u64) -> u64 {
    let mut r = 0;
    for _ in 0..count {
        r = sys::read_raw(name, 4, buf, 64);
    }
    r
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys::exit(u64::MAX)
}
