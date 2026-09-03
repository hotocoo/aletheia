//! This target's LIVE desktop: the SHARED desktop (`kernel_core::desktop`, ADR-085) plus the
//! three things that belong to the platform — who owns the static, which interrupt wakes the
//! pump, and where the frames come from.
//!
//! The aarch64 posture: the desktop is installed by the boot (before the SMMUv3 gate, so its
//! backing frames are inside the stage-2 identity domain that enforcement covers), and pumped
//! from the EL1 IRQ handler on the generic timer PPI, which only an interactive boot arms. A
//! non-interactive gate therefore composes the FIRST frame and then stands still — the desktop
//! is up and proved, and nothing is claimed about motion nobody is there to see.
//!
//! Two contexts touch the desktop and they never overlap: the timer IRQ (which runs with IRQs
//! masked by the hardware until `eret`) and the console's main thread, which enters only through
//! [`with_desktop`] with `DAIF.I` set. No lock is taken, for the same reason x86-64 takes none.

use core::sync::atomic::{AtomicBool, Ordering};

use alloc::vec::Vec;
use kernel_core::desktop::{Desktop as CoreDesktop, PAGES};
use kernel_core::virtioblk::MmioTransport;

use crate::virtio::{Aarch64Virtio, Gpu, Input};

type Desktop = CoreDesktop<Aarch64Virtio, MmioTransport>;

static mut DESKTOP: Option<Desktop> = None;
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Bring the desktop up over this machine's own frames. Returns how many windows came up.
pub fn install(gpu: Gpu, kb: Input, tab: Input) -> Result<usize, &'static str> {
    let mut pages: Vec<usize> = Vec::with_capacity(PAGES);
    for _ in 0..PAGES {
        match crate::frames::alloc_zeroed() {
            Some(f) => pages.push(f.addr()),
            None => return Err("the frame allocator could not cover the desktop's backing store"),
        }
    }
    // SAFETY: the GPU and both input devices were brought up by this boot and are exclusively
    // ours here; the pages are identity-mapped frames this kernel owns for the machine's life.
    let d = unsafe { Desktop::install(gpu, kb, tab, pages)? };
    let windows = d.window_count();
    // SAFETY: single-threaded boot with IRQs masked; this is the only writer the static has
    // until the timer IRQ starts pumping.
    unsafe {
        (*core::ptr::addr_of_mut!(DESKTOP)) = Some(d);
    }
    ENABLED.store(true, Ordering::Relaxed);
    Ok(windows)
}

/// The pump, on the generic timer PPI (interactive boots only — see the module docs).
#[cfg(feature = "interactive")]
pub fn tick_pump() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: IRQ context with IRQs masked until `eret`; the main thread only touches the static
    // with `DAIF.I` set, so no other reference exists while this one lives.
    let d = unsafe { (*core::ptr::addr_of_mut!(DESKTOP)).as_mut() };
    let Some(d) = d else { return };
    // SAFETY: the devices are live (DRIVER_OK at boot) and this is the sole live reference.
    unsafe { d.pump(crate::frames::free_count(), crate::frames::total_count()) };
}

/// Did this machine install a live desktop? Only the interrupt bring-up asks, and only an
/// interactive boot has one — a gate build installs the desktop, composes its first frame and
/// never arms a timer, which is exactly what ADR-085 names.
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub fn is_live() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The main thread's ONLY door to the desktop: runs `f` with IRQs masked so the pump cannot run
/// underneath it. `None` when no desktop was installed.
#[cfg(feature = "interactive")]
fn with_desktop<R>(f: impl FnOnce(&mut Desktop) -> R) -> Option<R> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let was_unmasked = crate::conirq::mask_irqs_pub();
    // SAFETY: IRQs are masked, so the pump (the only other writer) cannot run; this is the sole
    // live reference for the closure's duration.
    let out = unsafe { (*core::ptr::addr_of_mut!(DESKTOP)).as_mut().map(f) };
    if was_unmasked {
        crate::conirq::unmask_irqs_pub();
    }
    out
}

/// The console's output reaches the terminal window (ADR-083). Main thread only.
#[cfg(feature = "interactive")]
pub fn term_write(bytes: &[u8]) {
    let _ = with_desktop(|d| d.term_write(bytes));
}

/// One keystroke the input session routed into the terminal window, if any.
#[cfg(feature = "interactive")]
pub fn term_getc() -> Option<u8> {
    with_desktop(|d| d.term_getc()).flatten()
}

/// The facts the console's `input` command reports.
#[cfg(feature = "interactive")]
pub fn facts() -> Option<kernel_core::shell::InputFacts> {
    with_desktop(|d| d.facts())
}
