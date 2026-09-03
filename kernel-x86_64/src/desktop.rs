//! This target's LIVE desktop: the SHARED desktop (`kernel_core::desktop`, ADR-085) plus the
//! three things that belong to the platform — who owns the static, which interrupt wakes the
//! pump, and where the frames come from.
//!
//! Everything the desktop DOES now lives in `kernel-core` beside the contracts it runs
//! (composition ADR-077/078, the input session ADR-079, real devices ADR-080, the terminal
//! ADR-083, the managed window set ADR-084). What stays here is what only this CPU can answer.
//!
//! # The concurrency posture, stated rather than implied
//!
//! This kernel is single-running-CPU (the SMP suite parks its APs). Two contexts touch the
//! desktop and they never overlap: the IRQ0 (PIT) handler, which calls [`tick_pump`] at 100 Hz
//! with IF=0 by construction; and the main thread, which enters ONLY through [`with_desktop`],
//! inside `without_interrupts`. Neither can preempt the other, so every mutation is serialized
//! by the CPU's interrupt flag and no lock is needed — and none is taken, because the main
//! thread also holds console locks that an IRQ path must never spin on.

use core::sync::atomic::{AtomicBool, Ordering};

use alloc::vec::Vec;
use kernel_core::desktop::{Desktop as CoreDesktop, PAGES};
use kernel_core::virtiopci::PciTransport;

use crate::virtio::{Gpu, InputDev, X86Virtio};

/// This target's desktop: the shared one, over this target's HAL and transport.
type Desktop = CoreDesktop<X86Virtio, PciTransport>;

static mut DESKTOP: Option<Desktop> = None;
/// Pump enable — set once by [`install`], never cleared. Before it is set, [`tick_pump`] is one
/// relaxed load and a return, which is why it is safe to call unconditionally from IRQ0 during
/// the whole boot.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Bring the desktop up and hand back the GPU function's FULL grant list (backing pages
/// included) for the VT-d window builder. Called BEFORE the vt-d gate: every page the desktop
/// will ever touch must be inside the granted window when enforcement turns on (ADR-073).
pub fn install(
    gpu: Gpu,
    kb: InputDev,
    tab: InputDev,
) -> Result<Vec<kernel_core::dma::Grant>, &'static str> {
    let mut pages: Vec<usize> = Vec::with_capacity(PAGES);
    for _ in 0..PAGES {
        match crate::frames::alloc_zeroed() {
            Some(f) => pages.push(f.addr()),
            None => return Err("the frame allocator could not cover the desktop's backing store"),
        }
    }
    // SAFETY: the GPU and both input functions were brought up by the boot and are exclusively
    // ours here; the pages are identity-mapped frames this kernel owns for the machine's life.
    let d = unsafe { Desktop::install(gpu, kb.dev, tab.dev, pages)? };
    let grants = d.gpu_grants();
    // SAFETY: single-threaded boot with IF=0; this is the only writer the static ever has until
    // IRQ0 takes over, and IRQ0 cannot run mid-instruction.
    unsafe {
        (*core::ptr::addr_of_mut!(DESKTOP)) = Some(d);
    }
    ENABLED.store(true, Ordering::Relaxed);
    Ok(grants)
}

/// The pump, on IRQ0. See the module docs for why no lock is taken.
pub fn tick_pump() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: IRQ context, IF=0 — the main thread only touches the static inside
    // `without_interrupts`, so no other reference exists while this one lives.
    let d = unsafe { (*core::ptr::addr_of_mut!(DESKTOP)).as_mut() };
    let Some(d) = d else { return };
    // SAFETY: the devices are live (DRIVER_OK at boot) and this is the sole live reference.
    unsafe { d.pump(crate::frames::free_count(), crate::frames::total_count()) };
}

/// Did this machine install a live desktop? One relaxed load; the console's interrupt bring-up
/// asks this to decide whether the PIT tick stays unmasked (the pump runs on it) or is quieted.
#[cfg(feature = "interactive")]
pub fn is_live() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The main thread's ONLY door to the desktop: runs `f` with interrupts disabled so the pump
/// cannot run underneath it. `None` when no desktop was installed.
fn with_desktop<R>(f: impl FnOnce(&mut Desktop) -> R) -> Option<R> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    x86_64::instructions::interrupts::without_interrupts(|| {
        // SAFETY: IF is clear, so the pump (the only other writer) cannot run; this is the sole
        // live reference for the closure's duration.
        let d = unsafe { (*core::ptr::addr_of_mut!(DESKTOP)).as_mut() }?;
        Some(f(d))
    })
}

/// The console's output reaches the terminal window (ADR-083). Main thread only; a machine with
/// no desktop drops the bytes on the floor exactly as it always did.
#[cfg(feature = "interactive")]
pub fn term_write(bytes: &[u8]) {
    let _ = with_desktop(|d| d.term_write(bytes));
}

/// One keystroke the input session routed into the focused terminal window, if any — the
/// console's `getc` asks after the serial/PS-2 ring (ADR-083). Main thread only.
#[cfg(feature = "interactive")]
pub fn term_getc() -> Option<u8> {
    with_desktop(|d| d.term_getc()).flatten()
}

/// The facts the console's `input` command reports. `None` = this machine never installed a
/// desktop, and the command says so rather than inventing zeros.
pub fn facts() -> Option<kernel_core::shell::InputFacts> {
    with_desktop(|d| d.facts())
}

/// Windows the manager holds open right now — the boot log names how many came up (ADR-084).
pub fn window_count() -> usize {
    with_desktop(|d| d.window_count()).unwrap_or(0)
}
