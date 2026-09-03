//! ONE desktop, three CPUs (ALET-P2-021's portability rung, ADR-085).
//!
//! ADR-080 put a live desktop on x86-64 and ADR-084 gave it a managed set of windows — but the
//! machine that RAN them was one target's module: the compositor, the session, the window
//! manager, the terminal grid, the monitor panel and the pump all lived in `kernel-x86_64`.
//! Every contract underneath was arch-neutral and proved on all three CPUs; only the thing that
//! ran them was not. This module is that thing, moved to where the contracts already live:
//! generic over the same [`VirtioHal`] + [`Transport`] seams the drivers use, so aarch64,
//! RISC-V and x86-64 install and pump the SAME desktop, and a difference between targets can
//! only come from the platform's own plumbing (which timer wakes the pump, who owns the
//! statics) rather than from a second implementation drifting from the first.
//!
//! What stays with the target, deliberately:
//!
//! * **Ownership and the concurrency posture.** Each kernel keeps its own static and its own
//!   door into it (interrupts masked, or whatever that CPU's rule is). This module is a plain
//!   value with `&mut self` methods: it takes no lock and knows no interrupt flag.
//! * **The frame allocator's reading.** [`Desktop::pump`] is handed free/total frames rather
//!   than calling an allocator: the machine's memory ledger is the platform's to report.
//! * **The wake-up.** A pump that never runs is a dead desktop, but WHEN it runs is a timer
//!   question every CPU answers differently (PIT, the ARM generic timer PPI, the RISC-V timer).
//!
//! The pump is BOUNDED exactly as ADR-080 left it: at most one queue depth of events per tick
//! per device, a compose only when something owes a repaint, and a device command only when the
//! model reports it wrote pixels. An idle tick is two used-ring reads and one damage check.

use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::compositor::{Compositor, EventKind, Rect};
use crate::fbcon::{ComposeSink, Surface};
use crate::shell::InputFacts;
use crate::textgrid::TextGrid;
use crate::vinput::{self, Button, ConfigWrite, KeyDecoder, PointerDecoder, VirtioInput};
use crate::virtioblk::{Transport, VirtioHal};
use crate::virtiogpu::{self, Rect as GpuRect, VirtioGpu};
use crate::wm::{Press, WindowManager};

/// The desktop's resource id on the GPU device — distinct from the suites' ids, because the
/// suites' resources are torn down and this one lives as long as the machine does.
pub const DESKTOP_RID: u32 = 11;

/// The scanout: the framebuffer console's geometry, the same one `compose_suite` proves.
pub const W: u32 = virtiogpu::CONSOLE_FB_WIDTH;
pub const H: u32 = virtiogpu::CONSOLE_FB_HEIGHT;
/// Backing pages the caller must hand over (identity-mapped frames it owns).
pub const PAGES: usize = virtiogpu::CONSOLE_FB_PAGES;

/// Surface ids: the wallpaper panel (the desktop's own, not a window) and the two WINDOWS the
/// manager holds — the terminal and the system monitor (ADR-084).
const PANEL: u32 = 1;
pub const WINDOW: u32 = 2;
pub const MONITOR: u32 = 3;

/// The terminal grid: 40 columns x 12 rows of the console's alphabet, placed so a gap of empty
/// scanout remains for an "empty space" click.
const TERM_COLS: u32 = 40;
const TERM_ROWS: u32 = 12;
const WINDOW_X: i32 = 300;
const WINDOW_Y: i32 = 60;
const TITLE: &[u8] = b"aletheia";
/// The system monitor: a second application on the same contract.
const MON_COLS: u32 = 30;
const MON_ROWS: u32 = 6;
const MON_X: i32 = 20;
const MON_Y: i32 = 140;
const MON_TITLE: &[u8] = b"monitor";
/// Keystrokes the main thread may hold between drains (the console pops one per loop turn).
const TERM_INPUT_CAP: usize = 64;
/// Events drained per device per pump — bounded, so one noisy device cannot own the tick.
const EVENTS_PER_TICK: usize = 32;

/// Every number the monitor window prints. Compared whole, so "nothing changed" is a fact about
/// the panel's contents rather than about a hash of them (ADR-084).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MonitorFacts {
    free: u64,
    total: u64,
    kb_ev: u64,
    pt_ev: u64,
    posted: u64,
    dropped: u64,
    refused: u64,
    windows: u64,
    closes: u64,
    drags: u64,
    focus: u64,
}

/// The machine's live desktop: its devices, its decoders, its compositor and input session, its
/// managed windows, their grids, and the GPU resource it composes into.
pub struct Desktop<H: VirtioHal, T: Transport + ConfigWrite> {
    kb: VirtioInput<H, T>,
    tab: VirtioInput<H, T>,
    kb_dec: KeyDecoder,
    pt_dec: PointerDecoder,
    comp: Compositor,
    sess: u64,
    wm: WindowManager,
    gpu: VirtioGpu<H, T>,
    pages: Vec<usize>,
    posted: u64,
    term: TextGrid,
    packed: Vec<u8>,
    mon: TextGrid,
    mon_packed: Vec<u8>,
    mon_sig: MonitorFacts,
    /// Keystrokes drained from the terminal's queue, waiting for the console's `getc`.
    term_input: Vec<u8>,
    /// Where the pointer last was (mirrored from the cursor, so a press knows it).
    pointer: (u32, u32),
}

impl<H: VirtioHal, T: Transport + ConfigWrite> Desktop<H, T> {
    /// Bring the desktop up on a live GPU and a live keyboard/tablet pair: create the resource
    /// over the caller's backing pages, bind the scanout, mint the input session, open the two
    /// managed windows, focus the terminal, place the cursor, and hand the first frame to the
    /// device. Every failure is NAMED and the caller decides what to do about it — on all three
    /// targets a desktop that will not come up leaves the machine running on its serial console.
    ///
    /// # Safety
    /// The GPU and input devices must be live and exclusively the caller's, and `pages` must be
    /// [`PAGES`] identity-mapped frames the kernel owns for as long as the desktop lives.
    pub unsafe fn install(
        mut gpu: VirtioGpu<H, T>,
        kb: VirtioInput<H, T>,
        mut tab: VirtioInput<H, T>,
        pages: Vec<usize>,
    ) -> Result<Self, &'static str> {
        if pages.len() != PAGES {
            return Err("the desktop's backing store is not the scanout's page count");
        }
        gpu.create_resource_2d(DESKTOP_RID, W, H)
            .map_err(|_| "the desktop's GPU resource was refused")?;
        gpu.attach_backing(DESKTOP_RID, &pages)
            .map_err(|_| "the desktop's backing pages were refused")?;
        gpu.set_scanout(0, DESKTOP_RID)
            .map_err(|_| "the desktop's scanout bind was refused")?;

        let term = TextGrid::new(TERM_COLS, TERM_ROWS);
        let (tw, th) = term.pixel_size();
        let mon = TextGrid::new(MON_COLS, MON_ROWS);
        let (mw, mh) = mon.pixel_size();
        let mut comp = Compositor::new(0x0D3A_1D05, W, H);
        let sess = comp
            .open_input_session()
            .map_err(|_| "the desktop's input session was refused")?;
        let tok_panel = comp
            .mint_surface(PANEL, 400, 200)
            .map_err(|_| "the desktop's panel surface was refused")?;
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
        comp.attach(PANEL, tok_panel, 0, 0)
            .map_err(|_| "the panel placement was refused")?;

        let mut wm = WindowManager::new();
        let tok_win = wm
            .open(&mut comp, WINDOW, tw, th, WINDOW_X, WINDOW_Y)
            .map_err(|_| "the terminal window was refused")?;
        let tok_mon = wm
            .open(&mut comp, MONITOR, mw, mh, MON_X, MON_Y)
            .map_err(|_| "the monitor window was refused")?;
        let mut packed = Vec::new();
        term.render_packed(TITLE, &mut packed);
        comp.fill_packed(WINDOW, tok_win, &packed)
            .map_err(|_| "the terminal window's first paint was refused")?;
        let mut mon_packed = Vec::new();
        mon.render_packed(MON_TITLE, &mut mon_packed);
        comp.fill_packed(MONITOR, tok_mon, &mon_packed)
            .map_err(|_| "the monitor window's first paint was refused")?;
        comp.set_focus(sess, WINDOW)
            .map_err(|_| "focusing the terminal window was refused")?;
        comp.move_cursor(sess, W / 2, H / 2)
            .map_err(|_| "placing the cursor was refused")?;

        // The pointer's declared range, qualified exactly as `vinput_suite` qualifies it: a
        // device that will not say what its axes are cannot steer a cursor.
        let axis = tab.abs_info(vinput::ABS_X).zip(tab.abs_info(vinput::ABS_Y));
        let Some(((x_min, x_max), (y_min, y_max))) = axis else {
            return Err("the desktop's pointer device did not declare both absolute axes");
        };
        if x_min != 0 || y_min != 0 || x_max != y_max {
            return Err("the desktop's pointer axes do not match the qualified range");
        }
        let mut pt_dec = PointerDecoder::new(W, H);
        pt_dec.set_axis(x_max, y_max);

        let mut d = Desktop {
            kb,
            tab,
            kb_dec: KeyDecoder::new(),
            pt_dec,
            comp,
            sess,
            wm,
            gpu,
            pages,
            posted: 0,
            term,
            packed,
            mon,
            mon_packed,
            mon_sig: MonitorFacts::default(),
            term_input: Vec::with_capacity(TERM_INPUT_CAP),
            pointer: (W / 2, H / 2),
        };
        d.show_frame()?;
        Ok(d)
    }

    /// The GPU function's FULL grant list — captured after the backing attach, so a platform
    /// that builds an IOMMU window from it covers every page the desktop's frames will write.
    pub fn gpu_grants(&self) -> Vec<crate::dma::Grant> {
        self.gpu.dma_grants()
    }

    /// Raw events each device has delivered since boot.
    pub fn device_events(&self) -> (u64, u64) {
        (self.kb.events_seen(), self.tab.events_seen())
    }

    /// Windows the manager holds open right now.
    pub fn window_count(&self) -> usize {
        self.wm.count()
    }

    /// Compose one frame through the real raster; hand it to the device ONLY if it changed.
    fn show_frame(&mut self) -> Result<u64, &'static str> {
        let mut surf = Surface::new(&self.pages, W, H)
            .map_err(|_| "the backing pages did not form a raster")?;
        let mut sink = ComposeSink::new(&mut surf);
        let st = self.comp.compose_frame(&mut sink);
        if sink.refusals() != 0 {
            return Err("the real raster refused a put the model's bounds allowed");
        }
        if st.pixels_blitted == 0 {
            return Ok(0); // a quiet frame moves nothing — no device traffic at all
        }
        // SAFETY: the device is live and ours; the rect is the resource's own full extent.
        unsafe {
            self.gpu
                .transfer_to_host_2d(DESKTOP_RID, GpuRect::covering(W, H))
                .map_err(|_| "the desktop frame's TRANSFER was refused")?;
            self.gpu
                .resource_flush(DESKTOP_RID, GpuRect::covering(W, H))
                .map_err(|_| "the desktop frame's FLUSH was refused")?;
        }
        Ok(st.pixels_blitted)
    }

    /// Repaint any window whose grid changed, then show a frame if anything owes a repaint.
    pub fn repaint(&mut self) {
        if self.term.take_dirty() {
            if let Some(tok) = self.wm.token(WINDOW) {
                self.term.render_packed(TITLE, &mut self.packed);
                let _ = self.comp.fill_packed(WINDOW, tok, &self.packed);
            }
        }
        if self.mon.take_dirty() {
            if let Some(tok) = self.wm.token(MONITOR) {
                self.mon.render_packed(MON_TITLE, &mut self.mon_packed);
                let _ = self.comp.fill_packed(MONITOR, tok, &self.mon_packed);
            }
        }
        if self.comp.has_pending_damage() {
            let _ = self.show_frame();
        }
    }

    /// Repaint the monitor from what the machine knows about itself — but ONLY when one of
    /// those facts CHANGED (ADR-084). A panel on a timer would end the quiet desktop.
    fn refresh_monitor(&mut self, frames_free: usize, frames_total: usize) {
        if !self.wm.is_open(MONITOR) {
            return;
        }
        let (dropped, refused) = self.comp.input_counters();
        let (_, closes, drags, wm_refusals) = self.wm.counters();
        let sig = MonitorFacts {
            free: frames_free as u64,
            total: frames_total as u64,
            kb_ev: self.kb.events_seen(),
            pt_ev: self.tab.events_seen(),
            posted: self.posted,
            dropped,
            refused: refused + wm_refusals,
            windows: self.wm.count() as u64,
            closes,
            drags,
            focus: self.comp.focus().unwrap_or(0) as u64,
        };
        if sig == self.mon_sig {
            return;
        }
        self.mon_sig = sig;
        self.mon.clear();
        let _ = write!(
            self.mon,
            "frames  {} of {} free\ninput   kb {} pt {} posted {}\nlost    {} dropped, {} refused\nwindows {} open, {} closed\ndrags   {}\nfocus   {}\n",
            sig.free, sig.total, sig.kb_ev, sig.pt_ev, sig.posted, sig.dropped, sig.refused,
            sig.windows, sig.closes, sig.drags, sig.focus
        );
    }

    /// A pointer batch, decided by the WINDOW MANAGER (ADR-084): the cursor already moved (the
    /// session's own plane); the press is the manager's call.
    fn pointer_batch(&mut self, batch: vinput::PointerBatch) {
        if let Some(p) = batch.move_to {
            self.pointer = p;
        }
        let (px, py) = self.pointer;
        if let Some((Button::Left, down)) = batch.button {
            if down {
                // A closed terminal takes its unread keystrokes with it: bytes the console had
                // not read belonged to a window that no longer exists.
                if let Press::Closed(WINDOW) = self.wm.press(&mut self.comp, self.sess, px, py) {
                    self.term_input.clear();
                }
            } else {
                let _ = self.wm.release();
            }
        }
        if batch.move_to.is_some() {
            let _ = self.wm.motion(&mut self.comp, px, py);
        }
    }

    /// Drain the terminal window's queue into the console's input line (owner token only).
    pub fn drain_window(&mut self) {
        if self.term_input.len() >= TERM_INPUT_CAP {
            return;
        }
        let Some(tok) = self.wm.token(WINDOW) else {
            return; // the terminal window was closed: there is no queue to drain
        };
        // Allocation-free (ADR-086): the pump drains every tick, and a `Vec` per tick on a heap
        // that never frees is a leak with a nicer name. The storm suite holds this claim.
        while let Ok(Some(e)) = self.comp.pop_input(WINDOW, tok) {
            if let EventKind::Key(b) = e.kind {
                if self.term_input.len() < TERM_INPUT_CAP {
                    self.term_input.push(b);
                } else {
                    break;
                }
            }
            if self.term_input.len() >= TERM_INPUT_CAP {
                break;
            }
        }
    }

    /// ONE tick of the desktop: drain both devices through the session, let the manager decide
    /// what the pointer did, hand the terminal's keystrokes to the console's line, refresh the
    /// monitor if a fact changed, and show a frame if anything owes a repaint. The frame
    /// allocator's reading is the platform's to report; everything else is this module's.
    ///
    /// # Safety
    /// The devices must still be live (they were at `install`), and the caller must guarantee
    /// no other reference to this desktop exists for the duration (its own interrupt posture).
    pub unsafe fn pump(&mut self, frames_free: usize, frames_total: usize) {
        for _ in 0..EVENTS_PER_TICK {
            match self.kb.next_event() {
                Some(ev) => {
                    if let Ok(nb) =
                        vinput::route_key(&mut self.kb_dec, &mut self.comp, self.sess, ev)
                    {
                        self.posted += nb as u64;
                    }
                }
                None => break,
            }
        }
        for _ in 0..EVENTS_PER_TICK {
            match self.tab.next_event() {
                Some(ev) => {
                    if let Ok(batch) = vinput::route_pointer_motion(
                        &mut self.pt_dec,
                        &mut self.comp,
                        self.sess,
                        ev,
                    ) {
                        self.pointer_batch(batch);
                    }
                }
                None => break,
            }
        }
        self.drain_window();
        self.refresh_monitor(frames_free, frames_total);
        self.repaint();
    }

    /// The console's output reaches the terminal window (ADR-083). A window a user closed is
    /// not a place the console still writes.
    pub fn term_write(&mut self, bytes: &[u8]) {
        if !self.wm.is_open(WINDOW) {
            return;
        }
        self.term.write(bytes);
        self.repaint();
    }

    /// One keystroke the input session routed into the terminal window, if any.
    pub fn term_getc(&mut self) -> Option<u8> {
        self.drain_window();
        if self.term_input.is_empty() {
            None
        } else {
            Some(self.term_input.remove(0))
        }
    }

    /// Everything the console's `input` command reports, read from the live model.
    pub fn facts(&self) -> InputFacts {
        let (dropped, refusals) = self.comp.input_counters();
        let (_, closes, drags, wm_refusals) = self.wm.counters();
        let queued = match self.comp.focus() {
            Some(id) => self.comp.queued_len(id),
            None => 0,
        };
        let mut term_last = [0u8; 48];
        let line = self.term.last_nonblank_line();
        let n = line.len().min(term_last.len());
        term_last[..n].copy_from_slice(&line[..n]);
        InputFacts {
            events_posted: self.posted,
            dropped,
            refusals: refusals + wm_refusals,
            queued,
            cursor: self.comp.cursor(),
            focus: self.comp.focus(),
            kb_events: self.kb.events_seen(),
            pt_events: self.tab.events_seen(),
            window: self.comp.placement(WINDOW),
            term_lines: self.term.lines(),
            term_last,
            term_last_len: n as u8,
            windows: self.wm.count(),
            closes,
            drags,
        }
    }
}
