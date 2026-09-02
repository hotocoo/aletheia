//! The machine's LIVE desktop (ALET-P2-021's hardware rung, ADR-080).
//!
//! Everything upstream of this module is a contract plus a proof: ADR-077 modeled the
//! composition contract, ADR-078 put it on the scanout, ADR-079 built the input session,
//! and ADR-080 wired real virtio-input devices into that session. This module is where the
//! machine finally RUNS all of it at once: one compositor over the framebuffer console's
//! geometry, ONE input session, a wallpaper and a window under their owner tokens, a cursor
//! the pointer hardware steers, and a pump that drains the devices, routes what they say
//! through the session, and hands any frame that actually changed to the display device as
//! exactly one TRANSFER plus one FLUSH — a quiet desktop moves nothing (ADR-056's GUI twin,
//! measured on the driver's own command counter by `compose_suite`).
//!
//! # The concurrency posture, stated rather than implied
//!
//! This kernel is single-running-CPU (the SMP suite parks its APs), and after
//! [`install`] returns, the desktop has exactly ONE writer: the IRQ0 (PIT) handler, which
//! calls [`tick_pump`] at 100 Hz. The main thread never touches the desktop again except
//! through [`facts`], which reads only `AtomicU64`s — every fact is one aligned machine word,
//! so a concurrent pump cannot tear a readout. The devices themselves were brought up by the
//! boot (before the VT-d gate turned enforcement on — the ADR-073 ordering), and their DMA
//! windows cover exactly the frames their registries vouched for; the pump never registers or
//! revokes anything, so the granted set never changes under enforcement.
//!
//! The pump is BOUNDED: at most one queue depth of events per tick per device, a compose only
//! when something owes a repaint (`Compositor::has_pending_damage`, allocation-free), and a
//! device command only when the model reports it wrote pixels. The idle tick is two used-ring
//! reads and one damage check — no compose, no heap, no device traffic. A CHANGED frame still
//! pays `compose_frame`'s z-order clone once, for the change that caused it (ADR-063's heap
//! never frees; that cost is per event, never per tick).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::vec::Vec;
use kernel_core::compositor::{Compositor, Rect};
use kernel_core::fbcon::{ComposeSink, Surface};
use kernel_core::vinput::{self, KeyDecoder, PointerDecoder};
use kernel_core::virtiogpu::{self, Rect as GpuRect};

use crate::virtio::{Gpu, Input, InputDev};

/// The desktop's resource id on the GPU device — distinct from the suites' ids, because the
/// suites' resources are torn down and this one lives as long as the machine does.
const DESKTOP_RID: u32 = 11;

/// The scanout: the framebuffer console's geometry, the same one `compose_suite` proves.
const W: u32 = virtiogpu::CONSOLE_FB_WIDTH;
const H: u32 = virtiogpu::CONSOLE_FB_HEIGHT;
const PAGES: usize = virtiogpu::CONSOLE_FB_PAGES;

/// The desktop: its devices, its decoders, its compositor and session, its surfaces' owner
/// tokens, and its GPU resource.
pub struct Desktop {
    kb: Input,
    tab: Input,
    kb_dec: KeyDecoder,
    pt_dec: PointerDecoder,
    comp: Compositor,
    sess: u64,
    gpu: Gpu,
    pages: Vec<usize>,
    posted: u64,
}

static mut DESKTOP: Option<Desktop> = None;
/// Pump enable — set once by [`install`], never cleared. Before it is set, [`tick_pump`] is
/// one relaxed load and a return, which is why it is safe to call unconditionally from IRQ0
/// during the whole boot.
static ENABLED: AtomicBool = AtomicBool::new(false);

// The facts `input` reports. Each is one machine word the pump STOREs and the console LOADs,
// so the cross-context read is atomic by construction. `u64::MAX` = absent.
static FACT_KB_EVENTS: AtomicU64 = AtomicU64::new(0);
static FACT_PT_EVENTS: AtomicU64 = AtomicU64::new(0);
static FACT_POSTED: AtomicU64 = AtomicU64::new(0);
static FACT_DROPPED: AtomicU64 = AtomicU64::new(0);
static FACT_REFUSED: AtomicU64 = AtomicU64::new(0);
static FACT_QUEUED: AtomicU64 = AtomicU64::new(0);
static FACT_CURSOR: AtomicU64 = AtomicU64::new(u64::MAX);
static FACT_FOCUS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Bring the desktop up: own the GPU and the input devices, allocate the backing pages,
/// create the resource, attach the scanout, mint the session, paint the two surfaces, focus
/// the window, put the cursor where a user would look, compose the first frame, and hand it
/// to the device. Called BEFORE the VT-d gate — the device DMA window is built from what the
/// registries vouch for at that moment, so every page the desktop will ever touch must be
/// registered before enforcement turns on. Returns the GPU function's FULL grant list
/// (backing pages included) for the window builder.
///
/// On any failure the desktop does not come up: the machine continues (the console is the
/// session that matters), and the failure is NAMED on the boot log.
pub fn install(
    mut gpu: Gpu,
    kb: InputDev,
    mut tab: InputDev,
) -> Result<Vec<kernel_core::dma::Grant>, &'static str> {
    // Backing pages: identity-mapped allocator frames, the compose_suite shape.
    let mut pages: Vec<usize> = Vec::with_capacity(PAGES);
    for _ in 0..PAGES {
        match crate::frames::alloc_zeroed() {
            Some(f) => pages.push(f.addr()),
            None => return Err("the frame allocator could not cover the desktop's backing store"),
        }
    }

    // SAFETY: the GPU was brought up by the boot and is exclusively ours here; the pages are
    // identity-mapped frames this kernel owns.
    unsafe {
        gpu.create_resource_2d(DESKTOP_RID, W, H)
            .map_err(|_| "the desktop's GPU resource was refused")?;
        gpu.attach_backing(DESKTOP_RID, &pages)
            .map_err(|_| "the desktop's backing pages were refused")?;
        gpu.set_scanout(0, DESKTOP_RID)
            .map_err(|_| "the desktop's scanout bind was refused")?;
    }

    // The model: the same contract, the same geometry, the compose_suite scene — a wallpaper
    // panel and a window, each drawn through its OWN token.
    let mut comp = Compositor::new(0x0D3A_1D05, W, H);
    let sess = comp
        .open_input_session()
        .map_err(|_| "the desktop's input session was refused")?;
    let tok_panel = comp
        .mint_surface(1, 400, 200)
        .map_err(|_| "the desktop's panel surface was refused")?;
    let tok_win = comp
        .mint_surface(2, 200, 80)
        .map_err(|_| "the desktop's window surface was refused")?;
    let border = Rect {
        x: 0,
        y: 0,
        w: 400,
        h: 200,
    };
    let _ = comp.fill_rect(1, tok_panel, border, true);
    let _ = comp.fill_rect(
        1,
        tok_panel,
        Rect {
            x: 8,
            y: 8,
            w: 384,
            h: 184,
        },
        false,
    );
    let _ = comp.fill_rect(
        2,
        tok_win,
        Rect {
            x: 0,
            y: 0,
            w: 200,
            h: 80,
        },
        true,
    );
    let _ = comp.fill_rect(
        2,
        tok_win,
        Rect {
            x: 4,
            y: 4,
            w: 192,
            h: 72,
        },
        false,
    );
    comp.attach(1, tok_panel, 0, 0)
        .map_err(|_| "the panel placement was refused")?;
    comp.attach(2, tok_win, 300, 60)
        .map_err(|_| "the window placement was refused")?;
    // The window holds focus until somebody clicks elsewhere; the cursor starts over it.
    comp.set_focus(sess, 2)
        .map_err(|_| "focusing the window was refused")?;
    comp.move_cursor(sess, 320, 120)
        .map_err(|_| "placing the cursor was refused")?;

    // First frame: compose through the real raster and hand it to the device.
    let first = show_frame(&mut comp, &pages, &mut gpu);
    first?;

    let kb_events = kb.dev.events_seen();
    let pt_events = tab.dev.events_seen();
    // SAFETY: the GPU's grant list is captured AFTER the backing attach, so the window the
    // VT-d gate builds covers every page the desktop's frames will ever write through.
    let grants = gpu.dma_grants();

    // SAFETY: single-threaded boot; this is the only writer the static ever has until IRQ0
    // takes over, and IRQ0 cannot run mid-instruction.
    let axis = tab
        .dev
        .abs_info(kernel_core::vinput::ABS_X)
        .zip(tab.dev.abs_info(kernel_core::vinput::ABS_Y));
    let Some(((x_min, x_max), (y_min, y_max))) = axis else {
        return Err("the desktop's pointer device did not declare both absolute axes");
    };
    if x_min != 0 || y_min != 0 || x_max != y_max {
        return Err("the desktop's pointer axes do not match the qualified range");
    }

    unsafe {
        (*core::ptr::addr_of_mut!(DESKTOP)) = Some(Desktop {
            kb: kb.dev,
            tab: tab.dev,
            kb_dec: KeyDecoder::new(),
            pt_dec: PointerDecoder::new(W, H),
            comp,
            sess,
            gpu,
            pages,
            posted: 0,
        });
        let d = (*core::ptr::addr_of_mut!(DESKTOP)).as_mut().unwrap();
        d.pt_dec.set_axis(x_max, y_max); // device-declared range, qualified by vinput_suite
    }
    FACT_KB_EVENTS.store(kb_events, Ordering::Relaxed);
    FACT_PT_EVENTS.store(pt_events, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Relaxed);
    Ok(grants)
}

/// Compose one frame through the real raster; hand it to the device ONLY if it changed.
/// Returns what the model reported — the caller decides whether that is a proof.
fn show_frame(comp: &mut Compositor, pages: &[usize], gpu: &mut Gpu) -> Result<u64, &'static str> {
    let mut surf =
        Surface::new(pages, W, H).map_err(|_| "the backing pages did not form a raster")?;
    let mut sink = ComposeSink::new(&mut surf);
    let st = comp.compose_frame(&mut sink);
    if sink.refusals() != 0 {
        return Err("the real raster refused a put the model's bounds allowed");
    }
    if st.pixels_blitted == 0 {
        return Ok(0); // a quiet frame moves nothing — no device traffic at all
    }
    // SAFETY: live device; the rect is the resource's own full extent.
    unsafe {
        gpu.transfer_to_host_2d(DESKTOP_RID, GpuRect::covering(W, H))
            .map_err(|_| "the desktop frame's TRANSFER was refused")?;
        gpu.resource_flush(DESKTOP_RID, GpuRect::covering(W, H))
            .map_err(|_| "the desktop frame's FLUSH was refused")?;
    }
    Ok(st.pixels_blitted)
}

/// The pump: drain the devices, route through the session, show what changed. Runs in IRQ0
/// context after [`install`]; see the module docs for the concurrency posture.
pub fn tick_pump() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: the single-writer invariant from the module docs — every mutation below happens
    // here, in PIT context, serialized by the CPU; the boot's other phases ran before ENABLED.
    let d = unsafe { (*core::ptr::addr_of_mut!(DESKTOP)).as_mut() };
    let Some(d) = d else { return };

    for _ in 0..32 {
        // SAFETY: the device is live (DRIVER_OK at boot).
        match unsafe { d.kb.next_event() } {
            Some(ev) => {
                if let Ok(nb) = vinput::route_key(&mut d.kb_dec, &mut d.comp, d.sess, ev) {
                    d.posted += nb as u64;
                }
            }
            None => break,
        }
    }
    for _ in 0..32 {
        // SAFETY: the device is live (DRIVER_OK at boot).
        match unsafe { d.tab.next_event() } {
            Some(ev) => {
                let _ = vinput::route_pointer(&mut d.pt_dec, &mut d.comp, d.sess, ev);
            }
            None => break,
        }
    }

    // A frame is COMPOSED only when something owes a repaint, and goes to the device only when
    // the model actually painted. The question is allocation-free; `compose_frame` is not (it
    // clones the z-order to walk it), and on the boot heap — which never frees, ADR-063 — an
    // allocation per quiet tick would be a leak at 100 Hz. The idle tick touches no heap.
    if d.comp.has_pending_damage() {
        let _ = show_frame(&mut d.comp, &d.pages, &mut d.gpu);
    }

    // Publish the facts, one machine word each.
    let (dropped, refused) = d.comp.input_counters();
    let queued_len = d.comp.queued_len(2);
    FACT_KB_EVENTS.store(d.kb.events_seen(), Ordering::Relaxed);
    FACT_PT_EVENTS.store(d.tab.events_seen(), Ordering::Relaxed);
    FACT_POSTED.store(d.posted, Ordering::Relaxed);
    FACT_DROPPED.store(dropped, Ordering::Relaxed);
    FACT_REFUSED.store(refused, Ordering::Relaxed);
    FACT_QUEUED.store(queued_len as u64, Ordering::Relaxed);
    match d.comp.cursor() {
        Some((x, y)) => FACT_CURSOR.store(((x as u64) << 32) | y as u64, Ordering::Relaxed),
        None => FACT_CURSOR.store(u64::MAX, Ordering::Relaxed),
    }
    match d.comp.focus() {
        Some(id) => FACT_FOCUS.store(id as u64, Ordering::Relaxed),
        None => FACT_FOCUS.store(u64::MAX, Ordering::Relaxed),
    }
}

/// Did this machine install a live desktop? One relaxed load; the console's interrupt bring-up
/// asks this to decide whether the PIT tick stays unmasked (the pump runs on it) or is quieted.
/// Only the interactive console asks, so only that build carries the question.
#[cfg(feature = "interactive")]
pub fn is_live() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The facts the console's `input` command reports. `None` = this machine never installed a
/// desktop, and the command says so rather than inventing zeros.
pub fn facts() -> Option<kernel_core::shell::InputFacts> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let cursor = match FACT_CURSOR.load(Ordering::Relaxed) {
        u64::MAX => None,
        w => Some(((w >> 32) as u32, w as u32)),
    };
    let focus = match FACT_FOCUS.load(Ordering::Relaxed) {
        u64::MAX => None,
        id => Some(id as u32),
    };
    Some(kernel_core::shell::InputFacts {
        events_posted: FACT_POSTED.load(Ordering::Relaxed),
        dropped: FACT_DROPPED.load(Ordering::Relaxed),
        refusals: FACT_REFUSED.load(Ordering::Relaxed),
        queued: FACT_QUEUED.load(Ordering::Relaxed) as usize,
        cursor,
        focus,
        kb_events: FACT_KB_EVENTS.load(Ordering::Relaxed),
        pt_events: FACT_PT_EVENTS.load(Ordering::Relaxed),
    })
}
