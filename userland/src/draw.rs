//! `draw` (ADR-215): a program with its own desktop window. It draws a framed 160 x 96 scene - a
//! border, a diagonal, and a bar that moves one column per frame - and hands each frame to the
//! desktop with `SYS_PRESENT`. Given `once` it shows one frame and exits with the frame count;
//! otherwise it animates until the operator kills it or closes its window (then it exits 0).
#![no_std]
#![no_main]

mod mem;
mod sys;

const W: u32 = 160;
const H: u32 = 96;
const BYTES: usize = (W * H / 8) as usize;
/// Frames between presents' busy work, so the bar moves at a visible pace under emulation.
const SPIN: u32 = 20_000;

/// The frame, in `.bss` (two pages): where `SYS_PRESENT` requires it to be.
static mut FRAME: [u8; BYTES] = [0; BYTES];

fn put(frame: &mut [u8; BYTES], x: u32, y: u32) {
    let i = (y * W + x) as usize;
    frame[i / 8] |= 1 << (i % 8);
}

fn render(frame: &mut [u8; BYTES], tick: u32) {
    frame.fill(0);
    for x in 0..W {
        put(frame, x, 0);
        put(frame, x, H - 1);
    }
    for y in 0..H {
        put(frame, 0, y);
        put(frame, W - 1, y);
        put(frame, y * (W - 1) / (H - 1), y);
    }
    let bar = 4 + tick % (W - 12);
    for y in H / 3..2 * H / 3 {
        for x in bar..bar + 4 {
            put(frame, x, y);
        }
    }
}

/// The entry point the kernel jumps to.
///
/// # Safety
/// Only the kernel calls this, with `args` pointing at `len` readable bytes (ADR-206).
#[no_mangle]
#[link_section = ".text._start"]
pub unsafe extern "C" fn _start(args: *const u8, len: usize) -> ! {
    // SAFETY: single-threaded program; nothing else touches this global.
    let frame = unsafe { &mut *core::ptr::addr_of_mut!(FRAME) };
    // SAFETY: the kernel placed `len` argument bytes at `args`, on this program's stack page.
    let once = unsafe { core::slice::from_raw_parts(args, len) } == b"once";
    let mut tick = 0u32;
    loop {
        render(frame, tick);
        if sys::present(frame, W, H).is_err() {
            // No desktop, or the operator closed the window: nothing left to draw on.
            sys::exit(tick as u64);
        }
        tick += 1;
        if once {
            sys::exit(tick as u64);
        }
        for _ in 0..SPIN {
            core::hint::spin_loop();
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    sys::exit(u64::MAX)
}
