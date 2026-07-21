//! Kernel console: fans `kprintln!` output to the COM1 serial line AND, once captured, the GOP
//! framebuffer. Serial is the test oracle; the framebuffer is what a human sees in a VM window.
//!
//! `kprint!`/`kprintln!` are `#[macro_export]`ed at the crate root so the SHARED, arch-independent
//! `selftest.rs` (pulled in via `#[path]`, identical source to the aarch64 kernel) uses them
//! unchanged — the same invariant suite prints through whichever backend the target provides.

use crate::cell::Racy;
use crate::framebuffer::FrameBuffer;
use core::fmt::{self, Write};

static FB: Racy<Option<FrameBuffer>> = Racy::new(None);

/// Install the framebuffer as a second output sink (called once, after GOP capture).
pub fn set_framebuffer(fb: FrameBuffer) {
    // SAFETY: single-core boot; called once before any interrupt handler could touch FB.
    unsafe { *FB.get_mut() = Some(fb) }
}

struct Sink;

impl Write for Sink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::serial::puts(s);
        // SAFETY: single-core; no interrupt handler writes to the console (the timer handler only
        // bumps an atomic), so no reentrant mutable borrow of FB occurs.
        unsafe {
            if let Some(fb) = FB.get_mut() {
                fb.write_str(s);
            }
        }
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    let _ = Sink.write_fmt(args);
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => ($crate::console::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! kprintln {
    () => ($crate::console::_print(format_args!("\n")));
    ($($arg:tt)*) => ({
        $crate::console::_print(format_args!($($arg)*));
        $crate::console::_print(format_args!("\n"));
    });
}
