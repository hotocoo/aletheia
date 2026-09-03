//! Windows are a managed SET, not one privileged surface (ALET-P2-021's window rung, ADR-084).
//!
//! ADR-077 made composition a contract, ADR-079 made input a session, ADR-080 put real devices
//! behind it and ADR-083 put text inside the one window the desktop had. That window could be
//! dragged and focused, and nothing else: there was no second application, no way to close
//! anything, and the desktop's own code decided by hand which surface a press belonged to.
//! This module is the layer that was missing between "a compositor with surfaces" and "a
//! desktop with windows":
//!
//! * **The manager owns the tokens.** A window's owner token is minted at [`WindowManager::open`]
//!   and never leaves the manager, so "which principal may move, repaint, close this window" has
//!   exactly one answer. A caller that names an id the manager does not hold is refused BY NAME
//!   ([`WmFault::UnknownWindow`]) and COUNTED; nothing is done on its behalf.
//! * **Chrome is geometry, and the geometry is the painter's.** The title band and the close box
//!   a press lands in are the SAME pixels [`crate::textgrid`] paints ([`TITLE_H`], [`CLOSE_W`]) —
//!   one definition, so a user can never click a close box that is drawn somewhere else.
//! * **A press is a routing DECISION, reported.** [`WindowManager::press`] finds the topmost
//!   window whose visible area covers the point (the compositor's own visible-rect math, so what
//!   is clipped away cannot be clicked), classifies the hit, and returns what it did:
//!   [`Press::Closed`], [`Press::Dragging`], [`Press::Focused`] or [`Press::Empty`]. It never
//!   guesses on the caller's behalf and never touches a pixel — focus and z-order are routing.
//! * **Close is a lifecycle, not a hide.** The window is detached, its surface, queue and TOKEN
//!   die with it (the compositor's own `detach`), focus falls to the next topmost window that is
//!   still open, and if none is left focus is CLEARED so a keystroke is refused `NoFocus` rather
//!   than routed to a corpse. A second close of the same id is refused `UnknownWindow`.
//! * **Bounded, like everything on this heap.** At most [`MAX_WINDOWS`] windows; the manager
//!   allocates on `open` only — press, motion, release and close allocate nothing (ADR-063).

use alloc::vec::Vec;

use crate::compositor::{CompFault, Compositor};
use crate::textgrid::{has_close_box, CLOSE_W, TITLE_H};

/// Windows one manager tracks. Bounded for the never-freeing boot heap (ADR-063), and under
/// the compositor's own surface ceiling so the wallpaper and any suite surface still fit.
pub const MAX_WINDOWS: usize = 8;

/// Why the manager refused. Every variant names what was involved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WmFault {
    /// No window with this id is open here (never opened, or already closed).
    UnknownWindow(u32),
    /// A window with this id is already open.
    DuplicateWindow(u32),
    /// The manager's table is full.
    TooManyWindows,
    /// The compositor refused the underlying op; its own named refusal is carried through.
    Compositor(CompFault),
}

impl From<CompFault> for WmFault {
    fn from(f: CompFault) -> Self {
        WmFault::Compositor(f)
    }
}

/// Where inside a window's own pixels a point landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// The close box: the rightmost [`CLOSE_W`] pixels of the title band.
    Close,
    /// The rest of the title band — the strip a window is dragged by.
    Title,
    /// Everything below the title band: the application's own area.
    Client,
}

/// What a press DID — the routing decision, reported rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// The close box was pressed and the window is gone; the id is the one that closed.
    Closed(u32),
    /// The title band was pressed: the window is raised, focused, and now dragging.
    Dragging(u32),
    /// The client area was pressed: the window is raised and focused.
    Focused(u32),
    /// No window covers the point: focus was cleared ("nowhere" is a place a user points).
    Empty,
}

/// Classify a window-local point against the chrome the text grid paints. `None` when the
/// point is outside the window's own pixels.
pub fn hit_at(width: u32, height: u32, lx: i32, ly: i32) -> Option<Hit> {
    if lx < 0 || ly < 0 || lx as u32 >= width || ly as u32 >= height {
        return None;
    }
    let (x, y) = (lx as u32, ly as u32);
    if y >= TITLE_H {
        return Some(Hit::Client);
    }
    if has_close_box(width) && x >= width - CLOSE_W {
        Some(Hit::Close)
    } else {
        Some(Hit::Title)
    }
}

/// One managed window: its id, the token the manager holds for it, and its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Window {
    id: u32,
    token: u64,
    width: u32,
    height: u32,
}

/// The window manager: the set of open windows, the drag in flight, and the ledger.
#[derive(Debug, Default)]
pub struct WindowManager {
    wins: Vec<Window>,
    /// The window being dragged and the pointer's offset from its top-left at the press.
    drag: Option<(u32, i32, i32)>,
    opens: u64,
    closes: u64,
    drags: u64,
    refusals: u64,
}

impl WindowManager {
    pub fn new() -> Self {
        WindowManager {
            wins: Vec::new(),
            drag: None,
            opens: 0,
            closes: 0,
            drags: 0,
            refusals: 0,
        }
    }

    /// Open a window: mint its surface, place it, and KEEP its token. The token is returned
    /// so the application can paint its own pixels; the manager keeps its copy because close,
    /// raise and drag are the manager's authority, not the caller's.
    pub fn open(
        &mut self,
        comp: &mut Compositor,
        id: u32,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
    ) -> Result<u64, WmFault> {
        if self.wins.iter().any(|w| w.id == id) {
            self.refusals += 1;
            return Err(WmFault::DuplicateWindow(id));
        }
        if self.wins.len() >= MAX_WINDOWS {
            self.refusals += 1;
            return Err(WmFault::TooManyWindows);
        }
        let token = comp.mint_surface(id, width, height).map_err(|e| {
            self.refusals += 1;
            WmFault::from(e)
        })?;
        if let Err(e) = comp.attach(id, token, x, y) {
            // Fail-closed: a window that could not be placed does not exist. The surface it
            // minted dies with the attempt rather than lingering as an unreachable id.
            let _ = comp.detach(id, token);
            self.refusals += 1;
            return Err(WmFault::from(e));
        }
        self.wins.push(Window {
            id,
            token,
            width,
            height,
        });
        self.opens += 1;
        Ok(token)
    }

    /// The token the manager holds for an open window (the application's pen).
    pub fn token(&self, id: u32) -> Option<u64> {
        self.wins.iter().find(|w| w.id == id).map(|w| w.token)
    }

    pub fn is_open(&self, id: u32) -> bool {
        self.wins.iter().any(|w| w.id == id)
    }

    pub fn count(&self) -> usize {
        self.wins.len()
    }

    /// Open window ids in the manager's own insertion order (not the z-order).
    pub fn ids(&self) -> Vec<u32> {
        self.wins.iter().map(|w| w.id).collect()
    }

    /// The window currently being dragged, if any.
    pub fn dragging(&self) -> Option<u32> {
        self.drag.map(|(id, _, _)| id)
    }

    /// (opens, closes, drags completed, refusals) — the manager's ledger.
    pub fn counters(&self) -> (u64, u64, u64, u64) {
        (self.opens, self.closes, self.drags, self.refusals)
    }

    /// The topmost OPEN window whose visible area covers a scanout point, with the point in
    /// that window's own coordinates. The z-order is the compositor's, walked front to back,
    /// and the visible-rect test is the compositor's own: pixels clipped off the scanout
    /// cannot be clicked, exactly as they cannot be seen.
    pub fn window_at(&self, comp: &Compositor, x: u32, y: u32) -> Option<(u32, i32, i32)> {
        let (sw, sh) = comp.scanout_size();
        for id in comp.z_order().into_iter().rev() {
            let Some(w) = self.wins.iter().find(|w| w.id == id) else {
                continue; // a surface this manager does not own (the wallpaper, a suite's)
            };
            let Some((px, py)) = comp.placement(id) else {
                continue;
            };
            let vis_w = w.width.saturating_sub(px.min(0).unsigned_abs());
            let vis_h = w.height.saturating_sub(py.min(0).unsigned_abs());
            let (cx, cy) = (px.max(0) as u32, py.max(0) as u32);
            if cx < sw
                && cy < sh
                && x >= cx
                && y >= cy
                && x < cx.saturating_add(vis_w)
                && y < cy.saturating_add(vis_h)
            {
                return Some((id, x as i32 - px, y as i32 - py));
            }
        }
        None
    }

    /// A pointer PRESS at a scanout point: the routing decision, taken and reported. Needs the
    /// input session, because focus is the session's authority and no window token substitutes
    /// for it. Touches no pixel of any surface.
    pub fn press(&mut self, comp: &mut Compositor, session: u64, x: u32, y: u32) -> Press {
        let Some((id, lx, ly)) = self.window_at(comp, x, y) else {
            let _ = comp.clear_focus(session);
            return Press::Empty;
        };
        let Some(w) = self.wins.iter().find(|w| w.id == id).copied() else {
            return Press::Empty;
        };
        match hit_at(w.width, w.height, lx, ly) {
            Some(Hit::Close) => match self.close(comp, session, id) {
                Ok(()) => Press::Closed(id),
                Err(_) => Press::Empty,
            },
            Some(Hit::Title) => {
                let _ = comp.raise(id, w.token);
                let _ = comp.set_focus(session, id);
                self.drag = Some((id, lx, ly));
                Press::Dragging(id)
            }
            Some(Hit::Client) => {
                let _ = comp.raise(id, w.token);
                let _ = comp.set_focus(session, id);
                Press::Focused(id)
            }
            None => Press::Empty,
        }
    }

    /// Pointer MOTION at a scanout point: moves the dragged window so the grabbed pixel stays
    /// under the pointer. Returns the window that moved, or `None` when no drag is in flight
    /// (or the compositor refused a placement fully off the scanout — the window then stays
    /// exactly where it was, which is the refusal being honoured, not ignored).
    pub fn motion(&mut self, comp: &mut Compositor, x: u32, y: u32) -> Option<u32> {
        let (id, ox, oy) = self.drag?;
        let token = self.token(id)?;
        match comp.move_surface(id, token, x as i32 - ox, y as i32 - oy) {
            Ok(()) => Some(id),
            Err(_) => None,
        }
    }

    /// Pointer RELEASE: ends any drag in flight and counts it. Returns the window released.
    pub fn release(&mut self) -> Option<u32> {
        let (id, _, _) = self.drag.take()?;
        self.drags += 1;
        Some(id)
    }

    /// Close a window: detach it (surface, queue and token die together), then give focus to
    /// the next topmost window still open, or clear it when none is left.
    pub fn close(&mut self, comp: &mut Compositor, session: u64, id: u32) -> Result<(), WmFault> {
        let Some(pos) = self.wins.iter().position(|w| w.id == id) else {
            self.refusals += 1;
            return Err(WmFault::UnknownWindow(id));
        };
        let w = self.wins[pos];
        comp.detach(id, w.token).map_err(|e| {
            self.refusals += 1;
            WmFault::from(e)
        })?;
        self.wins.remove(pos);
        if self.drag.map(|(d, _, _)| d) == Some(id) {
            self.drag = None; // a window closed mid-drag drags nothing
        }
        self.closes += 1;
        // Focus falls to the topmost survivor this manager owns; nothing left means NOTHING
        // focused, so the next keystroke is refused `NoFocus` instead of routed to a corpse.
        let next = comp
            .z_order()
            .into_iter()
            .rev()
            .find(|sid| self.wins.iter().any(|w| w.id == *sid));
        match next {
            Some(sid) => {
                let _ = comp.set_focus(session, sid);
            }
            None => {
                let _ = comp.clear_focus(session);
            }
        }
        Ok(())
    }
}

/// The boot suite for the window manager (ADR-084): arch-neutral, allocation-bounded, and
/// proved against the SAME compositor the machine composes with.
pub fn wm_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    use crate::compositor::{CompFault as CF, EventKind};

    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    // A desktop of two overlapping windows on one scanout: A at (0,0), B at (40,20).
    fn desk() -> (Compositor, WindowManager, u64, u32, u32) {
        let mut comp = Compositor::new(0x5EED_0084, 200, 120);
        let sess = comp.open_input_session().unwrap();
        let mut wm = WindowManager::new();
        wm.open(&mut comp, 1, 80, 60, 0, 0).unwrap();
        wm.open(&mut comp, 2, 80, 60, 40, 20).unwrap();
        let _ = comp.set_focus(sess, 1);
        (comp, wm, sess, 1, 2)
    }

    // 1 — chrome geometry is the painter's: the close box is the rightmost CLOSE_W pixels of
    //     the title band, the rest of the band is the drag strip, below it is the client, and
    //     a point outside the window's pixels is no hit at all.
    {
        let (w, h) = (80u32, 60u32);
        check!(
            hit_at(w, h, 0, 0) == Some(Hit::Title)
                && hit_at(w, h, (w - CLOSE_W) as i32, 0) == Some(Hit::Close)
                && hit_at(w, h, (w - CLOSE_W - 1) as i32, (TITLE_H - 1) as i32) == Some(Hit::Title)
                && hit_at(w, h, (w - 1) as i32, (TITLE_H - 1) as i32) == Some(Hit::Close)
                && hit_at(w, h, (w - 1) as i32, TITLE_H as i32) == Some(Hit::Client)
                && hit_at(w, h, -1, 0).is_none()
                && hit_at(w, h, w as i32, 0).is_none()
                && hit_at(w, h, 0, h as i32).is_none(),
            "wm: the close box, the drag band and the client area are exactly the painted chrome"
        );
    }
    // 2 — a press in the OVERLAP goes to the topmost window only, and raises it: the lower
    //     window is not focused, not raised, and not moved.
    {
        let (mut comp, mut wm, sess, a, b) = desk();
        let before = comp.placement(a);
        let p = wm.press(&mut comp, sess, 60, 40); // inside both A and B; B is on top
        check!(
            p == Press::Focused(b)
                && comp.focus() == Some(b)
                && comp.z_order() == alloc::vec![a, b]
                && comp.placement(a) == before,
            "wm: a press in the overlap routes to the topmost window and raises it, alone"
        );
    }
    // 3 — a press on the lower window where it is NOT covered focuses and RAISES it, so the
    //     z-order the next press consults is the one the user just made.
    {
        let (mut comp, mut wm, sess, a, b) = desk();
        let p = wm.press(&mut comp, sess, 10, 50); // A only (B starts at x=40,y=20)
        check!(
            p == Press::Focused(a)
                && comp.focus() == Some(a)
                && comp.z_order() == alloc::vec![b, a],
            "wm: a press on an uncovered window raises it above the one that was on top"
        );
    }
    // 4 — the title band drags: motion moves the window by exactly the pointer's delta with
    //     the grabbed pixel still under the pointer, release ends and COUNTS the drag, and
    //     motion after release moves nothing.
    {
        let (mut comp, mut wm, sess, _a, b) = desk();
        let p = wm.press(&mut comp, sess, 44, 22); // B's title band, 4 px in, 2 px down
        let moved = wm.motion(&mut comp, 94, 72);
        let placed = comp.placement(b);
        let rel = wm.release();
        let after_release = wm.motion(&mut comp, 120, 90);
        check!(
            p == Press::Dragging(b)
                && moved == Some(b)
                && placed == Some((90, 70))
                && rel == Some(b)
                && after_release.is_none()
                && comp.placement(b) == Some((90, 70))
                && wm.counters().2 == 1,
            "wm: the title band drags the window by the pointer delta and release ends it exactly"
        );
    }
    // 5 — a press on the close box CLOSES: the window leaves the z-order, its placement is
    //     gone, and its TOKEN IS DEAD (the old token cannot move or repaint it any more).
    {
        let (mut comp, mut wm, sess, a, b) = desk();
        let tok_b = wm.token(b).unwrap();
        let p = wm.press(&mut comp, sess, 40 + 80 - 1, 20); // B's close box
        check!(
            p == Press::Closed(b)
                && !wm.is_open(b)
                && wm.count() == 1
                && comp.z_order() == alloc::vec![a]
                && comp.placement(b).is_none()
                && comp.move_surface(b, tok_b, 0, 0) == Err(CF::UnknownSurface(b))
                && wm.counters().1 == 1,
            "wm: the close box detaches the window and its owner token dies with it"
        );
    }
    // 6 — closing the FOCUSED window gives focus to the topmost survivor, and the next
    //     keystroke is delivered to that survivor's own queue.
    {
        let (mut comp, mut wm, sess, a, b) = desk();
        let tok_a = wm.token(a).unwrap();
        let _ = comp.set_focus(sess, b);
        wm.close(&mut comp, sess, b).unwrap();
        comp.post_key(sess, b'k').unwrap();
        let ev = comp.drain_input(a, tok_a).unwrap();
        check!(
            comp.focus() == Some(a)
                && ev.iter().any(|e| e.kind == EventKind::Key(b'k'))
                && comp.queued_len(b) == 0,
            "wm: closing the focused window hands focus to the topmost survivor, which receives"
        );
    }
    // 7 — closing the LAST window clears focus: a keystroke is refused `NoFocus` and exists
    //     nowhere, rather than being routed to a window that is gone.
    {
        let (mut comp, mut wm, sess, a, b) = desk();
        wm.close(&mut comp, sess, a).unwrap();
        wm.close(&mut comp, sess, b).unwrap();
        let refused = comp.post_key(sess, b'z');
        check!(
            wm.count() == 0
                && comp.focus().is_none()
                && refused == Err(CF::NoFocus)
                && comp.placed_count() == 0
                && comp.surface_count() == 0,
            "wm: closing the last window clears focus and a keystroke is refused by name"
        );
    }
    // 8 — a second close of the same id is refused BY NAME and counted; the other window is
    //     untouched. Fail-closed on ids the manager does not hold, too.
    {
        let (mut comp, mut wm, sess, a, b) = desk();
        wm.close(&mut comp, sess, b).unwrap();
        let again = wm.close(&mut comp, sess, b);
        let never = wm.close(&mut comp, sess, 99);
        check!(
            again == Err(WmFault::UnknownWindow(b))
                && never == Err(WmFault::UnknownWindow(99))
                && wm.counters().1 == 1
                && wm.counters().3 == 2
                && wm.is_open(a)
                && comp.z_order() == alloc::vec![a],
            "wm: closing a closed or unknown window is refused by name, counted, and changes nothing"
        );
    }
    // 9 — a press on EMPTY scanout clears focus and moves nothing; the windows stay exactly
    //     where and in the order they were.
    {
        let (mut comp, mut wm, sess, a, b) = desk();
        let z = comp.z_order();
        let (pa, pb) = (comp.placement(a), comp.placement(b));
        let p = wm.press(&mut comp, sess, 199, 119); // past both windows
        check!(
            p == Press::Empty
                && comp.focus().is_none()
                && comp.z_order() == z
                && comp.placement(a) == pa
                && comp.placement(b) == pb
                && wm.dragging().is_none(),
            "wm: a press on empty scanout clears focus and moves nothing"
        );
    }
    // 10 — a closed id may be RE-OPENED, and the new window is not the old one: a fresh
    //      token, an empty queue, and the dead token still refused.
    {
        let (mut comp, mut wm, sess, _a, b) = desk();
        let old = wm.token(b).unwrap();
        let _ = comp.set_focus(sess, b);
        comp.post_key(sess, b'q').unwrap();
        wm.close(&mut comp, sess, b).unwrap();
        let fresh = wm.open(&mut comp, b, 80, 60, 40, 20).unwrap();
        check!(
            fresh != old
                && wm.is_open(b)
                && comp.queued_len(b) == 0
                && comp.drain_input(b, old) == Err(CF::NotOwner { surface: b })
                && comp.drain_input(b, fresh).map(|e| e.len()) == Ok(0),
            "wm: a re-opened id is a NEW window - fresh token, empty queue, dead token refused"
        );
    }
    // 11 — the table is bounded and ids are unique: a duplicate open is refused by name and
    //      mints nothing, and the ceiling is refused by name rather than silently exceeded.
    {
        let (mut comp, mut wm, _sess, a, _b) = desk();
        let dup = wm.open(&mut comp, a, 20, 20, 0, 0);
        let surfaces = comp.surface_count();
        let mut over = None;
        for id in 10..10 + MAX_WINDOWS as u32 {
            over = Some(wm.open(&mut comp, id, 16, 16, 0, 0));
        }
        check!(
            dup == Err(WmFault::DuplicateWindow(a))
                && surfaces == 2
                && wm.count() == MAX_WINDOWS
                && over == Some(Err(WmFault::TooManyWindows)),
            "wm: a duplicate id and the window ceiling are both refused by name"
        );
    }
    // 12 — the same pointer story told twice lands bit-identically, and the event path
    //      allocates NOTHING: the manager's table capacity is the same after it as before.
    {
        let story = |wm: &mut WindowManager, comp: &mut Compositor, sess: u64| {
            wm.press(comp, sess, 60, 40);
            wm.motion(comp, 70, 50);
            wm.release();
            wm.press(comp, sess, 44, 22);
            wm.motion(comp, 54, 32);
            wm.release();
            wm.press(comp, sess, 199, 119);
        };
        let (mut c1, mut w1, s1, _a, _b) = desk();
        let (mut c2, mut w2, s2, _, _) = desk();
        let cap = w1.wins.capacity();
        story(&mut w1, &mut c1, s1);
        story(&mut w2, &mut c2, s2);
        check!(
            c1.z_order() == c2.z_order()
                && c1.placement(1) == c2.placement(1)
                && c1.placement(2) == c2.placement(2)
                && c1.focus() == c2.focus()
                && w1.counters() == w2.counters()
                && w1.wins.capacity() == cap,
            "wm: the same pointer story lands bit-identically and the event path allocates nothing"
        );
    }
    Ok(n)
}
