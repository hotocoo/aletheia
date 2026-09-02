//! The machine's LIVE desktop (ALET-P2-021's hardware rung, ADR-080; the terminal window,
//! ADR-083).
//!
//! Everything upstream of this module is a contract plus a proof: ADR-077 modeled the
//! composition contract, ADR-078 put it on the scanout, ADR-079 built the input session,
//! ADR-080 wired real virtio-input devices into that session, and ADR-083 put TEXT in the
//! window. This module is where the machine RUNS all of it at once: one compositor over the
//! framebuffer console's geometry, ONE input session, a wallpaper panel and a TERMINAL WINDOW
//! under their owner tokens, a cursor the pointer hardware steers, a title band the pointer
//! drags the window by, and a pump that drains the devices, routes what they say through the
//! session, and hands any frame that actually changed to the display device as exactly one
//! TRANSFER plus one FLUSH — a quiet desktop moves nothing (ADR-056's GUI twin, measured on the
//! driver's own command counter by `compose_suite`).
//!
//! # The terminal window is the console's second surface
//!
//! The interactive console (`shell::run_loop`, on the main thread) keeps ONE session. Since
//! ADR-083 that session has two surfaces: the serial line and this window. Every byte the
//! console emits also lands in the window's [`TextGrid`] ([`term_write`]); every keystroke the
//! input session routed into the focused window's queue is drained by the console's own `getc`
//! ([`term_getc`]) after the serial/PS-2 ring — so a virtio keyboard types at the same shell a
//! serial line does, and the shell's answer is painted where the keystroke landed.
//!
//! # The concurrency posture, stated rather than implied
//!
//! This kernel is single-running-CPU (the SMP suite parks its APs). The desktop has TWO
//! contexts that touch it and they never overlap: the IRQ0 (PIT) handler, which calls
//! [`tick_pump`] at 100 Hz with IF=0 by construction; and the main thread, which enters ONLY
//! through [`with_desktop`], inside `without_interrupts`. Neither can preempt the other, so
//! every mutation is serialized by the CPU's interrupt flag and no lock is needed — and none is
//! taken, because the main thread also holds console locks (`RESIDENT`, the ring) that an IRQ
//! path must never spin on. The devices were brought up by the boot before the VT-d gate turned
//! enforcement on (the ADR-073 ordering); their DMA windows cover exactly the frames their
//! registries vouched for, and nothing here registers or revokes.
//!
//! The pump is BOUNDED: at most one queue depth of events per tick per device, a compose only
//! when something owes a repaint (`Compositor::has_pending_damage`, allocation-free), and a
//! device command only when the model reports it wrote pixels. The idle tick is two used-ring
//! reads and one damage check — no compose, no heap, no device traffic. The terminal's render
//! buffer is allocated once at install and reused (ADR-063's heap never frees; that cost is
//! per install, never per frame).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::vec::Vec;
use kernel_core::compositor::{Compositor, EventKind, Rect};
use kernel_core::fbcon::{ComposeSink, Surface};
use kernel_core::textgrid::{TextGrid, TITLE_H};
use kernel_core::vinput::{self, Button, KeyDecoder, PointerDecoder};
use kernel_core::virtiogpu::{self, Rect as GpuRect};

use crate::virtio::{Gpu, Input, InputDev};

/// The desktop's resource id on the GPU device — distinct from the suites' ids, because the
/// suites' resources are torn down and this one lives as long as the machine does.
const DESKTOP_RID: u32 = 11;

/// The scanout: the framebuffer console's geometry, the same one `compose_suite` proves.
const W: u32 = virtiogpu::CONSOLE_FB_WIDTH;
const H: u32 = virtiogpu::CONSOLE_FB_HEIGHT;
const PAGES: usize = virtiogpu::CONSOLE_FB_PAGES;

/// Surface ids: the wallpaper panel and the terminal window.
const PANEL: u32 = 1;
const WINDOW: u32 = 2;
/// The terminal grid: 40 columns x 12 rows of the console's alphabet (320 x 106 pixels with
/// the title band), placed so a gap of empty scanout remains for an "empty space" click.
const TERM_COLS: u32 = 40;
const TERM_ROWS: u32 = 12;
const WINDOW_X: i32 = 300;
const WINDOW_Y: i32 = 60;
const TITLE: &[u8] = b"aletheia";
/// Keystrokes the main thread may hold between drains (the console pops one per loop turn).
const TERM_INPUT_CAP: usize = 64;

/// The desktop: its devices, its decoders, its compositor and session, its surfaces' owner
/// tokens, its terminal grid, and its GPU resource.
pub struct Desktop {
    kb: Input,
    tab: Input,
    kb_dec: KeyDecoder,
    pt_dec: PointerDecoder,
    comp: Compositor,
    sess: u64,
    tok_win: u64,
    gpu: Gpu,
    pages: Vec<usize>,
    posted: u64,
    term: TextGrid,
    packed: Vec<u8>,
    /// Keystrokes drained from the window's queue, waiting for the console's `getc`.
    term_input: Vec<u8>,
    /// A drag in progress: the pointer's offset from the window's top-left at the press.
    drag: Option<(i32, i32)>,
    /// Where the pointer last was (the cursor's own position, mirrored so a press knows it).
    pointer: (u32, u32),
    /// Drags completed (press on the title band, release anywhere), for the ledger.
    drags: u64,
}

static mut DESKTOP: Option<Desktop> = None;
/// Pump enable — set once by [`install`], never cleared. Before it is set, [`tick_pump`] is
/// one relaxed load and a return, which is why it is safe to call unconditionally from IRQ0
/// during the whole boot.
static ENABLED: AtomicBool = AtomicBool::new(false);

// The facts `input` reports that the PUMP owns. Each is one machine word the pump STOREs and
// the console LOADs, so the cross-context read is atomic by construction. `u64::MAX` = absent.
static FACT_KB_EVENTS: AtomicU64 = AtomicU64::new(0);
static FACT_PT_EVENTS: AtomicU64 = AtomicU64::new(0);
static FACT_POSTED: AtomicU64 = AtomicU64::new(0);
static FACT_DROPPED: AtomicU64 = AtomicU64::new(0);
static FACT_REFUSED: AtomicU64 = AtomicU64::new(0);
static FACT_QUEUED: AtomicU64 = AtomicU64::new(0);
static FACT_CURSOR: AtomicU64 = AtomicU64::new(u64::MAX);
static FACT_FOCUS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Bring the desktop up: own the GPU and the input devices, allocate the backing pages,
/// create the resource, attach the scanout, mint the session, paint the panel and the
/// terminal window, focus the window, put the cursor where a user would look, compose the
/// first frame, and hand it to the device. Called BEFORE the VT-d gate — the device DMA window
/// is built from what the registries vouch for at that moment, so every page the desktop will
/// ever touch must be registered before enforcement turns on. Returns the GPU function's FULL
/// grant list (backing pages included) for the window builder.
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

    // The model: the same contract, the same geometry — a wallpaper panel and the terminal
    // window, each drawn through its OWN token.
    let term = TextGrid::new(TERM_COLS, TERM_ROWS);
    let (tw, th) = term.pixel_size();
    let mut comp = Compositor::new(0x0D3A_1D05, W, H);
    let sess = comp
        .open_input_session()
        .map_err(|_| "the desktop's input session was refused")?;
    let tok_panel = comp
        .mint_surface(PANEL, 400, 200)
        .map_err(|_| "the desktop's panel surface was refused")?;
    let tok_win = comp
        .mint_surface(WINDOW, tw, th)
        .map_err(|_| "the desktop's window surface was refused")?;
    let _ = comp.fill_rect(
        PANEL,
        tok_panel,
        Rect {
            x: 0,
            y: 0,
            w: 400,
            h: 200,
        },
        true,
    );
    let _ = comp.fill_rect(
        PANEL,
        tok_panel,
        Rect {
            x: 8,
            y: 8,
            w: 384,
            h: 184,
        },
        false,
    );
    let mut packed = Vec::new();
    term.render_packed(TITLE, &mut packed);
    comp.fill_packed(WINDOW, tok_win, &packed)
        .map_err(|_| "the terminal window's first paint was refused")?;
    comp.attach(PANEL, tok_panel, 0, 0)
        .map_err(|_| "the panel placement was refused")?;
    comp.attach(WINDOW, tok_win, WINDOW_X, WINDOW_Y)
        .map_err(|_| "the window placement was refused")?;
    // The window holds focus until somebody clicks elsewhere; the cursor starts over it.
    comp.set_focus(sess, WINDOW)
        .map_err(|_| "focusing the window was refused")?;
    comp.move_cursor(sess, 320, 120)
        .map_err(|_| "placing the cursor was refused")?;

    // First frame: compose through the real raster and hand it to the device.
    show_frame(&mut comp, &pages, &mut gpu)?;

    let kb_events = kb.dev.events_seen();
    let pt_events = tab.dev.events_seen();
    // The GPU's grant list is captured AFTER the backing attach, so the window the VT-d gate
    // builds covers every page the desktop's frames will ever write through.
    let grants = gpu.dma_grants();

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

    // SAFETY: single-threaded boot with IF=0; this is the only writer the static ever has
    // until IRQ0 takes over, and IRQ0 cannot run mid-instruction.
    unsafe {
        (*core::ptr::addr_of_mut!(DESKTOP)) = Some(Desktop {
            kb: kb.dev,
            tab: tab.dev,
            kb_dec: KeyDecoder::new(),
            pt_dec: PointerDecoder::new(W, H),
            comp,
            sess,
            tok_win,
            gpu,
            pages,
            posted: 0,
            term,
            packed,
            term_input: Vec::with_capacity(TERM_INPUT_CAP),
            drag: None,
            pointer: (320, 120),
            drags: 0,
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

impl Desktop {
    /// Repaint the terminal window from its grid if the grid changed, then show a frame if
    /// anything owes a repaint. Both contexts call this; both hold the interrupt flag clear.
    fn repaint(&mut self) {
        if self.term.take_dirty() {
            self.term.render_packed(TITLE, &mut self.packed);
            let _ = self.comp.fill_packed(WINDOW, self.tok_win, &self.packed);
        }
        if self.comp.has_pending_damage() {
            let _ = show_frame(&mut self.comp, &self.pages, &mut self.gpu);
        }
    }

    /// A pointer batch on top of what `route_pointer_batch` already did (cursor move, click as
    /// a focus decision): a press on the terminal's title band starts a DRAG and raises the
    /// window; motion while dragging moves the window by the pointer's delta (the compositor
    /// refuses a fully-off placement, and the window then simply stays); a release ends it.
    fn pointer_batch(&mut self, batch: vinput::PointerBatch) {
        if let Some(p) = batch.move_to {
            self.pointer = p;
        }
        let (px, py) = (self.pointer.0 as i32, self.pointer.1 as i32);
        if let Some((Button::Left, down)) = batch.button {
            if down {
                if let Some((wx, wy)) = self.comp.placement(WINDOW) {
                    if self.term.in_title(px - wx, py - wy) {
                        self.drag = Some((px - wx, py - wy));
                        let _ = self.comp.raise(WINDOW, self.tok_win);
                    }
                }
            } else if self.drag.take().is_some() {
                self.drags += 1;
            }
        }
        if let (Some((dx, dy)), Some(_)) = (self.drag, batch.move_to) {
            let _ = self
                .comp
                .move_surface(WINDOW, self.tok_win, px - dx, py - dy);
        }
    }

    /// Drain the focused window's queue into the terminal's input line (owner token only —
    /// the window is the principal that reads what the session routed to it).
    fn drain_window(&mut self) {
        if self.term_input.len() >= TERM_INPUT_CAP {
            return;
        }
        if let Ok(events) = self.comp.drain_input(WINDOW, self.tok_win) {
            for e in events {
                if let EventKind::Key(b) = e.kind {
                    if self.term_input.len() < TERM_INPUT_CAP {
                        self.term_input.push(b);
                    }
                }
            }
        }
    }

    fn publish_facts(&self) {
        let (dropped, refused) = self.comp.input_counters();
        let queued_len = self.comp.queued_len(WINDOW);
        FACT_KB_EVENTS.store(self.kb.events_seen(), Ordering::Relaxed);
        FACT_PT_EVENTS.store(self.tab.events_seen(), Ordering::Relaxed);
        FACT_POSTED.store(self.posted, Ordering::Relaxed);
        FACT_DROPPED.store(dropped, Ordering::Relaxed);
        FACT_REFUSED.store(refused, Ordering::Relaxed);
        FACT_QUEUED.store(queued_len as u64, Ordering::Relaxed);
        match self.comp.cursor() {
            Some((x, y)) => FACT_CURSOR.store(((x as u64) << 32) | y as u64, Ordering::Relaxed),
            None => FACT_CURSOR.store(u64::MAX, Ordering::Relaxed),
        }
        match self.comp.focus() {
            Some(id) => FACT_FOCUS.store(id as u64, Ordering::Relaxed),
            None => FACT_FOCUS.store(u64::MAX, Ordering::Relaxed),
        }
    }
}

/// The pump: drain the devices, route through the session, show what changed. Runs in IRQ0
/// context after [`install`]; see the module docs for the concurrency posture.
pub fn tick_pump() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: IRQ context, IF=0 — the main thread only touches the static inside
    // `without_interrupts`, so no other reference exists while this one lives.
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
                if let Ok(batch) =
                    vinput::route_pointer_batch(&mut d.pt_dec, &mut d.comp, d.sess, ev)
                {
                    d.pointer_batch(batch);
                }
            }
            None => break,
        }
    }
    // The keystrokes the session routed to the window are the console's to read: hand them
    // across to the main thread's line now, so `getc` finds them on its next turn.
    d.drain_window();
    d.repaint();
    d.publish_facts();
}

/// Did this machine install a live desktop? One relaxed load; the console's interrupt bring-up
/// asks this to decide whether the PIT tick stays unmasked (the pump runs on it) or is quieted.
/// Only the interactive console asks, so only that build carries the question.
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

/// The console's output reaches the terminal window (ADR-083). Main thread only; a machine
/// with no desktop drops the bytes on the floor exactly as it always did.
#[cfg(feature = "interactive")]
pub fn term_write(bytes: &[u8]) {
    let _ = with_desktop(|d| {
        d.term.write(bytes);
        d.repaint();
    });
}

/// One keystroke the input session routed into the focused terminal window, if any — the
/// console's `getc` asks after the serial/PS-2 ring (ADR-083). Main thread only.
#[cfg(feature = "interactive")]
pub fn term_getc() -> Option<u8> {
    with_desktop(|d| {
        d.drain_window();
        if d.term_input.is_empty() {
            None
        } else {
            Some(d.term_input.remove(0))
        }
    })
    .flatten()
}

/// The facts the console's `input` command reports. `None` = this machine never installed a
/// desktop, and the command says so rather than inventing zeros. The pump-owned counters are
/// machine-word atomics; the window's placement and the terminal's line are read through the
/// same door the console writes through.
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
    let (window, term_lines, term_last, term_last_len) = with_desktop(|d| {
        let mut last = [0u8; 48];
        let line = d.term.last_nonblank_line();
        let n = line.len().min(last.len());
        last[..n].copy_from_slice(&line[..n]);
        (d.comp.placement(WINDOW), d.term.lines(), last, n as u8)
    })
    .unwrap_or((None, 0, [0u8; 48], 0));
    Some(kernel_core::shell::InputFacts {
        events_posted: FACT_POSTED.load(Ordering::Relaxed),
        dropped: FACT_DROPPED.load(Ordering::Relaxed),
        refusals: FACT_REFUSED.load(Ordering::Relaxed),
        queued: FACT_QUEUED.load(Ordering::Relaxed) as usize,
        cursor,
        focus,
        kb_events: FACT_KB_EVENTS.load(Ordering::Relaxed),
        pt_events: FACT_PT_EVENTS.load(Ordering::Relaxed),
        window,
        term_lines,
        term_last,
        term_last_len,
    })
}

/// Drags completed since install, for the boot log and a human's curiosity.
#[allow(dead_code)]
pub fn drags() -> u64 {
    with_desktop(|d| d.drags).unwrap_or(0)
}

#[allow(dead_code)]
const _: () = assert!(TITLE_H > 0);
