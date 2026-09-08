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
use crate::textgrid::{
    has_resize_grip, has_window_controls, CLOSE_W, CONTROL_W, RESIZE_W, TITLE_H,
};

/// Windows one manager tracks. Bounded for the never-freeing boot heap (ADR-063), and under
/// the compositor's own surface ceiling so the wallpaper and any suite surface still fit.
pub const MAX_WINDOWS: usize = 8;
/// Pointer distance from a scanout edge that activates drag-to-snap on release.
const SNAP_EDGE_PX: u32 = 1;
/// Distance between successive windows in the deterministic cascade layout.
const CASCADE_OFFSET_PX: u32 = 32;

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
    /// The left title-bar control: minimize without destroying the window.
    Minimize,
    /// The middle title-bar control: maximize or restore the window.
    Maximize,
    /// The close box: the rightmost [`CLOSE_W`] pixels of the title band.
    Close,
    /// The rest of the title band — the strip a window is dragged by.
    Title,
    /// Everything below the title band: the application's own area.
    Client,
    /// A resize edge/corner. The value identifies which opposing edge stays fixed.
    Resize(ResizeEdge),
}

/// Which edge(s) a pointer resize owns. Corners resize both axes at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// What a press DID — the routing decision, reported rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// The minimize control was pressed and the window is now hidden.
    Minimized(u32),
    /// The maximize/restore control was pressed and the window changed geometry.
    Maximized(u32),
    /// The close box was pressed and the window is gone; the id is the one that closed.
    Closed(u32),
    /// The title band was pressed: the window is raised, focused, and now dragging.
    Dragging(u32),
    /// The bottom-right resize grip was pressed: the window is focused and now resizing.
    Resizing(u32),
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
        if has_resize_grip(width, height) {
            let near_left = x < RESIZE_W;
            let near_right = x >= width - RESIZE_W;
            let near_top = y - TITLE_H < RESIZE_W;
            let near_bottom = y >= height - RESIZE_W;
            return match (near_left, near_right, near_top, near_bottom) {
                (true, false, true, false) => Some(Hit::Resize(ResizeEdge::TopLeft)),
                (false, true, true, false) => Some(Hit::Resize(ResizeEdge::TopRight)),
                (true, false, false, true) => Some(Hit::Resize(ResizeEdge::BottomLeft)),
                (false, true, false, true) => Some(Hit::Resize(ResizeEdge::BottomRight)),
                (true, false, false, false) => Some(Hit::Resize(ResizeEdge::Left)),
                (false, true, false, false) => Some(Hit::Resize(ResizeEdge::Right)),
                (false, false, true, false) => Some(Hit::Resize(ResizeEdge::Top)),
                (false, false, false, true) => Some(Hit::Resize(ResizeEdge::Bottom)),
                _ => return Some(Hit::Client),
            };
        }
        return Some(Hit::Client);
    }
    if has_window_controls(width) {
        let controls = width - CONTROL_W * 3;
        if x >= controls {
            Some(if x < controls + CONTROL_W {
                Hit::Minimize
            } else if x < controls + CONTROL_W * 2 {
                Hit::Maximize
            } else {
                Hit::Close
            })
        } else {
            Some(Hit::Title)
        }
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
    /// Geometry saved by the maximize toggle. `None` means the window is not maximized.
    restore: Option<(i32, i32, u32, u32)>,
}

/// The window manager: the set of open windows, the drag in flight, and the ledger.
#[derive(Debug, Default)]
pub struct WindowManager {
    wins: Vec<Window>,
    /// The window being dragged and the pointer's offset from its top-left at the press.
    drag: Option<(u32, i32, i32, DragKind)>,
    /// Geometry at the start of the current move drag, used as the restore target if that drag
    /// ends in an edge snap. It is separate from `Window::restore` so free-form dragging does
    /// not make a window appear maximized before a snap actually occurs.
    drag_restore: Option<(u32, (i32, i32, u32, u32))>,
    opens: u64,
    closes: u64,
    drags: u64,
    refusals: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragKind {
    Move,
    Resize(ResizeEdge),
}

/// Keyboard-directed desktop snap target. This is deliberately the same four-way geometry
/// family as pointer edge snapping, but does not require a pointer event to be in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Keyboard-directed movement of the focused window. Kept separate from snapping so a held
/// Shift modifier can request a precise nudge without changing the existing four-way snap
/// contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NudgeDirection {
    Left,
    Right,
    Up,
    Down,
}

const KEYBOARD_NUDGE_PX: i32 = 16;
const KEYBOARD_RESIZE_PX: i32 = 16;

/// Direction in which keyboard resizing changes the focused window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizeDirection {
    Left,
    Right,
    Up,
    Down,
}

impl WindowManager {
    pub fn new() -> Self {
        WindowManager {
            wins: Vec::new(),
            drag: None,
            drag_restore: None,
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
            restore: None,
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

    /// Current client geometry for a managed window.
    pub fn size(&self, id: u32) -> Option<(u32, u32)> {
        self.wins
            .iter()
            .find(|w| w.id == id)
            .map(|w| (w.width, w.height))
    }

    /// Open window ids in the manager's own insertion order (not the z-order).
    pub fn ids(&self) -> Vec<u32> {
        self.wins.iter().map(|w| w.id).collect()
    }

    /// The window currently being dragged, if any.
    pub fn dragging(&self) -> Option<u32> {
        self.drag.map(|(id, _, _, _)| id)
    }

    /// Whether a managed window currently occupies the full scanout.
    pub fn is_maximized(&self, id: u32) -> Option<bool> {
        self.wins
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.restore.is_some())
    }

    /// Whether a managed window is currently presented. A minimized window keeps its token,
    /// surface, queue and geometry alive; only presentation and pointer eligibility change.
    pub fn is_minimized(&self, comp: &Compositor, id: u32) -> Option<bool> {
        self.wins.iter().find(|w| w.id == id)?;
        comp.is_visible(id).map(|visible| !visible)
    }

    /// Minimize or restore a managed window without destroying its lifecycle state.
    pub fn toggle_minimize(
        &mut self,
        comp: &mut Compositor,
        session: u64,
        id: u32,
    ) -> Result<bool, WmFault> {
        let w = self
            .wins
            .iter()
            .find(|w| w.id == id)
            .copied()
            .ok_or_else(|| {
                self.refusals += 1;
                WmFault::UnknownWindow(id)
            })?;
        let was_minimized = comp
            .is_visible(id)
            .ok_or(WmFault::UnknownWindow(id))
            .map(|v| !v)?;
        comp.set_visible(id, w.token, was_minimized)?;
        if was_minimized {
            comp.raise(id, w.token)?;
            comp.set_focus(session, id)?;
            Ok(false)
        } else {
            if comp.focus() == Some(id) {
                let _ = comp.clear_focus(session);
                self.focus_topmost_visible(comp, session)?;
            }
            Ok(true)
        }
    }

    fn focus_topmost_visible(&self, comp: &mut Compositor, session: u64) -> Result<(), WmFault> {
        for i in (0..comp.placed_len()).rev() {
            let Some((id, _, _)) = comp.placed_at(i) else {
                continue;
            };
            if self.wins.iter().any(|w| w.id == id) && comp.is_visible(id) == Some(true) {
                comp.set_focus(session, id)?;
                return Ok(());
            }
        }
        Ok(())
    }

    /// Toggle a window between its saved geometry and the full scanout. The manager owns the
    /// geometry transition and token, so callers cannot maximize a window they do not own.
    /// Existing pixels survive the resize through the compositor's resize contract.
    pub fn toggle_maximize(
        &mut self,
        comp: &mut Compositor,
        session: u64,
        id: u32,
    ) -> Result<bool, WmFault> {
        let pos = self.wins.iter().position(|w| w.id == id).ok_or_else(|| {
            self.refusals += 1;
            WmFault::UnknownWindow(id)
        })?;
        let w = self.wins[pos];
        let (sw, sh) = comp.scanout_size();

        if let Some((x, y, width, height)) = w.restore {
            comp.resize_surface(id, w.token, width, height)?;
            comp.move_surface(id, w.token, x, y)?;
            let entry = &mut self.wins[pos];
            entry.width = width;
            entry.height = height;
            entry.restore = None;
            comp.raise(id, w.token)?;
            comp.set_focus(session, id)?;
            Ok(false)
        } else {
            if sw == 0 || sh == 0 {
                self.refusals += 1;
                return Err(WmFault::Compositor(CompFault::BadGeometry(id)));
            }
            let (x, y) = comp
                .placement(id)
                .ok_or(WmFault::Compositor(CompFault::UnknownSurface(id)))?;
            let restore = (x, y, w.width, w.height);
            // The desktop's scanout is bounded by the compositor's surface-pixel ceiling.
            // Let the compositor make the authoritative geometry decision.
            comp.resize_surface(id, w.token, sw, sh)?;
            comp.move_surface(id, w.token, 0, 0)?;
            let entry = &mut self.wins[pos];
            entry.restore = Some(restore);
            entry.width = sw;
            entry.height = sh;
            comp.raise(id, w.token)?;
            comp.set_focus(session, id)?;
            Ok(true)
        }
    }

    /// Tile every visible managed window into equal-width columns across the scanout. Hidden
    /// windows keep their presentation and geometry. Tiling is a manager-owned layout operation:
    /// owner tokens, input queues, focus authority and z-order remain unchanged.
    pub fn tile_visible(&mut self, comp: &mut Compositor, session: u64) -> Result<usize, WmFault> {
        let (sw, sh) = comp.scanout_size();
        if sw < CLOSE_W * 2 || sh < TITLE_H + RESIZE_W * 2 {
            self.refusals += 1;
            return Err(WmFault::Compositor(CompFault::BadGeometry(0)));
        }
        let mut visible = 0usize;
        for w in &self.wins {
            if comp.is_visible(w.id) == Some(true) {
                visible += 1;
            }
        }
        if visible == 0 {
            let _ = comp.clear_focus(session);
            return Ok(0);
        }
        let column_w = sw / visible as u32;
        if column_w < CLOSE_W * 2 {
            self.refusals += 1;
            return Err(WmFault::Compositor(CompFault::BadGeometry(0)));
        }

        let mut column = 0usize;
        for i in 0..self.wins.len() {
            let w = self.wins[i];
            if comp.is_visible(w.id) != Some(true) {
                continue;
            }
            let x = (column as u32 * column_w) as i32;
            let width = if column + 1 == visible {
                sw.saturating_sub(column_w.saturating_mul((visible - 1) as u32))
            } else {
                column_w
            };
            comp.resize_surface(w.id, w.token, width, sh)?;
            comp.move_surface(w.id, w.token, x, 0)?;
            let entry = &mut self.wins[i];
            entry.width = width;
            entry.height = sh;
            entry.restore = None;
            column += 1;
        }
        if let Some(id) = comp.focus() {
            if self
                .wins
                .iter()
                .any(|w| w.id == id && comp.is_visible(id) == Some(true))
            {
                comp.set_focus(session, id)?;
            }
        }
        Ok(visible)
    }

    /// Cascade every visible managed window while preserving each window's size. Windows are
    /// offset diagonally by a bounded amount and clamped so their top-left remains on-screen.
    /// Hidden windows retain their placement; changing a visible window's geometry clears its
    /// maximize restore state because the cascade becomes the current layout.
    pub fn cascade_visible(
        &mut self,
        comp: &mut Compositor,
        session: u64,
    ) -> Result<usize, WmFault> {
        let (sw, sh) = comp.scanout_size();
        if sw == 0 || sh == 0 {
            self.refusals += 1;
            return Err(WmFault::Compositor(CompFault::BadGeometry(0)));
        }

        let mut visible = 0usize;
        for w in &self.wins {
            if comp.is_visible(w.id) == Some(true) {
                visible += 1;
            }
        }
        if visible == 0 {
            let _ = comp.clear_focus(session);
            return Ok(0);
        }

        let mut slot = 0u32;
        for i in 0..self.wins.len() {
            let w = self.wins[i];
            if comp.is_visible(w.id) != Some(true) {
                continue;
            }
            let offset = slot.saturating_mul(CASCADE_OFFSET_PX);
            let max_x = sw.saturating_sub(w.width) as i32;
            let max_y = sh.saturating_sub(w.height) as i32;
            let x = offset.min(max_x.max(0) as u32) as i32;
            let y = offset.min(max_y.max(0) as u32) as i32;
            comp.move_surface(w.id, w.token, x, y)?;
            let entry = &mut self.wins[i];
            entry.restore = None;
            slot += 1;
        }
        if let Some(id) = comp.focus() {
            if self
                .wins
                .iter()
                .any(|w| w.id == id && comp.is_visible(id) == Some(true))
            {
                comp.set_focus(session, id)?;
            }
        }
        Ok(visible)
    }

    /// Snap the focused managed window to a scanout half without requiring a pointer drag.
    /// Existing maximize/snap restore geometry is preserved, so repeated directional moves can
    /// move between halves and the existing maximize toggle can still return to the user's
    /// pre-snap geometry. The operation is allocation-free and uses the manager-held token.
    pub fn snap_focused(
        &mut self,
        comp: &mut Compositor,
        session: u64,
        direction: SnapDirection,
    ) -> Result<Option<u32>, WmFault> {
        let Some(id) = comp.focus() else {
            return Ok(None);
        };
        let pos = self.wins.iter().position(|w| w.id == id).ok_or_else(|| {
            self.refusals += 1;
            WmFault::UnknownWindow(id)
        })?;
        if comp.is_visible(id) != Some(true) {
            return Ok(None);
        }

        let (sw, sh) = comp.scanout_size();
        let half_w = sw / 2;
        let half_h = sh / 2;
        if sw < CLOSE_W * 2 || sh < TITLE_H + RESIZE_W * 2 || half_w == 0 || half_h == 0 {
            self.refusals += 1;
            return Err(WmFault::Compositor(CompFault::BadGeometry(id)));
        }

        let (nx, ny, nw, nh) = match direction {
            SnapDirection::Left => (0, 0, half_w, sh),
            SnapDirection::Right => ((sw - half_w) as i32, 0, half_w, sh),
            SnapDirection::Up => (0, 0, sw, half_h),
            SnapDirection::Down => (0, (sh - half_h) as i32, sw, half_h),
        };
        let w = self.wins[pos];
        if w.restore.is_none() {
            let (x, y) = comp
                .placement(id)
                .ok_or(WmFault::Compositor(CompFault::UnknownSurface(id)))?;
            self.wins[pos].restore = Some((x, y, w.width, w.height));
        }
        comp.resize_surface(id, w.token, nw, nh)?;
        comp.move_surface(id, w.token, nx, ny)?;
        let entry = &mut self.wins[pos];
        entry.width = nw;
        entry.height = nh;
        comp.raise(id, w.token)?;
        comp.set_focus(session, id)?;
        Ok(Some(id))
    }

    /// Move the focused visible managed window by one fixed keyboard step. Maximized/snapped
    /// windows are deliberately not moved: their restore geometry is user state, and silently
    /// discarding it would make a keyboard nudge destructive. The operation stays allocation-free
    /// and uses the manager-held owner token.
    pub fn nudge_focused(
        &mut self,
        comp: &mut Compositor,
        direction: NudgeDirection,
    ) -> Result<Option<u32>, WmFault> {
        let Some(id) = comp.focus() else {
            return Ok(None);
        };
        let Some(w) = self.wins.iter().find(|w| w.id == id).copied() else {
            self.refusals += 1;
            return Err(WmFault::UnknownWindow(id));
        };
        if comp.is_visible(id) != Some(true) || w.restore.is_some() {
            return Ok(None);
        }
        let Some((x, y)) = comp.placement(id) else {
            self.refusals += 1;
            return Err(WmFault::Compositor(CompFault::UnknownSurface(id)));
        };
        let (sw, sh) = comp.scanout_size();
        let max_x = sw.saturating_sub(w.width) as i32;
        let max_y = sh.saturating_sub(w.height) as i32;
        let (dx, dy) = match direction {
            NudgeDirection::Left => (-KEYBOARD_NUDGE_PX, 0),
            NudgeDirection::Right => (KEYBOARD_NUDGE_PX, 0),
            NudgeDirection::Up => (0, -KEYBOARD_NUDGE_PX),
            NudgeDirection::Down => (0, KEYBOARD_NUDGE_PX),
        };
        let nx = (x + dx).clamp(0, max_x);
        let ny = (y + dy).clamp(0, max_y);
        if (nx, ny) == (x, y) {
            return Ok(Some(id));
        }
        comp.move_surface(id, w.token, nx, ny)?;
        Ok(Some(id))
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
        // Front to back over the compositor's own placement table, WITHOUT allocating: a pointer
        // event must not cost a `Vec` on a heap that never frees (ADR-086).
        for i in (0..comp.placed_len()).rev() {
            let Some((id, px, py)) = comp.placed_at(i) else {
                continue;
            };
            let Some(w) = self.wins.iter().find(|w| w.id == id) else {
                continue; // a surface this manager does not own (the wallpaper, a suite's)
            };
            if comp.is_visible(id) != Some(true) {
                continue;
            }
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
            Some(Hit::Minimize) => match self.toggle_minimize(comp, session, id) {
                Ok(_) => Press::Minimized(id),
                Err(_) => Press::Empty,
            },
            Some(Hit::Maximize) => match self.toggle_maximize(comp, session, id) {
                Ok(_) => Press::Maximized(id),
                Err(_) => Press::Empty,
            },
            Some(Hit::Close) => match self.close(comp, session, id) {
                Ok(()) => Press::Closed(id),
                Err(_) => Press::Empty,
            },
            Some(Hit::Title) => {
                let _ = comp.raise(id, w.token);
                let _ = comp.set_focus(session, id);
                self.drag = Some((id, lx, ly, DragKind::Move));
                self.drag_restore = comp.placement(id).and_then(|(x, y)| {
                    self.size(id)
                        .map(|(width, height)| (id, (x, y, width, height)))
                });
                Press::Dragging(id)
            }
            Some(Hit::Resize(edge)) => {
                let _ = comp.raise(id, w.token);
                let _ = comp.set_focus(session, id);
                self.drag = Some((id, lx, ly, DragKind::Resize(edge)));
                self.drag_restore = None;
                Press::Resizing(id)
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
        let (id, ox, oy, kind) = self.drag?;
        let token = self.token(id)?;
        match kind {
            DragKind::Move => match comp.move_surface(id, token, x as i32 - ox, y as i32 - oy) {
                Ok(()) => Some(id),
                Err(_) => None,
            },
            DragKind::Resize(edge) => {
                let (px, py) = comp.placement(id)?;
                let (old_w, old_h) = self.size(id)?;
                let right = px.saturating_add(old_w as i32).saturating_sub(1);
                let bottom = py.saturating_add(old_h as i32).saturating_sub(1);
                let min_w = (RESIZE_W * 2) as i32;
                let min_h = (TITLE_H + RESIZE_W * 2) as i32;
                let mut nx = px;
                let mut ny = py;
                let mut width = old_w as i32;
                let mut height = old_h as i32;
                match edge {
                    ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft => {
                        nx = (x as i32).min(right - min_w + 1);
                        width = right - nx + 1;
                    }
                    ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight => {
                        width = (x as i32 - px + 1).max(min_w);
                    }
                    _ => {}
                }
                match edge {
                    ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight => {
                        ny = (y as i32).min(bottom - min_h + 1);
                        height = bottom - ny + 1;
                    }
                    ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight => {
                        height = (y as i32 - py + 1).max(min_h);
                    }
                    _ => {}
                }
                if nx < 0 || ny < 0 {
                    return None;
                }
                let (sw, sh) = comp.scanout_size();
                if nx as u32 + width as u32 > sw || ny as u32 + height as u32 > sh {
                    return None;
                }
                let width = width as u32;
                let height = height as u32;
                match comp.resize_surface(id, token, width, height) {
                    Ok(()) => {
                        if let Some(w) = self.wins.iter_mut().find(|w| w.id == id) {
                            w.width = width;
                            w.height = height;
                            w.restore = None;
                        }
                        if (nx, ny) != (px, py) {
                            if comp.move_surface(id, token, nx, ny).is_err() {
                                return None;
                            }
                        }
                        Some(id)
                    }
                    Err(_) => None,
                }
            }
        }
    }

    /// Pointer RELEASE: ends any drag in flight and counts it. Returns the window released.
    pub fn release(&mut self) -> Option<u32> {
        let (id, _, _, _) = self.drag.take()?;
        self.drag_restore = None;
        self.drags += 1;
        Some(id)
    }

    /// Finish a pointer drag and apply the desktop's edge-snap policy. A move ending in a
    /// corner claims that quarter of the scanout; otherwise the top edge maximizes the window,
    /// while the left and right edges claim the corresponding half and the bottom edge claims
    /// the lower half. Resize drags are never snapped.
    pub fn release_at(
        &mut self,
        comp: &mut Compositor,
        session: u64,
        x: u32,
        y: u32,
    ) -> Option<u32> {
        let (id, _, _, kind) = self.drag?;
        if kind == DragKind::Move {
            let _ = self.snap_edge(comp, session, id, x, y);
        }
        self.release()
    }

    fn snap_edge(
        &mut self,
        comp: &mut Compositor,
        session: u64,
        id: u32,
        x: u32,
        y: u32,
    ) -> Result<bool, WmFault> {
        let pos = self.wins.iter().position(|w| w.id == id).ok_or_else(|| {
            self.refusals += 1;
            WmFault::UnknownWindow(id)
        })?;
        let w = self.wins[pos];
        let (sw, sh) = comp.scanout_size();
        if sw < CLOSE_W * 2 || sh < TITLE_H + RESIZE_W * 2 {
            return Ok(false);
        }

        let half_w = sw / 2;
        let half_h = sh / 2;
        let snap = if x <= SNAP_EDGE_PX && y <= SNAP_EDGE_PX {
            Some((0i32, 0i32, half_w, half_h))
        } else if x.saturating_add(SNAP_EDGE_PX) >= sw && y <= SNAP_EDGE_PX {
            Some(((sw - half_w) as i32, 0i32, half_w, half_h))
        } else if x <= SNAP_EDGE_PX && y.saturating_add(SNAP_EDGE_PX) >= sh {
            Some((0i32, (sh - half_h) as i32, half_w, half_h))
        } else if x.saturating_add(SNAP_EDGE_PX) >= sw && y.saturating_add(SNAP_EDGE_PX) >= sh {
            Some(((sw - half_w) as i32, (sh - half_h) as i32, half_w, half_h))
        } else if y <= SNAP_EDGE_PX {
            Some((0i32, 0i32, sw, sh))
        } else if x <= SNAP_EDGE_PX {
            Some((0i32, 0i32, half_w, sh))
        } else if x.saturating_add(SNAP_EDGE_PX) >= sw {
            Some(((sw - half_w) as i32, 0i32, half_w, sh))
        } else if y.saturating_add(SNAP_EDGE_PX) >= sh {
            Some((0i32, (sh - half_h) as i32, sw, half_h))
        } else {
            None
        };
        let Some((nx, ny, nw, nh)) = snap else {
            return Ok(false);
        };

        if w.restore.is_none() {
            let restore = self
                .drag_restore
                .filter(|(drag_id, _)| *drag_id == id)
                .map(|(_, geometry)| geometry)
                .or_else(|| {
                    comp.placement(id)
                        .map(|(ox, oy)| (ox, oy, w.width, w.height))
                })
                .ok_or(WmFault::Compositor(CompFault::UnknownSurface(id)))?;
            self.wins[pos].restore = Some(restore);
        }
        comp.resize_surface(id, w.token, nw, nh)?;
        comp.move_surface(id, w.token, nx, ny)?;
        let entry = &mut self.wins[pos];
        entry.width = nw;
        entry.height = nh;
        comp.raise(id, w.token)?;
        comp.set_focus(session, id)?;
        Ok(true)
    }

    /// Close a window: detach it (surface, queue and token die together), then give focus to
    /// the next topmost window still open, or clear it when none is left.
    /// Resize the focused visible window by one fixed keyboard step. Maximized/snapped and
    /// hidden windows are refused so their saved restore geometry remains untouched.
    pub fn resize_focused(
        &mut self,
        comp: &mut Compositor,
        direction: ResizeDirection,
    ) -> Result<Option<u32>, WmFault> {
        let Some(id) = comp.focus() else {
            self.refusals += 1;
            return Ok(None);
        };
        let Some(pos) = self.wins.iter().position(|w| w.id == id) else {
            self.refusals += 1;
            return Ok(None);
        };
        let w = &self.wins[pos];
        if comp.is_visible(id) != Some(true) || w.restore.is_some() {
            self.refusals += 1;
            return Ok(None);
        }
        let (x, y) = match comp.placement(id) {
            Some(p) => p,
            None => {
                self.refusals += 1;
                return Ok(None);
            }
        };
        let (sw, sh) = comp.scanout_size();
        let (mut width, mut height) = (w.width as i32, w.height as i32);
        match direction {
            ResizeDirection::Left => width -= KEYBOARD_RESIZE_PX,
            ResizeDirection::Right => width += KEYBOARD_RESIZE_PX,
            ResizeDirection::Up => height -= KEYBOARD_RESIZE_PX,
            ResizeDirection::Down => height += KEYBOARD_RESIZE_PX,
        }
        if width <= 0
            || height <= 0
            || x < 0
            || y < 0
            || x as u32 + width as u32 > sw
            || y as u32 + height as u32 > sh
        {
            self.refusals += 1;
            return Ok(None);
        }
        comp.resize_surface(id, w.token, width as u32, height as u32)?;
        self.wins[pos].width = width as u32;
        self.wins[pos].height = height as u32;
        Ok(Some(id))
    }

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
        if self.drag.map(|(d, _, _, _)| d) == Some(id) {
            self.drag = None; // a window closed mid-drag drags nothing
            self.drag_restore = None;
        }
        self.closes += 1;
        // Focus falls to the topmost survivor this manager owns; nothing left means NOTHING
        // focused, so the next keystroke is refused `NoFocus` instead of routed to a corpse.
        let next = (0..comp.placed_len())
            .rev()
            .filter_map(|i| comp.placed_at(i).map(|(sid, _, _)| sid))
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

    /// Cycle keyboard focus through this manager's windows without allocating. The cycle follows
    /// the compositor's current z-order, so the next target is the next visible window rather
    /// than an insertion-order surprise. Non-window surfaces (the desktop panel, for example)
    /// are skipped. `forward=false` walks the same order backwards. The selected window is raised
    /// before focus changes, keeping keyboard focus and visual stacking aligned.
    pub fn cycle_focus(
        &mut self,
        comp: &mut Compositor,
        session: u64,
        forward: bool,
    ) -> Result<Option<u32>, WmFault> {
        let len = comp.placed_len();
        if len == 0 || self.wins.is_empty() {
            return Ok(None);
        }

        let current = comp.focus().and_then(|focused| {
            (0..len).find(|&i| comp.placed_at(i).map(|(id, _, _)| id) == Some(focused))
        });

        let first = current.unwrap_or_else(|| if forward { len - 1 } else { 0 });
        for step in 1..=len {
            let i = if forward {
                (first + step) % len
            } else {
                (first + len - (step % len)) % len
            };
            let Some((id, _, _)) = comp.placed_at(i) else {
                continue;
            };
            if comp.is_visible(id) != Some(true) {
                continue;
            }
            let Some(w) = self.wins.iter().find(|w| w.id == id).copied() else {
                continue;
            };
            comp.raise(id, w.token)?;
            comp.set_focus(session, id)?;
            return Ok(Some(id));
        }
        Ok(None)
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

    // 1 — chrome geometry is the painter's: the three title controls occupy the rightmost
    //     CONTROL_W-sized slots, the rest of the band is the drag strip, below it is the client,
    //     and a point outside the window's pixels is no hit at all.
    {
        let (w, h) = (80u32, 60u32);
        check!(
            hit_at(w, h, 0, 0) == Some(Hit::Title)
                && hit_at(w, h, (w - CONTROL_W * 3) as i32, 0) == Some(Hit::Minimize)
                && hit_at(w, h, (w - CONTROL_W * 2) as i32, 0) == Some(Hit::Maximize)
                && hit_at(w, h, (w - CONTROL_W) as i32, 0) == Some(Hit::Close)
                && hit_at(w, h, (w - CONTROL_W * 3 - 1) as i32, (TITLE_H - 1) as i32)
                    == Some(Hit::Title)
                && hit_at(w, h, (w - 1) as i32, (TITLE_H - 1) as i32) == Some(Hit::Close)
                && hit_at(w, h, (w - 1) as i32, (TITLE_H + RESIZE_W + 1) as i32)
                    == Some(Hit::Resize(ResizeEdge::Right))
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
        let p = wm.press(&mut comp, sess, 10, 40); // A only (B starts at x=40,y=20)
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
    // 13 — keyboard nudge is a fixed, allocation-free move of the focused window and never
    // silently converts a snapped/maximized layout into a new restore geometry.
    {
        let (mut comp, mut wm, sess, _a, b) = desk();
        let _ = comp.set_focus(sess, b);
        let before = comp.placement(b);
        let moved = wm.nudge_focused(&mut comp, NudgeDirection::Right);
        let after = comp.placement(b);
        let snapped = wm.snap_focused(&mut comp, sess, SnapDirection::Left);
        let snap_place = comp.placement(b);
        let refused = wm.nudge_focused(&mut comp, NudgeDirection::Right);
        check!(
            moved == Ok(Some(b))
                && before == Some((40, 20))
                && after == Some((56, 20))
                && snapped == Ok(Some(b))
                && snap_place == Some((0, 0))
                && refused == Ok(None)
                && wm.is_maximized(b) == Some(true),
            "wm: keyboard nudge moves by one fixed step and preserves snap restore state"
        );
    }
    Ok(n)
}
