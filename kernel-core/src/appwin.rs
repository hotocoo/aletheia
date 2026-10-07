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
    presents: u64,
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
            presents: 0,
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
        (w, h): (u32, u32),
        at: (i32, i32),
        fill: &mut dyn FnMut(&mut [u8]) -> bool,
    ) -> Result<(), Refusal> {
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
        }
        if !fill(&mut self.packed) {
            return Err(Refusal::BadBuffer);
        }
        comp.fill_packed(APP, self.token, &self.packed)
            .map_err(|_| Refusal::BadBuffer)?;
        self.presents += 1;
        Ok(())
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
                (16, 8),
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
            app.present(&mut wm, &mut comp, s, 8, (16, 8), (0, 0), &mut solid(0)),
            Err(Refusal::Busy)
        );
        assert_eq!(
            app.present(&mut wm, &mut comp, s, 7, (24, 8), (0, 0), &mut solid(0)),
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
            app.present(&mut wm, &mut comp, s, 8, (8, 8), (0, 0), &mut solid(1)),
            Ok(())
        );
    }

    #[test]
    fn sizes_buffers_and_an_operator_close_are_refused_by_name() {
        let (mut wm, mut comp, s) = setup();
        let mut app = AppWindow::new();
        for size in [(0, 8), (8, 0), (MAX_W + 1, 8), (8, MAX_H + 1)] {
            assert_eq!(
                app.present(&mut wm, &mut comp, s, 1, size, (0, 0), &mut solid(0)),
                Err(Refusal::BadSize)
            );
        }
        assert_eq!(
            app.present(&mut wm, &mut comp, s, 1, (8, 8), (0, 0), &mut |_| false),
            Err(Refusal::BadBuffer)
        );
        let mut seen = 0;
        assert_eq!(
            app.present(&mut wm, &mut comp, s, 1, (13, 5), (0, 0), &mut |b| {
                seen = b.len();
                true
            }),
            Ok(())
        );
        assert_eq!(seen, packed_len(13, 5));
        // The operator closes the window: the program's next presents are refused, and the
        // window does not come back until the program ends.
        wm.close(&mut comp, s, APP).unwrap();
        for _ in 0..2 {
            assert_eq!(
                app.present(&mut wm, &mut comp, s, 1, (13, 5), (0, 0), &mut solid(0)),
                Err(Refusal::Dismissed)
            );
        }
        assert!(!wm.is_open(APP));
        app.close(&mut wm, &mut comp, s, 1);
        assert_eq!(
            app.present(&mut wm, &mut comp, s, 2, (13, 5), (0, 0), &mut solid(0)),
            Ok(())
        );
    }
}
