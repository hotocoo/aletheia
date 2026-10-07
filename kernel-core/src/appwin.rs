//! The window a running program draws into (ADR-215).
//!
//! A program hands the kernel a packed one-bit bitmap (`SYS_PRESENT`); the desktop shows it in one
//! managed window that belongs to that program alone. This module is the policy over the window
//! manager and the compositor, kept free of devices so the host can prove it:
//!
//! * **One owner at a time.** The window is opened by the first program that presents and is that
//!   program's until it ends ([`AppWindow::close`]); another program's present is refused.
//! * **Bounded.** At most [`MAX_W`] x [`MAX_H`] pixels; the buffer must be exactly the packed size.
//! * **The operator wins.** A window the operator closed stays closed: its owner's later presents
//!   are refused, so the program can notice and end, instead of the window springing back.
//! * **Its keys are its own (ADR-216).** The window takes the keyboard when it opens; what the
//!   operator types there queues on its surface, and only its owner can take it (`SYS_POLL_INPUT`).
//! * **Allocation per open, never per present.** The packed buffer is sized when the window opens
//!   (or changes size); a present only fills it.

use crate::compositor::Compositor;
use crate::wm::WindowManager;
use alloc::vec::Vec;

/// The surface id of the program window.
pub const APP: u32 = 10;
/// The largest program window, in pixels.
pub const MAX_W: u32 = 320;
pub const MAX_H: u32 = 200;

/// Why a present was refused. Each one is a `u64::MAX` to the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Zero, or past [`MAX_W`] x [`MAX_H`].
    BadSize,
    /// Another program holds the window.
    Busy,
    /// The operator closed this program's window.
    Dismissed,
    /// The window manager would not open it (too many windows, no room).
    NoWindow,
    /// The program's bytes could not be gathered.
    BadBuffer,
}

/// The program window's state.
#[derive(Default)]
pub struct AppWindow {
    /// The program (its supervisor id) the window belongs to, and whether the operator closed it.
    owner: Option<(u64, bool)>,
    token: u64,
    size: (u32, u32),
    packed: Vec<u8>,
    /// The frame in RGB332 when the program presents colour (ADR-218); empty for one-bit frames.
    colour: Vec<u8>,
    presents: u64,
    /// Pointer events over the window, oldest first (ADR-217).
    pointer: [u64; POINTER_RING],
    pointer_len: usize,
    held: bool,
}

/// No event waiting.
pub const NO_EVENT: u64 = 0;
/// A key: `KEY | byte`, the byte in the console's decoded alphabet (the keymap's output).
pub const KEY: u64 = 0x100;
/// The window lost the keyboard to another.
pub const FOCUS_LOST: u64 = 0x200;

/// An input event as `SYS_POLL_INPUT` returns it (ADR-216). Never `u64::MAX`, which is a refusal.
pub fn encode(e: Option<crate::compositor::Event>) -> u64 {
    match e.map(|e| e.kind) {
        None => NO_EVENT,
        Some(crate::compositor::EventKind::Key(b)) => KEY | b as u64,
        Some(crate::compositor::EventKind::FocusLost) => FOCUS_LOST,
    }
}

/// A pointer event (ADR-217): `POINTER | x | y << 16`, plus [`HELD`] while the left button is
/// down after it and [`CLICK`] when this event is the button changing.
pub const POINTER: u64 = 1 << 40;
pub const HELD: u64 = 1 << 32;
pub const CLICK: u64 = 1 << 33;
/// Pointer events kept for a program that is not polling; moves coalesce into one.
const POINTER_RING: usize = 16;

/// What a program's frame is (ADR-218): its size, and whether it is one bit per pixel (packed,
/// the compositor's own format) or one byte per pixel in RGB332 (`RRRGGGBB`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSpec {
    pub width: u32,
    pub height: u32,
    pub colour: bool,
}

impl FrameSpec {
    pub const fn mono(width: u32, height: u32) -> Self {
        FrameSpec {
            width,
            height,
            colour: false,
        }
    }

    /// Bytes the program's buffer holds for this frame.
    pub const fn len(&self) -> usize {
        if self.colour {
            self.width as usize * self.height as usize
        } else {
            packed_len(self.width, self.height)
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// An RGB332 byte as the display's BGRA. Each channel's bits are repeated to fill eight, so
/// 0x00 is black and 0xFF is white exactly.
pub const fn rgb332_bgra(c: u8) -> [u8; 4] {
    let r = (c >> 5) & 7;
    let g = (c >> 2) & 7;
    let b = c & 3;
    [
        b * 0x55,
        (g << 5) | (g << 2) | (g >> 1),
        (r << 5) | (r << 2) | (r >> 1),
        0xFF,
    ]
}

/// Whether an RGB332 pixel is ink in the window's one-bit plane: bright enough to read as the
/// foreground (luminance at least half), so a one-bit readback of a colour frame still means
/// something.
pub const fn rgb332_ink(c: u8) -> bool {
    let [b, g, r, _] = rgb332_bgra(c);
    (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) >= 128 * 1000
}

/// The packed size of a `w` x `h` one-bit bitmap, LSB first, row-major (the compositor's format).
pub const fn packed_len(w: u32, h: u32) -> usize {
    (w as usize * h as usize).div_ceil(8)
}

impl AppWindow {
    pub const fn new() -> Self {
        AppWindow {
            owner: None,
            token: 0,
            size: (0, 0),
            packed: Vec::new(),
            colour: Vec::new(),
            presents: 0,
            pointer: [0; POINTER_RING],
            pointer_len: 0,
            held: false,
        }
    }

    /// Show `owner`'s next frame: `w` x `h` pixels, which `fill` writes into the buffer it is handed
    /// (exactly [`packed_len`] bytes) and confirms with `true`. Opens the window at `at` on the
    /// owner's first present, and reopens it at the same place when the size changes.
    #[allow(clippy::too_many_arguments)]
    pub fn present(
        &mut self,
        wm: &mut WindowManager,
        comp: &mut Compositor,
        session: u64,
        owner: u64,
        spec: FrameSpec,
        at: (i32, i32),
        fill: &mut dyn FnMut(&mut [u8]) -> bool,
    ) -> Result<(), Refusal> {
        let (w, h) = (spec.width, spec.height);
        if w == 0 || h == 0 || w > MAX_W || h > MAX_H {
            return Err(Refusal::BadSize);
        }
        match self.owner {
            Some((o, _)) if o != owner => return Err(Refusal::Busy),
            Some((_, true)) => return Err(Refusal::Dismissed),
            Some((_, false)) if !wm.is_open(APP) => {
                // Closed by the operator since the last present: it stays closed.
                self.owner = Some((owner, true));
                return Err(Refusal::Dismissed);
            }
            _ => {}
        }
        if self.owner.is_none() || self.size != (w, h) {
            let at = match self.owner {
                Some(_) => {
                    let here = (0..comp.placed_len())
                        .filter_map(|i| comp.placed_at(i))
                        .find(|&(id, _, _)| id == APP)
                        .map(|(_, x, y)| (x, y));
                    let _ = wm.close(comp, session, APP);
                    here.unwrap_or(at)
                }
                None => at,
            };
            let token = wm
                .open(comp, APP, w, h, at.0, at.1)
                .map_err(|_| Refusal::NoWindow)?;
            self.token = token;
            self.size = (w, h);
            self.packed.clear();
            self.packed.resize(packed_len(w, h), 0);
            self.owner = Some((owner, false));
            let _ = comp.raise(APP, token);
            // A window that opens takes the keyboard, as on every desktop (ADR-216).
            let _ = comp.set_focus(session, APP);
        }
        if spec.colour {
            // Allocated when the window opens or changes format, never per frame.
            if self.colour.len() != spec.len() {
                self.colour.clear();
                self.colour.resize(spec.len(), 0);
            }
            if !fill(&mut self.colour) {
                return Err(Refusal::BadBuffer);
            }
            self.packed.fill(0);
            for (i, &c) in self.colour.iter().enumerate() {
                if rgb332_ink(c) {
                    self.packed[i / 8] |= 1 << (i % 8);
                }
            }
        } else {
            self.colour = Vec::new();
            if !fill(&mut self.packed) {
                return Err(Refusal::BadBuffer);
            }
        }
        comp.fill_packed(APP, self.token, &self.packed)
            .map_err(|_| Refusal::BadBuffer)?;
        self.presents += 1;
        Ok(())
    }

    /// The next input event the operator gave `owner`'s window (ADR-216), encoded for a register:
    /// see [`encode`]. Refused when `owner` does not hold an open window.
    pub fn poll(
        &mut self,
        wm: &WindowManager,
        comp: &mut Compositor,
        owner: u64,
    ) -> Result<u64, Refusal> {
        match self.owner {
            Some((o, false)) if o == owner && wm.is_open(APP) => {}
            Some((o, _)) if o == owner => return Err(Refusal::Dismissed),
            Some(_) => return Err(Refusal::Busy),
            None => return Err(Refusal::NoWindow),
        }
        if self.pointer_len > 0 {
            let e = self.pointer[0];
            self.pointer.copy_within(1..self.pointer_len, 0);
            self.pointer_len -= 1;
            return Ok(e);
        }
        Ok(encode(comp.pop_input(APP, self.token).ok().flatten()))
    }

    /// The pointer moved to, or its left button changed at, `(x, y)` in the window's own
    /// coordinates (ADR-217). `button` is `Some(down)` for a press or release. A move right after
    /// a move replaces it, so a program that is slow to poll sees where the pointer is, not a
    /// backlog; a full ring drops the event. No allocation.
    pub fn pointer(&mut self, x: u32, y: u32, button: Option<bool>) {
        if self.owner.is_none_or(|(_, dismissed)| dismissed) {
            return;
        }
        if let Some(down) = button {
            self.held = down;
        }
        let e = POINTER
            | (x.min(0xFFFF) as u64)
            | ((y.min(0xFFFF) as u64) << 16)
            | if self.held { HELD } else { 0 }
            | if button.is_some() { CLICK } else { 0 };
        if button.is_none() && self.pointer_len > 0 {
            let last = &mut self.pointer[self.pointer_len - 1];
            if *last & CLICK == 0 {
                *last = e;
                return;
            }
        }
        if self.pointer_len < POINTER_RING {
            self.pointer[self.pointer_len] = e;
            self.pointer_len += 1;
        }
    }

    /// `owner` has ended: its window goes, and the next program may open one.
    pub fn close(
        &mut self,
        wm: &mut WindowManager,
        comp: &mut Compositor,
        session: u64,
        owner: u64,
    ) {
        if self.owner.map(|(o, _)| o) != Some(owner) {
            return;
        }
        if wm.is_open(APP) {
            let _ = wm.close(comp, session, APP);
        }
        self.owner = None;
        self.size = (0, 0);
        self.packed = Vec::new();
        self.colour = Vec::new();
        self.pointer_len = 0;
        self.held = false;
    }

    /// The colour plane of the window's current frame, `(width, height, RGB332 bytes)`, when the
    /// program presented colour (ADR-218).
    pub fn colour_plane(&self) -> Option<(u32, u32, &[u8])> {
        if self.owner.is_some() && !self.colour.is_empty() {
            Some((self.size.0, self.size.1, &self.colour))
        } else {
            None
        }
    }

    /// The program holding the window, if any.
    pub fn owner(&self) -> Option<u64> {
        self.owner.map(|(o, _)| o)
    }

    /// Frames shown since boot.
    pub fn presents(&self) -> u64 {
        self.presents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (WindowManager, Compositor, u64) {
        let mut comp = Compositor::new(0xA99, 640, 240);
        let session = comp.open_input_session().unwrap();
        (WindowManager::new(), comp, session)
    }

    fn solid(byte: u8) -> impl FnMut(&mut [u8]) -> bool {
        move |b: &mut [u8]| {
            b.fill(byte);
            true
        }
    }

    #[test]
    fn a_program_opens_its_window_owns_it_and_loses_it_when_it_ends() {
        let (mut wm, mut comp, s) = setup();
        let mut app = AppWindow::new();
        assert_eq!(
            app.present(
                &mut wm,
                &mut comp,
                s,
                7,
                FrameSpec::mono(16, 8),
                (40, 30),
                &mut solid(0xFF)
            ),
            Ok(())
        );
        assert!(wm.is_open(APP));
        assert_eq!(wm.size(APP), Some((16, 8)));
        assert_eq!(app.owner(), Some(7));
        // Another program is refused while 7 holds it; 7 may present again, and resize.
        assert_eq!(
            app.present(
                &mut wm,
                &mut comp,
                s,
                8,
                FrameSpec::mono(16, 8),
                (0, 0),
                &mut solid(0)
            ),
            Err(Refusal::Busy)
        );
        assert_eq!(
            app.present(
                &mut wm,
                &mut comp,
                s,
                7,
                FrameSpec::mono(24, 8),
                (0, 0),
                &mut solid(0)
            ),
            Ok(())
        );
        assert_eq!(wm.size(APP), Some((24, 8)));
        assert_eq!(app.presents(), 2);
        // A stranger cannot close it; its owner's end does.
        app.close(&mut wm, &mut comp, s, 8);
        assert!(wm.is_open(APP));
        app.close(&mut wm, &mut comp, s, 7);
        assert!(!wm.is_open(APP) && app.owner().is_none());
        // Now program 8 may have it.
        assert_eq!(
            app.present(
                &mut wm,
                &mut comp,
                s,
                8,
                FrameSpec::mono(8, 8),
                (0, 0),
                &mut solid(1)
            ),
            Ok(())
        );
    }

    #[test]
    fn sizes_buffers_and_an_operator_close_are_refused_by_name() {
        let (mut wm, mut comp, s) = setup();
        let mut app = AppWindow::new();
        for size in [(0, 8), (8, 0), (MAX_W + 1, 8), (8, MAX_H + 1)] {
            assert_eq!(
                app.present(
                    &mut wm,
                    &mut comp,
                    s,
                    1,
                    FrameSpec::mono(size.0, size.1),
                    (0, 0),
                    &mut solid(0)
                ),
                Err(Refusal::BadSize)
            );
        }
        assert_eq!(
            app.present(
                &mut wm,
                &mut comp,
                s,
                1,
                FrameSpec::mono(8, 8),
                (0, 0),
                &mut |_| false
            ),
            Err(Refusal::BadBuffer)
        );
        let mut seen = 0;
        assert_eq!(
            app.present(
                &mut wm,
                &mut comp,
                s,
                1,
                FrameSpec::mono(13, 5),
                (0, 0),
                &mut |b| {
                    seen = b.len();
                    true
                }
            ),
            Ok(())
        );
        assert_eq!(seen, packed_len(13, 5));
        // The operator closes the window: the program's next presents are refused, and the
        // window does not come back until the program ends.
        wm.close(&mut comp, s, APP).unwrap();
        for _ in 0..2 {
            assert_eq!(
                app.present(
                    &mut wm,
                    &mut comp,
                    s,
                    1,
                    FrameSpec::mono(13, 5),
                    (0, 0),
                    &mut solid(0)
                ),
                Err(Refusal::Dismissed)
            );
        }
        assert!(!wm.is_open(APP));
        app.close(&mut wm, &mut comp, s, 1);
        assert_eq!(
            app.present(
                &mut wm,
                &mut comp,
                s,
                2,
                FrameSpec::mono(13, 5),
                (0, 0),
                &mut solid(0)
            ),
            Ok(())
        );
    }

    #[test]
    fn keys_typed_at_the_window_reach_its_owner_and_nobody_else() {
        let (mut wm, mut comp, s) = setup();
        let mut app = AppWindow::new();
        assert_eq!(app.poll(&wm, &mut comp, 3), Err(Refusal::NoWindow));
        app.present(
            &mut wm,
            &mut comp,
            s,
            3,
            FrameSpec::mono(8, 8),
            (0, 0),
            &mut solid(0),
        )
        .unwrap();
        // The new window holds the keyboard: a key typed now queues on its surface.
        assert_eq!(comp.focus(), Some(APP));
        comp.post_key(s, b'q').unwrap();
        assert_eq!(app.poll(&wm, &mut comp, 4), Err(Refusal::Busy));
        assert_eq!(app.poll(&wm, &mut comp, 3), Ok(KEY | b'q' as u64));
        assert_eq!(app.poll(&wm, &mut comp, 3), Ok(NO_EVENT));
        // Closed by the operator: its owner's polls are refused like its presents.
        wm.close(&mut comp, s, APP).unwrap();
        assert_eq!(app.poll(&wm, &mut comp, 3), Err(Refusal::Dismissed));
        assert_ne!(encode(None), u64::MAX);
    }

    #[test]
    fn pointer_events_reach_the_owner_moves_coalesce_and_clicks_do_not() {
        let (mut wm, mut comp, s) = setup();
        let mut app = AppWindow::new();
        app.pointer(1, 1, None); // no window: ignored
        app.present(
            &mut wm,
            &mut comp,
            s,
            5,
            FrameSpec::mono(32, 32),
            (0, 0),
            &mut solid(0),
        )
        .unwrap();
        app.pointer(3, 4, None);
        app.pointer(5, 6, None); // replaces the move before it
        app.pointer(5, 6, Some(true));
        app.pointer(7, 8, None);
        app.pointer(7, 8, Some(false));
        assert_eq!(app.poll(&wm, &mut comp, 5), Ok(POINTER | 5 | 6 << 16));
        assert_eq!(
            app.poll(&wm, &mut comp, 5),
            Ok(POINTER | 5 | 6 << 16 | HELD | CLICK)
        );
        assert_eq!(
            app.poll(&wm, &mut comp, 5),
            Ok(POINTER | 7 | 8 << 16 | HELD)
        );
        assert_eq!(
            app.poll(&wm, &mut comp, 5),
            Ok(POINTER | 7 | 8 << 16 | CLICK)
        );
        assert_eq!(app.poll(&wm, &mut comp, 5), Ok(NO_EVENT));
        // A full ring keeps what it has; nothing is encoded as the refusal value.
        for i in 0..40 {
            app.pointer(i, i, Some(i % 2 == 0));
        }
        let mut n = 0;
        while let Ok(e) = app.poll(&wm, &mut comp, 5) {
            if e == NO_EVENT {
                break;
            }
            assert_ne!(e, u64::MAX);
            n += 1;
        }
        assert_eq!(n, POINTER_RING);
        assert_eq!(app.poll(&wm, &mut comp, 6), Err(Refusal::Busy));
    }

    #[test]
    fn a_colour_frame_keeps_its_colours_and_a_one_bit_plane_by_brightness() {
        let (mut wm, mut comp, s) = setup();
        let mut app = AppWindow::new();
        let spec = FrameSpec {
            width: 4,
            height: 2,
            colour: true,
        };
        let px = [0x00, 0xFF, 0xE0, 0x1C, 0x03, 0x92, 0x6D, 0xFF];
        assert_eq!(
            app.present(&mut wm, &mut comp, s, 1, spec, (0, 0), &mut |b| {
                assert_eq!(b.len(), 8);
                b.copy_from_slice(&px);
                true
            }),
            Ok(())
        );
        assert_eq!(app.colour_plane(), Some((4, 2, &px[..])));
        // White and the light grey are ink; black, pure blue and the dark grey are not.
        assert!(!comp.has_pixel(APP, 0, 0) && comp.has_pixel(APP, 1, 0));
        assert!(comp.has_pixel(APP, 3, 1) && !comp.has_pixel(APP, 0, 1));
        assert_eq!(rgb332_bgra(0x00), [0, 0, 0, 0xFF]);
        assert_eq!(rgb332_bgra(0xFF), [0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(rgb332_bgra(0xE0), [0, 0, 0xFF, 0xFF]);
        // A one-bit frame drops the colour plane.
        app.present(
            &mut wm,
            &mut comp,
            s,
            1,
            FrameSpec::mono(4, 2),
            (0, 0),
            &mut solid(0),
        )
        .unwrap();
        assert_eq!(app.colour_plane(), None);
    }
}
