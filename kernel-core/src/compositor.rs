//! The composition contract: pixels are AUTHORITY, the scanout is a HARD BOUND (ALET-P2-021,
//! ADR-077); input is AUTHORITY and the cursor is the COMPOSITOR'S OWN (ALET-P2-021, ADR-079).
//!
//! A GUI that promises both maximum performance and maximum security is, in this kernel's
//! grammar, one contract: WHO may put pixels on the scanout (an authority question — ambient
//! screen access would be the hole every other boundary here refuses to have), and WHERE those
//! pixels may land (a bounds question — a surface must no more write outside the scanout than
//! a DMA window may translate outside its grant). This module defines, once, the contract a
//! compositor must satisfy and a complete SOFTWARE MODEL of it that every proof can run
//! against today — the posture of ADR-071 (IOMMU) and ADR-076 (power).
//!
//! The contract, in one breath: every surface is minted with an unforgeable OWNER token —
//! possession-based like every other authority in this kernel — and every placement, move,
//! raise, lower, detach and pixel write on that surface requires ITS token, refused BY NAME
//! otherwise; a placement is clipped to the scanout EXACTLY (partially-off surfaces render
//! their intersection and nothing more; fully-off placements are refused at attach); the
//! painter's order is the z-order and only the token holder may change it; buffers are
//! SIZE-HONEST — a fill from a short or long buffer is refused with the surface untouched, so
//! no client can overread another's bytes nor smuggle in extra ones; damage is tracked as
//! bounded rects and a frame recomposes ONLY what changed — an unchanged frame blits ZERO
//! pixels (the idle desktop costs nothing, ADR-056's GUI twin) and the cost of every frame is
//! REPORTED, not assumed (ADR-064's posture); every table is capacity-capped (surfaces,
//! damage rects, surface pixels) because the boot heap never frees (ADR-063); and two engines
//! fed the same op sequence land bit-identical.
//!
//! # Proof posture
//!
//! Host-exhaustive in `tests/compositor.rs` (clip exactness under guarded canary rasters on
//! every side, the ownership table, damage accounting, buffer-honesty matrices, z-order and
//! determinism sweeps), plus a compact in-kernel suite so every target proves the core
//! promises at boot. Named non-claim: this is the CONTRACT — composing onto REAL scanout
//! pixels over the virtio-gpu flush path (and GPU isolation between surfaces at the device)
//! stays scoped in the gap register, exactly as the IOMMU contract preceded its silicon.
//!
//! # Input (ADR-079)
//!
//! A keystroke is an authority decision about WHO may read what the user typed, and a
//! pointer is a device nobody else may steer. The contract decomposes into the two
//! questions this module already knows how to answer, plus one new possession: the INPUT
//! PATH (the driver that stands between the user's hardware and the desktop) mints exactly
//! ONE session token per compositor and every event post, focus change and cursor move
//! answers to it — refused BY NAME otherwise, fail-closed like every possession here. At
//! most ONE surface is focused; events route ONLY to the focused surface's BOUNDED queue,
//! and only its OWNER TOKEN may drain it — the input path decides WHERE events go, the
//! owner decides WHO reads them, and neither can act as the other. A surface that stops
//! draining gets a NAMED refusal and a COUNTED drop, never an unbounded queue (the boot
//! heap never frees, ADR-063); detaching the focused surface clears focus and its queue
//! dies with it — events are never resurrected under a re-minted id. The cursor is the
//! compositor's OWN plane: no token names it, no z-order slot holds it, no surface can
//! cover or read it; only the input session may move or hide it, it is clipped EXACTLY to
//! the scanout like every other geometry, it paints ABOVE every surface, its moves are
//! visible the same frame through the same damage machinery, and its cost is REPORTED in
//! `FrameStats::cursor_pixels` (ADR-064). And input is not pixels: a keystroke with no
//! repaint damages NOTHING — a quiet frame stays quiet.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

/// Attached surfaces the model tracks. Bounded for the never-freeing boot heap (ADR-063).
pub const MAX_SURFACES: usize = 16;
/// Damage rects tracked per surface before coalescing to whole-surface damage.
pub const MAX_DAMAGE_RECTS: usize = 32;
/// Largest surface the model accepts, in pixels (a 1024x768 1-bit plane).
pub const MAX_SURFACE_PIXELS: usize = 1 << 20;

/// Why the compositor refused. Every variant names what was involved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompFault {
    /// The surface was never minted (or already detached).
    UnknownSurface(u32),
    /// The surface id is already minted (or already placed).
    AlreadyAttached(u32),
    /// The op requires the surface's OWNER token; this caller offered none that matches.
    /// Fail-closed: absent, wrong and forged tokens are all "not the owner".
    NotOwner { surface: u32 },
    /// The surface does not fit: zero-sized, or more than MAX_SURFACE_PIXELS.
    BadGeometry(u32),
    /// A placement that lies ENTIRELY off the scanout — nothing could ever be shown.
    OffScanout { surface: u32 },
    /// The surface table is full.
    NoSpace,
    /// A pixel write outside the surface's own bounds.
    OutsideSurface { surface: u32, x: u32, y: u32 },
    /// A fill buffer whose length is not exactly ceil(w*h/8) bytes — refused BEFORE any
    /// pixel moves, so a short buffer can never overread and a long one can never smuggle.
    BufferMismatch {
        surface: u32,
        expected_bytes: usize,
        got_bytes: usize,
    },
    /// The input session already exists — a second opener is refused, fail-closed. The
    /// input path is ONE principal; a second token would be a second opinion about where
    /// the user's keystrokes go.
    InputSealed,
    /// An input op was offered no session token, or a wrong or forged one. Absent, wrong
    /// and forged are all "not the input path".
    NotInputSession,
    /// Focus named a minted surface that is not placed on the scanout — nothing focused
    /// can be shown, so nothing focused can receive.
    NotPlaced(u32),
    /// A keystroke arrived and nothing is focused. The event is refused, not queued into
    /// limbo — input that goes nowhere must be NAMED as going nowhere.
    NoFocus,
    /// The focused surface's bounded input queue is full: the event is refused AND COUNTED
    /// as dropped. A surface that stops draining loses input loudly, never silently.
    Backlogged { surface: u32 },
    /// A cursor move whose glyph could never show a pixel — refused, like a fully-off
    /// placement. Partially-off cursor positions are legal and clipped exactly.
    CursorOffScanout { x: u32, y: u32 },
}

/// Input events per surface before drops are counted instead — a window that stops
/// draining must not become an unbounded sink (the boot heap never frees, ADR-063).
pub const MAX_INPUT_EVENTS: usize = 32;
/// The cursor glyph is 8x8.
pub const CURSOR_SIZE: u32 = 8;
/// The cursor glyph, 1-bit rows (bit 7 = leftmost column): a crosshair, transparent where
/// 0. The compositor owns it outright — it is not a surface, has no token, no z-order slot.
const CURSOR_GLYPH: [u8; 8] = [0x10, 0x10, 0x10, 0xFE, 0x10, 0x10, 0x10, 0x00];

/// Wrap one input op: run its body, then COUNT a refusal. The closure borrow ends before
/// the counter moves, so a refusal both changes nothing and is never silent.
macro_rules! count_refusal {
    ($self:expr, $ty:ty, $body:expr) => {{
        let r: Result<$ty, CompFault> = $body;
        if r.is_err() {
            $self.input_refusals += 1;
        }
        r
    }};
}

/// One routed input event. `seq` is a per-compositor monotonic serial — delivery order is
/// observable and replay/reorder is detectable, because an event stream that could lie
/// about order would be a second place authority could be laundered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub seq: u64,
    pub kind: EventKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// A byte of the console's own decoded alphabet (the keymap's output — the same
    /// alphabet the serial editor already rules on, never a raw device byte).
    Key(u8),
    /// Synthesized by the compositor when the surface LOST focus to another. Delivered
    /// into the surface's own queue like any event; a full queue drops it, counted.
    FocusLost,
}

/// One damage rect, in a surface's own coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    fn intersect(a: Rect, b: Rect) -> Option<Rect> {
        let x1 = a.x.max(b.x);
        let y1 = a.y.max(b.y);
        let x2 = a.x.saturating_add(a.w).min(b.x.saturating_add(b.w));
        let y2 = a.y.saturating_add(a.h).min(b.y.saturating_add(b.h));
        if x2 > x1 && y2 > y1 {
            Some(Rect {
                x: x1,
                y: y1,
                w: x2 - x1,
                h: y2 - y1,
            })
        } else {
            None
        }
    }

    fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
}

/// What one compose frame actually did — the measurement that makes "maximum performance"
/// a claim about numbers, not adjectives (ADR-064).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub frames: u64,
    /// Pixels written this frame (0s and 1s alike) after clipping.
    pub pixels_blitted: u64,
    /// Damaged placed pixels a damage-naive compositor would have rewritten that this
    /// frame did not — the damage tracker's measured savings.
    pub pixels_skipped_by_damage: u64,
    /// Cursor-plane ink pixels this frame — the cursor's cost is REPORTED, not assumed
    /// (ADR-064); zero on every frame the cursor did not move or repaint under.
    pub cursor_pixels: u64,
}

/// A client-owned 1-bit surface: its own pixels, its own damage.
pub struct ClientSurface {
    id: u32,
    width: u32,
    height: u32,
    /// Bitpacked rows: bit (y*width + x), word = 64 bits, LSB-first.
    bits: Vec<u64>,
    damage: Vec<Rect>,
    whole_damage: bool,
}

impl ClientSurface {
    /// The exact packed size a fill buffer must have for this surface.
    pub fn packed_bytes(&self) -> usize {
        (self.width as usize * self.height as usize).div_ceil(8)
    }

    /// Record damage. Returns false when the ledger overflowed and COALESCED — the damage
    /// is never lost, only summarized.
    fn mark_damage(&mut self, r: Rect) -> bool {
        if self.whole_damage {
            return true;
        }
        if self.damage.len() >= MAX_DAMAGE_RECTS {
            self.damage.clear();
            self.whole_damage = true;
            return false;
        }
        self.damage.push(r);
        true
    }

    /// Paint one pixel of this surface. Reachable only through the engine, which gates
    /// every call on the owner token.
    pub fn draw_pixel(&mut self, x: u32, y: u32, ink: bool) -> Result<(), CompFault> {
        if x >= self.width || y >= self.height {
            return Err(CompFault::OutsideSurface {
                surface: self.id,
                x,
                y,
            });
        }
        let idx = y as usize * self.width as usize + x as usize;
        let bit = 1u64 << (idx % 64);
        if ink {
            self.bits[idx / 64] |= bit;
        } else {
            self.bits[idx / 64] &= !bit;
        }
        self.mark_damage(Rect { x, y, w: 1, h: 1 });
        Ok(())
    }

    /// Fill a rect of this surface with one color. Clipped to the surface; the damage
    /// recorded is the rect AS ASKED (compose clips again against reality).
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, ink: bool) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                let idx = yy as usize * self.width as usize + xx as usize;
                let bit = 1u64 << (idx % 64);
                if ink {
                    self.bits[idx / 64] |= bit;
                } else {
                    self.bits[idx / 64] &= !bit;
                }
            }
        }
        self.mark_damage(Rect { x, y, w, h });
    }

    /// Fill the whole pixel content from a packed 1-bpp buffer. SIZE-HONEST: the buffer
    /// must be exactly ceil(w*h/8) bytes or the fill is refused with NOTHING touched — a
    /// short buffer must not overread, a long one must not smuggle, and a refused fill
    /// must not leave the surface half-painted.
    pub fn fill_packed(&mut self, buf: &[u8]) -> Result<(), CompFault> {
        let expect = self.packed_bytes();
        if buf.len() != expect {
            return Err(CompFault::BufferMismatch {
                surface: self.id,
                expected_bytes: expect,
                got_bytes: buf.len(),
            });
        }
        let pixels = self.width as usize * self.height as usize;
        for i in 0..pixels {
            let ink = buf[i / 8] & (1 << (i % 8)) != 0;
            if ink {
                self.bits[i / 64] |= 1u64 << (i % 64);
            } else {
                self.bits[i / 64] &= !(1u64 << (i % 64));
            }
        }
        self.whole_damage = true;
        self.damage.clear();
        Ok(())
    }

    pub fn clear(&mut self) {
        self.bits.iter_mut().for_each(|w| *w = 0);
        self.mark_damage(Rect {
            x: 0,
            y: 0,
            w: self.width,
            h: self.height,
        });
    }

    fn pixel(&self, x: u32, y: u32) -> bool {
        let idx = y as usize * self.width as usize + x as usize;
        self.bits[idx / 64] & (1u64 << (idx % 64)) != 0
    }

    /// The surface's damaged regions, coalesced to whole-surface when summarized.
    fn damage_rects(&self) -> Vec<Rect> {
        if self.whole_damage {
            vec![Rect {
                x: 0,
                y: 0,
                w: self.width,
                h: self.height,
            }]
        } else {
            self.damage.clone()
        }
    }

    fn has_damage(&self) -> bool {
        self.whole_damage || !self.damage.is_empty()
    }

    fn clear_damage(&mut self) {
        self.damage.clear();
        self.whole_damage = false;
    }
}

/// One placement: a surface at a scanout position. List order IS the z-order — the last
/// attached (or last raised) paints on top, the painter's way.
#[derive(Clone, Copy, Debug)]
struct Placed {
    surface: u32,
    x: i32,
    y: i32,
}

/// The software compositor: mint surfaces, place them under their owner tokens, compose
/// frames through a caller-supplied pixel sink.
pub struct Compositor {
    secret: u64,
    next_serial: u64,
    scanout: (u32, u32),
    surfaces: Vec<(u32, ClientSurface)>, // id -> surface
    placed: Vec<Placed>,                 // back-to-front
    tokens: Vec<(u32, u64)>,             // surface id -> owner token
    /// SCREEN-space damage from placement changes (attach/move/detach/raise/lower): the
    /// union of vacated and covered regions, which no single surface owns. Bounded and
    /// coalescing like surface damage.
    scanout_damage: Vec<Rect>,
    whole_scanout_damage: bool,
    stats: FrameStats,
    // -- input (ADR-079) ------------------------------------------------------
    /// The ONE input session, minted once, possession-based. `None` = input never opened,
    /// and every input op is refused by name — a desktop that never opened its input path
    /// cannot receive a single keystroke.
    input_session: Option<u64>,
    /// The ONE focused surface id — the only one `post_key` routes to.
    focus: Option<u32>,
    /// Per-minted-surface bounded event queues; they die with the surface.
    queues: Vec<(u32, VecDeque<Event>)>,
    next_event: u64,
    /// Events DROPPED (a full queue behind a surface that stopped draining) and input-op
    /// refusals — both counted, never silent.
    events_dropped: u64,
    input_refusals: u64,
    /// The compositor's own cursor plane: the glyph's top-left on the scanout, `None` =
    /// hidden. No token names it; only the input session moves it.
    cursor: Option<(u32, u32)>,
}

impl Compositor {
    pub fn new(secret: u64, scanout_w: u32, scanout_h: u32) -> Self {
        Compositor {
            secret,
            next_serial: 0x5CAF_0000,
            scanout: (scanout_w, scanout_h),
            surfaces: Vec::new(),
            placed: Vec::new(),
            tokens: Vec::new(),
            scanout_damage: Vec::new(),
            whole_scanout_damage: false,
            stats: FrameStats::default(),
            input_session: None,
            focus: None,
            queues: Vec::new(),
            next_event: 1,
            events_dropped: 0,
            input_refusals: 0,
            cursor: None,
        }
    }

    /// Mint a surface with its OWNER token. The token is possession-based
    /// (`next_serial ^ secret`): holding it is the authority, exactly like the spine's
    /// capabilities and the PM elevation grants.
    pub fn mint_surface(&mut self, id: u32, width: u32, height: u32) -> Result<u64, CompFault> {
        if self.surface(id).is_some() {
            return Err(CompFault::AlreadyAttached(id));
        }
        if width == 0 || height == 0 || width as usize * height as usize > MAX_SURFACE_PIXELS {
            return Err(CompFault::BadGeometry(id));
        }
        if self.surfaces.len() >= MAX_SURFACES {
            return Err(CompFault::NoSpace);
        }
        let pixels = width as usize * height as usize;
        self.surfaces.push((
            id,
            ClientSurface {
                id,
                width,
                height,
                bits: vec![0u64; pixels.div_ceil(64)],
                damage: Vec::new(),
                whole_damage: false,
            },
        ));
        self.next_serial += 1;
        let token = self.next_serial ^ self.secret;
        self.tokens.push((id, token));
        self.queues.push((id, VecDeque::new()));
        Ok(token)
    }

    /// Attach a minted surface at a scanout position. The caller must hold its token, and
    /// the placement must at least INTERSECT the scanout — a placement that could never
    /// show a pixel is refused, not silently accepted. Partial overlap is legal and
    /// clipped at compose time.
    pub fn attach(&mut self, id: u32, token: u64, x: i32, y: i32) -> Result<(), CompFault> {
        self.owner_check(id, token)?;
        if self.placed.iter().any(|p| p.surface == id) {
            return Err(CompFault::AlreadyAttached(id));
        }
        if !self.intersects_scanout(id, x, y) {
            return Err(CompFault::OffScanout { surface: id });
        }
        if self.placed.len() >= MAX_SURFACES {
            return Err(CompFault::NoSpace);
        }
        self.placed.push(Placed { surface: id, x, y });
        self.damage_scanout_at(id, x, y);
        Ok(())
    }

    /// Move a placed surface (owner only). Both the vacated and the covered regions are
    /// damaged — a move must be VISIBLE the same frame, without waiting for a redraw.
    pub fn move_surface(&mut self, id: u32, token: u64, x: i32, y: i32) -> Result<(), CompFault> {
        self.owner_check(id, token)?;
        if !self.intersects_scanout(id, x, y) {
            return Err(CompFault::OffScanout { surface: id });
        }
        let old = self
            .placed
            .iter()
            .find(|p| p.surface == id)
            .copied()
            .ok_or(CompFault::UnknownSurface(id))?;
        self.damage_scanout_at(id, old.x, old.y);
        let p = self
            .placed
            .iter_mut()
            .find(|p| p.surface == id)
            .ok_or(CompFault::UnknownSurface(id))?;
        p.x = x;
        p.y = y;
        self.damage_scanout_at(id, x, y);
        Ok(())
    }

    /// Raise a surface to the top of the z-order (owner only). Its covered area is
    /// damaged: what it now covers AND what uncovering its old z-slot revealed.
    pub fn raise(&mut self, id: u32, token: u64) -> Result<(), CompFault> {
        self.owner_check(id, token)?;
        let pos = self
            .placed
            .iter()
            .position(|p| p.surface == id)
            .ok_or(CompFault::UnknownSurface(id))?;
        let (x, y) = {
            let p = &self.placed[pos];
            (p.x, p.y)
        };
        let p = self.placed.remove(pos);
        self.placed.push(p);
        self.damage_scanout_at(id, x, y);
        Ok(())
    }

    /// Lower a surface to the bottom of the z-order (owner only).
    pub fn lower(&mut self, id: u32, token: u64) -> Result<(), CompFault> {
        self.owner_check(id, token)?;
        let pos = self
            .placed
            .iter()
            .position(|p| p.surface == id)
            .ok_or(CompFault::UnknownSurface(id))?;
        let (x, y) = {
            let p = &self.placed[pos];
            (p.x, p.y)
        };
        let p = self.placed.remove(pos);
        self.placed.insert(0, p);
        self.damage_scanout_at(id, x, y);
        Ok(())
    }

    /// Detach a surface: it leaves the scanout at the next frame (owner only), and the
    /// region it vacated is damaged — what was underneath must reappear. The mint slot
    /// and the token die with it; the id may be minted fresh again.
    pub fn detach(&mut self, id: u32, token: u64) -> Result<(), CompFault> {
        self.owner_check(id, token)?;
        if let Some(p) = self.placed.iter().find(|p| p.surface == id) {
            let (x, y) = (p.x, p.y);
            self.damage_scanout_at(id, x, y);
        }
        self.placed.retain(|p| p.surface != id);
        self.surfaces.retain(|(sid, _)| *sid != id);
        self.tokens.retain(|(sid, _)| *sid != id);
        self.queues.retain(|(sid, _)| *sid != id);
        if self.focus == Some(id) {
            self.focus = None;
        }
        Ok(())
    }

    /// Draw one pixel on a surface (owner only) — the direct pen.
    pub fn draw_pixel(
        &mut self,
        id: u32,
        token: u64,
        x: u32,
        y: u32,
        ink: bool,
    ) -> Result<(), CompFault> {
        self.owner_check(id, token)?;
        self.surface_mut(id)
            .ok_or(CompFault::UnknownSurface(id))?
            .draw_pixel(x, y, ink)
    }

    pub fn fill_rect(
        &mut self,
        id: u32,
        token: u64,
        rect: Rect,
        ink: bool,
    ) -> Result<(), CompFault> {
        self.owner_check(id, token)?;
        self.surface_mut(id)
            .ok_or(CompFault::UnknownSurface(id))?
            .fill_rect(rect.x, rect.y, rect.w, rect.h, ink);
        Ok(())
    }

    pub fn fill_packed(&mut self, id: u32, token: u64, buf: &[u8]) -> Result<(), CompFault> {
        self.owner_check(id, token)?;
        self.surface_mut(id)
            .ok_or(CompFault::UnknownSurface(id))?
            .fill_packed(buf)
    }

    pub fn clear_surface(&mut self, id: u32, token: u64) -> Result<(), CompFault> {
        self.owner_check(id, token)?;
        self.surface_mut(id)
            .ok_or(CompFault::UnknownSurface(id))?
            .clear();
        Ok(())
    }

    // -- input (ADR-079) -------------------------------------------------------

    /// Mint the ONE input session. Possession-based like every authority here
    /// (`next_serial ^ secret`); a second opening is refused `InputSealed` — the input
    /// path is one principal, and a second opinion about where the user's keystrokes go
    /// is exactly the ambient authority this contract refuses to mint.
    pub fn open_input_session(&mut self) -> Result<u64, CompFault> {
        count_refusal!(
            self,
            u64,
            (|| {
                if self.input_session.is_some() {
                    return Err(CompFault::InputSealed);
                }
                self.next_serial += 1;
                let s = self.next_serial ^ self.secret;
                self.input_session = Some(s);
                Ok(s)
            })()
        )
    }

    /// Focus a PLACED surface (input session only). At most one surface is focused;
    /// refocusing queues a synthesized `FocusLost` into the surface that lost it —
    /// delivered through the same bounded queue as any event, dropped AND COUNTED if that
    /// surface stopped draining. Idempotent on the already-focused surface (nothing was
    /// lost, so nothing is queued). Focus is a ROUTING decision: it changes no pixel.
    pub fn set_focus(&mut self, session: u64, id: u32) -> Result<(), CompFault> {
        count_refusal!(
            self,
            (),
            (|| {
                self.session_check(session)?;
                self.surface(id).ok_or(CompFault::UnknownSurface(id))?;
                if !self.placed.iter().any(|p| p.surface == id) {
                    return Err(CompFault::NotPlaced(id));
                }
                if self.focus == Some(id) {
                    return Ok(());
                }
                if let Some(old) = self.focus.take() {
                    self.enqueue(
                        old,
                        Event {
                            seq: self.next_event,
                            kind: EventKind::FocusLost,
                        },
                    );
                    self.next_event += 1;
                }
                self.focus = Some(id);
                Ok(())
            })()
        )
    }

    /// Clear focus (input session only). The surface that had it gets `FocusLost`.
    pub fn clear_focus(&mut self, session: u64) -> Result<(), CompFault> {
        count_refusal!(
            self,
            (),
            (|| {
                self.session_check(session)?;
                if let Some(old) = self.focus.take() {
                    self.enqueue(
                        old,
                        Event {
                            seq: self.next_event,
                            kind: EventKind::FocusLost,
                        },
                    );
                    self.next_event += 1;
                }
                Ok(())
            })()
        )
    }

    /// Focus the topmost placed surface under the screen point `(x, y)` (input session only)
    /// — the routing decision a pointer CLICK carries (ALET-P2-021, ADR-080). The z-order is
    /// scanned front to back and the first placed surface whose VISIBLE area covers the point
    /// wins, with the same FocusLost-through-its-own-queue courtesy `set_focus` gives; a
    /// click on empty space (no placed surface covers the point) CLEARS focus, because
    /// "nowhere" is a place the user pointed at. Routing only: this changes no pixel.
    pub fn focus_at(&mut self, session: u64, x: u32, y: u32) -> Result<Option<u32>, CompFault> {
        count_refusal!(
            self,
            Option<u32>,
            (|| {
                self.session_check(session)?;
                let (sw, sh) = self.scanout;
                let mut target = None;
                for p in self.placed.iter().rev() {
                    let Some(s) = self.surface(p.surface) else {
                        continue;
                    };
                    let vis_w = s.width.saturating_sub(p.x.min(0).unsigned_abs());
                    let vis_h = s.height.saturating_sub(p.y.min(0).unsigned_abs());
                    let px = p.x.max(0) as u32;
                    let py = p.y.max(0) as u32;
                    // The same visible-rect math `blit_region` uses: what is on screen is
                    // what can be clicked, and what is clipped away cannot.
                    if x >= px
                        && y >= py
                        && x < px.saturating_add(vis_w)
                        && y < py.saturating_add(vis_h)
                        && px < sw
                        && py < sh
                    {
                        target = Some(p.surface);
                        break;
                    }
                }
                match target {
                    Some(id) if self.focus == Some(id) => Ok(Some(id)),
                    Some(id) => {
                        if let Some(old) = self.focus.take() {
                            self.enqueue(
                                old,
                                Event {
                                    seq: self.next_event,
                                    kind: EventKind::FocusLost,
                                },
                            );
                            self.next_event += 1;
                        }
                        self.focus = Some(id);
                        Ok(Some(id))
                    }
                    None => {
                        if let Some(old) = self.focus.take() {
                            self.enqueue(
                                old,
                                Event {
                                    seq: self.next_event,
                                    kind: EventKind::FocusLost,
                                },
                            );
                            self.next_event += 1;
                        }
                        Ok(None)
                    }
                }
            })()
        )
    }

    /// Post one decoded keystroke to the FOCUSED surface's bounded queue (input session
    /// only). No focus: refused `NoFocus`, the event exists NOWHERE (asserted by the
    /// suites — nothing is queued into limbo). Full queue: refused `Backlogged` and
    /// COUNTED as a drop. Input is not pixels: this damages nothing.
    pub fn post_key(&mut self, session: u64, byte: u8) -> Result<(), CompFault> {
        count_refusal!(
            self,
            (),
            (|| {
                self.session_check(session)?;
                let Some(id) = self.focus else {
                    return Err(CompFault::NoFocus);
                };
                // Bounded: a surface that stopped draining gets its refusal BY NAME and its
                // drop COUNTED — the event is never queued and never silently swallowed.
                match self.queues.iter_mut().find(|(sid, _)| *sid == id) {
                    Some((_, q)) if q.len() < MAX_INPUT_EVENTS => {
                        q.push_back(Event {
                            seq: self.next_event,
                            kind: EventKind::Key(byte),
                        });
                    }
                    Some(_) => {
                        self.events_dropped += 1;
                        return Err(CompFault::Backlogged { surface: id });
                    }
                    None => return Err(CompFault::UnknownSurface(id)),
                }
                self.next_event += 1;
                Ok(())
            })()
        )
    }

    /// Drain a surface's queue — OWNER TOKEN ONLY. This is the other half of the routing
    /// authority: the input path decides where events GO, the owner decides who READS
    /// them, and a wrong token here is the same refusal a forged draw token is. Returns
    /// the queued events in seq order and empties the queue.
    pub fn drain_input(&mut self, id: u32, token: u64) -> Result<Vec<Event>, CompFault> {
        count_refusal!(
            self,
            Vec<Event>,
            (|| {
                self.owner_check(id, token)?;
                match self.queues.iter_mut().find(|(sid, _)| *sid == id) {
                    Some((_, q)) => Ok(q.drain(..).collect()),
                    None => Ok(Vec::new()),
                }
            })()
        )
    }

    /// Move the compositor's cursor (input session only). A pointer is a device nobody
    /// else may steer: no surface token, however legitimate, moves it. A position whose
    /// glyph could never show a pixel is refused `CursorOffScanout`; a partially-off
    /// position is legal and clipped exactly at compose time, like every other geometry.
    /// The move damages the union of the old and new glyph rects — visible the SAME frame
    /// — and idempotent on the current position (no damage, no wasted frame).
    pub fn move_cursor(&mut self, session: u64, x: u32, y: u32) -> Result<(), CompFault> {
        count_refusal!(
            self,
            (),
            (|| {
                self.session_check(session)?;
                let (sw, sh) = self.scanout;
                if x >= sw || y >= sh {
                    return Err(CompFault::CursorOffScanout { x, y });
                }
                if self.cursor == Some((x, y)) {
                    return Ok(());
                }
                self.damage_cursor();
                self.cursor = Some((x, y));
                self.damage_cursor();
                Ok(())
            })()
        )
    }

    /// Hide the cursor (input session only). The vacated glyph region is damaged: what
    /// was underneath must reappear — the same clear-then-repaint rule a detached surface
    /// obeys, and the reason hide is visible the same frame.
    pub fn hide_cursor(&mut self, session: u64) -> Result<(), CompFault> {
        count_refusal!(
            self,
            (),
            (|| {
                self.session_check(session)?;
                self.damage_cursor();
                self.cursor = None;
                Ok(())
            })()
        )
    }

    /// The focused surface id, if any — observable so the suites can pin it.
    pub fn focus(&self) -> Option<u32> {
        self.focus
    }

    /// The cursor's glyph top-left, if shown.
    pub fn cursor(&self) -> Option<(u32, u32)> {
        self.cursor
    }

    /// (events dropped, input refusals) — the input ledger's two counters.
    pub fn input_counters(&self) -> (u64, u64) {
        (self.events_dropped, self.input_refusals)
    }

    /// How many events sit in a surface's queue right now — observable so the live desktop
    /// can report what the hardware has delivered that the surface has not yet read (ADR-080).
    /// A detached id reports 0: its queue died with it.
    pub fn queued_len(&self, id: u32) -> usize {
        self.queues
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, q)| q.len())
            .unwrap_or(0)
    }

    /// Does anything owe a repaint — a surface's damage, the cursor plane's, or the scanout's?
    /// Read-only and ALLOCATION-FREE (ADR-080): the live desktop's pump asks this every tick so
    /// that a QUIET tick composes nothing at all. `compose_frame` clones the z-order to walk it,
    /// and on a heap that never frees (ADR-063) an allocation per idle tick is a leak by another
    /// name; a changed frame still pays that clone, once, for the change that caused it.
    pub fn has_pending_damage(&self) -> bool {
        self.whole_scanout_damage
            || !self.scanout_damage.is_empty()
            || self.surfaces.iter().any(|(_, s)| s.has_damage())
    }

    fn session_check(&self, session: u64) -> Result<(), CompFault> {
        match self.input_session {
            Some(s) if s == session => Ok(()),
            _ => Err(CompFault::NotInputSession),
        }
    }

    /// Enqueue one event into a surface's bounded queue. Full queue: the event is DROPPED
    /// (never queued, never silently) and the drop counter moves; the queue's existing
    /// contents are untouched — a backlog must not evict events that arrived in order.
    fn enqueue(&mut self, id: u32, ev: Event) {
        match self.queues.iter_mut().find(|(sid, _)| *sid == id) {
            Some((_, q)) if q.len() < MAX_INPUT_EVENTS => q.push_back(ev),
            _ => self.events_dropped += 1,
        }
    }

    /// Damage the screen region the cursor's glyph covers, clipped to the scanout — the
    /// same bounded screen-space ledger every other placement change uses.
    fn damage_cursor(&mut self) {
        let Some((cx, cy)) = self.cursor else {
            return;
        };
        let (sw, sh) = self.scanout;
        let Some(r) = Rect::intersect(
            Rect {
                x: cx,
                y: cy,
                w: CURSOR_SIZE,
                h: CURSOR_SIZE,
            },
            Rect {
                x: 0,
                y: 0,
                w: sw,
                h: sh,
            },
        ) else {
            return;
        };
        if self.whole_scanout_damage {
            return;
        }
        if self.scanout_damage.len() >= MAX_DAMAGE_RECTS {
            self.scanout_damage.clear();
            self.whole_scanout_damage = true;
            return;
        }
        self.scanout_damage.push(r);
    }

    /// The cursor's glyph bit at glyph-local (gx, gy): true where the crosshair has ink.
    fn cursor_ink(gx: u32, gy: u32) -> bool {
        CURSOR_GLYPH[gy as usize] & (0x80u8 >> gx) != 0
    }

    // -- composition ----------------------------------------------------------

    /// Compose one frame into `sink`: every damaged SCREEN region is cleared to the
    /// background and then repainted through the z-order, back to front. The damaged
    /// regions are the union of placement damage (attach/move/detach/raise/lower — the
    /// vacated and covered areas) and each damaged surface's own damage mapped onto the
    /// screen; a surface's damaged pixels are repainted THROUGH the z-order because
    /// painting a damaged bottom surface in isolation would overwrite the windows above
    /// it. A frame in which nothing changed visits NO region and still advances the frame
    /// counter. Every write goes through the sink — the model never touches a buffer it
    /// was not handed, and no pixel outside the scanout is ever put.
    pub fn compose_frame(&mut self, sink: &mut impl Raster) -> FrameStats {
        let (sw, sh) = self.scanout;
        let scanout = Rect {
            x: 0,
            y: 0,
            w: sw,
            h: sh,
        };
        let order: Vec<Placed> = self.placed.clone();
        let mut stats = FrameStats {
            frames: self.stats.frames + 1,
            ..FrameStats::default()
        };

        // Collect the screen regions that owe a repaint, bounded: past MAX_DAMAGE_RECTS
        // the collection COALESCES to the whole scanout — damage is summarized, never
        // lost.
        let mut regions: Vec<Rect> = Vec::new();
        let mut whole = self.whole_scanout_damage;
        if !whole {
            regions.extend(self.scanout_damage.iter().copied());
        }
        if !whole {
            'outer: for p in &order {
                let Some(s) = self.surface(p.surface) else {
                    continue;
                };
                if !s.has_damage() {
                    continue;
                }
                for dmg in s.damage_rects() {
                    let surface_bounds = Rect {
                        x: 0,
                        y: 0,
                        w: s.width,
                        h: s.height,
                    };
                    let Some(dmg) = Rect::intersect(dmg, surface_bounds) else {
                        continue;
                    };
                    regions.push(Rect {
                        x: (p.x + dmg.x as i32).max(0) as u32,
                        y: (p.y + dmg.y as i32).max(0) as u32,
                        w: dmg.w,
                        h: dmg.h,
                    });
                    if regions.len() >= MAX_DAMAGE_RECTS {
                        whole = true;
                        break 'outer;
                    }
                }
            }
        }
        let regions: Vec<Rect> = if whole { vec![scanout] } else { regions };
        let cursor = self.cursor;

        for r in &regions {
            let Some(r) = Rect::intersect(*r, scanout) else {
                continue;
            };
            // Background first: a vacated region must not keep the pixels of whoever left.
            for yy in r.y..r.y + r.h {
                for xx in r.x..r.x + r.w {
                    sink.put(xx, yy, false);
                    stats.pixels_blitted += 1;
                }
            }
            for p in &order {
                self.blit_region(p, r, sink, &mut stats);
            }
            // The cursor is the compositor's own plane: painted LAST, above every surface,
            // clipped exactly like every other geometry, transparent where its glyph is 0,
            // its ink REPORTED in cursor_pixels rather than blended into the count.
            if let Some((cx, cy)) = cursor {
                if let Some(cr) = Rect::intersect(
                    Rect {
                        x: cx,
                        y: cy,
                        w: CURSOR_SIZE,
                        h: CURSOR_SIZE,
                    },
                    r,
                ) {
                    for yy in cr.y..cr.y + cr.h {
                        for xx in cr.x..cr.x + cr.w {
                            if Self::cursor_ink(xx - cx, yy - cy) {
                                sink.put(xx, yy, true);
                                stats.pixels_blitted += 1;
                                stats.cursor_pixels += 1;
                            }
                        }
                    }
                }
            }
        }
        stats.pixels_skipped_by_damage = scanout.area().saturating_sub(stats.pixels_blitted);
        self.scanout_damage.clear();
        self.whole_scanout_damage = false;
        for (_, s) in self.surfaces.iter_mut() {
            s.clear_damage();
        }
        self.stats = stats;
        stats
    }

    /// Blit the intersection of `region` (screen coordinates), one placement and the
    /// scanout into the sink, reading the surface's own pixels. The loops only ever visit
    /// pixels that exist in BOTH the scanout and the surface — the bound is structural.
    fn blit_region(
        &self,
        p: &Placed,
        region: Rect,
        sink: &mut impl Raster,
        stats: &mut FrameStats,
    ) {
        let Some(s) = self.surface(p.surface) else {
            return;
        };
        let (sw, sh) = self.scanout;
        let scanout = Rect {
            x: 0,
            y: 0,
            w: sw,
            h: sh,
        };
        let vis_w = s.width.saturating_sub(p.x.min(0).unsigned_abs());
        let vis_h = s.height.saturating_sub(p.y.min(0).unsigned_abs());
        let place = Rect {
            x: p.x.max(0) as u32,
            y: p.y.max(0) as u32,
            w: vis_w,
            h: vis_h,
        };
        let Some(r) = Rect::intersect(region, place) else {
            return;
        };
        let Some(r) = Rect::intersect(r, scanout) else {
            return;
        };
        for yy in r.y..r.y + r.h {
            for xx in r.x..r.x + r.w {
                let sx = (xx as i32 - p.x) as u32;
                let sy = (yy as i32 - p.y) as u32;
                let ink = s.pixel(sx, sy);
                sink.put(xx, yy, ink);
                stats.pixels_blitted += 1;
            }
        }
    }

    pub fn stats(&self) -> FrameStats {
        self.stats
    }

    /// Where a placed surface's top-left sits on the scanout (ADR-083: the live desktop reports
    /// its window's position through `input`). `None` for minted-but-unplaced or unknown ids.
    pub fn placement(&self, id: u32) -> Option<(i32, i32)> {
        self.placed
            .iter()
            .find(|p| p.surface == id)
            .map(|p| (p.x, p.y))
    }

    pub fn scanout_size(&self) -> (u32, u32) {
        self.scanout
    }

    pub fn surface_count(&self) -> usize {
        self.surfaces.len()
    }

    pub fn placed_count(&self) -> usize {
        self.placed.len()
    }

    /// The z-order as surface ids, back to front — observable so the suite can pin it.
    pub fn z_order(&self) -> Vec<u32> {
        self.placed.iter().map(|p| p.surface).collect()
    }

    /// Live damage-rect ledger depth of a surface (after coalescing: 1).
    pub fn damage_rects_len(&self, id: u32) -> Option<usize> {
        self.surface(id)
            .map(|s| if s.whole_damage { 1 } else { s.damage.len() })
    }

    // -- internals -------------------------------------------------------------

    fn surface(&self, id: u32) -> Option<&ClientSurface> {
        self.surfaces
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, s)| s)
    }

    fn surface_mut(&mut self, id: u32) -> Option<&mut ClientSurface> {
        self.surfaces
            .iter_mut()
            .find(|(sid, _)| *sid == id)
            .map(|(_, s)| s)
    }

    fn intersects_scanout(&self, id: u32, x: i32, y: i32) -> bool {
        let (sw, sh) = self.scanout;
        match self.surface(id) {
            Some(s) => {
                x < sw as i32 && y < sh as i32 && x + s.width as i32 > 0 && y + s.height as i32 > 0
            }
            None => false,
        }
    }

    /// Damage the SCREEN region a placement of `id` at (x, y) covers, clipped to the
    /// scanout. Bounded and coalescing exactly like surface damage.
    fn damage_scanout_at(&mut self, id: u32, x: i32, y: i32) {
        let Some(s) = self.surface(id) else { return };
        let vis_w = s.width.saturating_sub(x.min(0).unsigned_abs());
        let vis_h = s.height.saturating_sub(y.min(0).unsigned_abs());
        let place = Rect {
            x: x.max(0) as u32,
            y: y.max(0) as u32,
            w: vis_w,
            h: vis_h,
        };
        let (sw, sh) = self.scanout;
        let Some(r) = Rect::intersect(
            place,
            Rect {
                x: 0,
                y: 0,
                w: sw,
                h: sh,
            },
        ) else {
            return;
        };
        if self.whole_scanout_damage {
            return;
        }
        if self.scanout_damage.len() >= MAX_DAMAGE_RECTS {
            self.scanout_damage.clear();
            self.whole_scanout_damage = true;
            return;
        }
        self.scanout_damage.push(r);
    }

    fn owner_check(&self, id: u32, token: u64) -> Result<(), CompFault> {
        match self.tokens.iter().find(|(sid, _)| *sid == id) {
            Some((_, t)) if *t == token => Ok(()),
            Some(_) => Err(CompFault::NotOwner { surface: id }),
            None => Err(CompFault::UnknownSurface(id)),
        }
    }
}

/// The only way pixels reach a destination: a sink the CALLER owns. The kernel leg hands a
/// real framebuffer-backed raster; tests hand canary-guarded ones.
pub trait Raster {
    fn put(&mut self, x: u32, y: u32, ink: bool);
}

// ---------------------------------------------------------------------------
// The in-kernel invariant suite. Kept SMALL by design: the boot heap never
// frees (ADR-063); the exhaustive sweeps live in tests/compositor.rs.
// ---------------------------------------------------------------------------
pub fn compositor_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
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

    const S: u32 = 1;
    const T: u32 = 2;

    // A plain shadow raster the suite reads back. It is exactly scanout-sized: any
    // out-of-bounds write from the model would panic the suite, not corrupt a neighbor.
    struct Shadow {
        bits: Vec<bool>,
    }
    impl Shadow {
        fn new(w: u32, h: u32) -> Self {
            Shadow {
                bits: vec![false; w as usize * h as usize],
            }
        }
        fn get(&self, x: u32, y: u32) -> bool {
            self.bits[y as usize * 64 + x as usize]
        }
    }
    impl Raster for Shadow {
        fn put(&mut self, x: u32, y: u32, ink: bool) {
            self.bits[y as usize * 64 + x as usize] = ink;
        }
    }

    // 1 - fail-closed: nothing attached, nothing composed; unknown ids refused by name.
    let mut comp = Compositor::new(0x0CAF_FFEE, 64, 32);
    let mut shadow = Shadow::new(64, 32);
    let st = comp.compose_frame(&mut shadow);
    check!(
        shadow.bits.iter().all(|b| !*b)
            && st.pixels_blitted == 0
            && matches!(comp.attach(9, 0, 0, 0), Err(CompFault::UnknownSurface(9))),
        "compositor: a scanout with no owned surfaces composes nothing and unknown ids are refused by name"
    );

    // 2 - ownership: every mutating op needs the surface's own token; wrong, absent and
    // forged tokens are all "not the owner".
    let tok = comp.mint_surface(S, 16, 16).unwrap();
    let wrong = comp.attach(S, tok ^ 1, 0, 0);
    let right = comp.attach(S, tok, 0, 0);
    check!(
        matches!(wrong, Err(CompFault::NotOwner { surface: S })) && right.is_ok(),
        "compositor: a surface answers only to its own owner token"
    );

    // 3 - the pen writes the surface; the frame shows it at the placement.
    comp.fill_rect(
        S,
        tok,
        Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
        },
        true,
    )
    .unwrap();
    comp.compose_frame(&mut shadow);
    check!(
        shadow.get(0, 0) && shadow.get(7, 7) && !shadow.get(8, 8) && !shadow.get(63, 31),
        "compositor: a painted surface lands on the scanout exactly where it is placed"
    );

    // 4 - clip exactness: a surface pushed past the right edge paints only its
    // intersection; the column before it stays clean.
    comp.move_surface(S, tok, 60, 0).unwrap();
    comp.fill_rect(
        S,
        tok,
        Rect {
            x: 0,
            y: 0,
            w: 16,
            h: 16,
        },
        true,
    )
    .unwrap();
    comp.compose_frame(&mut shadow);
    check!(
        (60..64).all(|x| shadow.get(x, 3)) && !shadow.get(59, 3),
        "compositor: a surface crossing the scanout edge is clipped exactly - nothing lands outside"
    );

    // 5 - fully-off placements are refused at attach; they could never show a pixel.
    let far = comp.mint_surface(T, 8, 8).unwrap();
    check!(
        matches!(
            comp.attach(T, far, 200, 200),
            Err(CompFault::OffScanout { surface: T })
        ) && comp.attach(T, far, -4, 8).is_ok(),
        "compositor: a placement that could never show a pixel is refused at attach"
    );

    // 6 - z-order: the later placement wins the overlap; only the owner may change it.
    comp.fill_rect(
        S,
        tok,
        Rect {
            x: 0,
            y: 0,
            w: 16,
            h: 16,
        },
        false,
    )
    .unwrap();
    comp.fill_rect(
        S,
        tok,
        Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 16,
        },
        true,
    )
    .unwrap(); // S is true only in cols 0..4
    comp.fill_rect(
        T,
        far,
        Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
        },
        true,
    )
    .unwrap(); // T is true everywhere
    comp.move_surface(S, tok, 0, 0).unwrap();
    comp.move_surface(T, far, 4, 4).unwrap(); // overlaps S in [4,8)x[4,8)
    comp.compose_frame(&mut shadow);
    let top_wins = shadow.get(6, 6); // T on top: T(2,2) = true
    comp.raise(S, tok).unwrap();
    comp.compose_frame(&mut shadow);
    let after_raise = !shadow.get(6, 6); // S on top: S(6,6) = false
    check!(
        top_wins
            && after_raise
            && comp.z_order() == vec![T, S]
            && matches!(
                comp.raise(T, tok ^ 7),
                Err(CompFault::NotOwner { surface: T })
            ),
        "compositor: the painter's order is the z-order and only the owner may change it"
    );

    // 7 - damage: an unchanged frame writes NOTHING (the whole frame is measured as
    // skipped); a one-pixel change writes exactly its region - background plus repaint.
    // The idle desktop costs nothing, and the cost is COUNTED (ADR-056/064).
    comp.compose_frame(&mut shadow);
    let quiet = comp.compose_frame(&mut shadow);
    comp.draw_pixel(S, tok, 1, 1, false).unwrap(); // S(1,1) is true -> a real change
    let one_pixel = comp.compose_frame(&mut shadow);
    check!(
        quiet.pixels_blitted == 0
            && quiet.pixels_skipped_by_damage == 64 * 32
            && one_pixel.pixels_blitted == 2
            && one_pixel.frames == quiet.frames + 1,
        "compositor: an unchanged frame writes zero pixels and a one-pixel change writes exactly its region"
    );

    // 8 - buffer honesty: a short or long fill is refused BEFORE any pixel moves.
    let expect = 16usize * 16 / 8;
    let short = vec![0xFFu8; expect - 1];
    let long = vec![0xFFu8; expect + 1];
    let s_short = comp.fill_packed(S, tok, &short);
    let s_long = comp.fill_packed(S, tok, &long);
    check!(
        matches!(
            s_short,
            Err(CompFault::BufferMismatch {
                surface: S,
                expected_bytes: 32,
                got_bytes: 31
            })
        ) && matches!(
            s_long,
            Err(CompFault::BufferMismatch {
                surface: S,
                got_bytes: 33,
                ..
            })
        ),
        "compositor: a fill buffer that is not exactly the packed size is refused by name"
    );

    // 9 - isolation: everything refused above left the OTHER surface standing and the
    // model still composes both.
    comp.compose_frame(&mut shadow);
    check!(
        comp.surface_count() == 2 && comp.placed_count() == 2 && shadow.get(2, 2),
        "compositor: a refused op disturbs neither the other surface nor the composed frame"
    );

    // 10 - detach removes the surface from the next frame and frees the id for re-mint.
    comp.detach(T, far).unwrap();
    comp.compose_frame(&mut shadow);
    let freed = comp.mint_surface(T, 4, 4);
    check!(
        comp.placed_count() == 1 && freed.is_ok(),
        "compositor: a detached surface leaves the frame and its slot is reusable"
    );

    // 11+12 - geometry and capacity are bounded: zero-sized and oversized surfaces refused,
    // and a damage-list overflow COALESCES (the damage is summarized, never lost).
    let mut comp2 = Compositor::new(0x0CAF_FFEE, 64, 32);
    check!(
        matches!(comp2.mint_surface(1, 0, 8), Err(CompFault::BadGeometry(1)))
            && matches!(
                comp2.mint_surface(1, 2048, 2048),
                Err(CompFault::BadGeometry(1))
            ),
        "compositor: zero-sized and oversized surfaces are refused at mint"
    );
    let ok = comp2.mint_surface(1, 64, 32).unwrap();
    for i in 0..300u32 {
        comp2
            .draw_pixel(1, ok, i % 64, (i / 64) % 32, true)
            .unwrap();
    }
    let depth = comp2.damage_rects_len(1).unwrap_or(usize::MAX);
    comp2.compose_frame(&mut shadow);
    check!(
        depth <= MAX_DAMAGE_RECTS && comp2.surface_count() == 1,
        "compositor: the damage ledger coalesces instead of growing without bound"
    );

    // 13 - determinism: two engines fed the same sequence compose bit-identical frames
    // with identical counters.
    let run = || {
        let mut c = Compositor::new(0x1234_5678, 64, 32);
        let mut sh = Shadow::new(64, 32);
        let a = c.mint_surface(1, 16, 16).unwrap();
        let b = c.mint_surface(2, 16, 16).unwrap();
        c.attach(1, a, 0, 0).unwrap();
        c.attach(2, b, 8, 8).unwrap();
        c.fill_rect(
            1,
            a,
            Rect {
                x: 0,
                y: 0,
                w: 12,
                h: 12,
            },
            true,
        )
        .unwrap();
        c.fill_rect(
            2,
            b,
            Rect {
                x: 0,
                y: 0,
                w: 12,
                h: 12,
            },
            true,
        )
        .unwrap();
        c.compose_frame(&mut sh);
        c.raise(1, a).unwrap();
        c.move_surface(2, b, 4, 4).unwrap();
        let st = c.compose_frame(&mut sh);
        (sh.bits.clone(), st, c.z_order())
    };
    let (r1, s1, z1) = run();
    let (r2, s2, z2) = run();
    check!(
        r1 == r2 && s1 == s2 && z1 == z2,
        "compositor: identical op sequences compose bit-identical frames with identical counters"
    );

    // 14 - the bounds question is asked at MOVE too: a move that would take the surface
    // fully off the scanout is refused BY NAME and the surface stays exactly where it was
    // (the refused move is a no-op on the next frame, not a teleport).
    let refused_move = comp.move_surface(S, tok, 200, 200);
    comp.compose_frame(&mut shadow);
    check!(
        matches!(refused_move, Err(CompFault::OffScanout { surface: S })) && shadow.get(2, 2),
        "compositor: a move that could never show a pixel is refused and the surface stays where it was"
    );

    Ok(n)
}

// ---------------------------------------------------------------------------
// The input-routing suite (ALET-P2-021, ADR-079). Same posture as the
// composition suite above: the core promises at boot, the exhaustive sweeps
// on the host in tests/input.rs.
// ---------------------------------------------------------------------------
pub fn input_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
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

    // The same exact-sized shadow the composition suite uses: any out-of-bounds write
    // would panic the suite, not corrupt a neighbor.
    struct Shadow {
        bits: Vec<bool>,
    }
    impl Shadow {
        fn new(w: u32, h: u32) -> Self {
            Shadow {
                bits: vec![false; w as usize * h as usize],
            }
        }
        fn get(&self, x: u32, y: u32) -> bool {
            self.bits[y as usize * 64 + x as usize]
        }
    }
    impl Raster for Shadow {
        fn put(&mut self, x: u32, y: u32, ink: bool) {
            self.bits[y as usize * 64 + x as usize] = ink;
        }
    }

    let mut comp = Compositor::new(0x51CA_1E57, 64, 32);
    let mut shadow = Shadow::new(64, 32);

    // 1 - the input path is ONE principal: the session mints once, possession-based; a
    //     second opening is refused by name and mints nothing.
    let sess = comp.open_input_session().unwrap();
    let again = comp.open_input_session();
    check!(
        matches!(again, Err(CompFault::InputSealed))
            && comp.focus().is_none()
            && comp.cursor().is_none(),
        "input: the input path mints exactly one session and a second opener is refused by name"
    );

    // 2 - focus answers only for a PLACED surface: unminted and minted-but-unplaced are
    //     refused by name, and nothing becomes focused by a refusal.
    let tok = comp.mint_surface(1, 16, 16).unwrap();
    let unplaced = comp.set_focus(sess, 1);
    let unknown = comp.set_focus(sess, 9);
    check!(
        matches!(unplaced, Err(CompFault::NotPlaced(1)))
            && matches!(unknown, Err(CompFault::UnknownSurface(9)))
            && comp.focus().is_none(),
        "input: focus requires a placed surface - unminted and unplaced ids are refused by name"
    );

    // 3 - events route to the focused surface and only its OWNER may read them, in seq
    //     order, exactly once; a wrong owner token is the same refusal a forged draw
    //     token is, and it changes nothing.
    comp.attach(1, tok, 8, 8).unwrap();
    comp.set_focus(sess, 1).unwrap();
    comp.post_key(sess, b'a').unwrap();
    comp.post_key(sess, b'b').unwrap();
    let stranger = comp.drain_input(1, tok ^ 1);
    let events = comp.drain_input(1, tok).unwrap();
    let re_drain = comp.drain_input(1, tok).unwrap();
    check!(
        matches!(stranger, Err(CompFault::NotOwner { surface: 1 }))
            && events.len() == 2
            && events[0].kind == EventKind::Key(b'a')
            && events[1].kind == EventKind::Key(b'b')
            && events[0].seq < events[1].seq
            && re_drain.is_empty(),
        "input: events route to the focused surface and only its owner drains them - in order, exactly once"
    );

    // 4 - at most ONE focus: refocusing moves it, the loser is told through its own queue
    //     (FocusLost, after the events it never read), and refocusing the already-focused
    //     surface is idempotent — nothing lost, nothing queued.
    let tok2 = comp.mint_surface(2, 16, 16).unwrap();
    comp.attach(2, tok2, 32, 8).unwrap();
    comp.set_focus(sess, 2).unwrap();
    comp.post_key(sess, b'x').unwrap();
    comp.set_focus(sess, 1).unwrap();
    let lost = comp.drain_input(2, tok2).unwrap();
    let routed_to_one = comp.drain_input(1, tok).unwrap();
    let idempotent = comp.set_focus(sess, 1).is_ok();
    let nothing_more = comp.drain_input(2, tok2).unwrap().is_empty();
    check!(
        lost.len() == 2
            && lost[0].kind == EventKind::Key(b'x')
            && lost[1].kind == EventKind::FocusLost
            && routed_to_one.len() == 1
            && routed_to_one[0].kind == EventKind::FocusLost
            && idempotent
            && nothing_more,
        "input: exactly one surface is focused - each loser is told through its own queue and an idempotent refocus queues nothing"
    );

    // 5 - a keystroke with nothing focused is refused NoFocus and exists NOWHERE: the
    //     clear told the old focus through its queue, but no queue anywhere holds the
    //     keystroke — input that goes nowhere is NAMED as going nowhere.
    comp.clear_focus(sess).unwrap();
    let no_focus = comp.post_key(sess, b'q');
    let told = comp.drain_input(1, tok).unwrap();
    let q2 = comp.drain_input(2, tok2).unwrap();
    check!(
        matches!(no_focus, Err(CompFault::NoFocus))
            && told.len() == 1
            && told[0].kind == EventKind::FocusLost
            && q2.is_empty()
            && !told.iter().any(|e| e.kind == EventKind::Key(b'q')),
        "input: a keystroke with nothing focused is refused by name and queued nowhere"
    );

    // 6 - a WRONG session token changes nothing on any input op: no focus move, no event,
    //     no cursor change — and the refusals are counted, not silent.
    let (focus0, cursor0) = (comp.focus(), comp.cursor());
    let bad_session = comp.set_focus(sess ^ 0xA5, 2).is_err()
        && comp.post_key(sess ^ 0xA5, b'z').is_err()
        && comp.move_cursor(sess ^ 0xA5, 0, 0).is_err()
        && comp.hide_cursor(sess ^ 0xA5).is_err()
        && comp.clear_focus(sess ^ 0xA5).is_err();
    check!(
        bad_session
            && comp.focus() == focus0
            && comp.cursor() == cursor0
            && comp.drain_input(1, tok).unwrap().is_empty()
            && comp.drain_input(2, tok2).unwrap().is_empty(),
        "input: a wrong session token changes nothing - absent, wrong and forged are all not-the-input-path"
    );

    // 7 - the queue is BOUNDED: at MAX_INPUT_EVENTS the next keystroke is refused
    //     Backlogged AND counted as dropped; draining restores capacity exactly.
    comp.set_focus(sess, 1).unwrap();
    for _ in 0..MAX_INPUT_EVENTS {
        comp.post_key(sess, b'k').unwrap();
    }
    let overflow = comp.post_key(sess, b'k');
    let (dropped, _) = comp.input_counters();
    let drained = comp.drain_input(1, tok).unwrap();
    let after_drain = comp.post_key(sess, b'k').is_ok();
    comp.drain_input(1, tok).unwrap();
    check!(
        matches!(overflow, Err(CompFault::Backlogged { surface: 1 }))
            && dropped == 1
            && drained.len() == MAX_INPUT_EVENTS
            && after_drain,
        "input: a backlogged surface is refused and counted by name - draining restores capacity exactly"
    );

    // 8 - detaching the FOCUSED surface clears focus and the queue dies with it: the
    //     owner token goes dead and no event outlives its surface.
    comp.set_focus(sess, 1).unwrap();
    comp.post_key(sess, b'p').unwrap();
    comp.detach(1, tok).unwrap();
    check!(
        comp.focus().is_none()
            && matches!(comp.drain_input(1, tok), Err(CompFault::UnknownSurface(1))),
        "input: detaching the focused surface clears focus and the queue dies with its surface"
    );

    // 9 - a re-minted surface id starts with an EMPTY queue (events are never resurrected
    //     under a fresh mint) and the OLD token is dead: not the owner of anything.
    let tok1b = comp.mint_surface(1, 16, 16).unwrap();
    comp.attach(1, tok1b, 0, 0).unwrap();
    check!(
        comp.drain_input(1, tok1b).unwrap().is_empty()
            && matches!(
                comp.drain_input(1, tok),
                Err(CompFault::NotOwner { surface: 1 })
            ),
        "input: a re-minted surface id starts empty and its old token is dead - events are never resurrected"
    );

    // 10 - the cursor is the compositor's own plane: only the session moves it, a
    //      position that could never show a pixel is refused by name, and a partially-off
    //      glyph lands EXACTLY its clipped ink — 36 background puts over the one damaged
    //      6x6 region plus the 11 crosshair bits inside it, measured on the frame. (The
    //      settle frame first: the earlier invariants' placement damage must not leak
    //      into this measured one.)
    comp.compose_frame(&mut shadow);
    comp.move_cursor(sess, 58, 26).unwrap();
    let off = comp.move_cursor(sess, 64, 31);
    let st = comp.compose_frame(&mut shadow);
    check!(
        matches!(
            comp.move_cursor(sess ^ 1, 0, 0),
            Err(CompFault::NotInputSession)
        )
            && matches!(off, Err(CompFault::CursorOffScanout { x: 64, y: 31 }))
            && comp.cursor() == Some((58, 26))
            && st.pixels_blitted == 36 + 11
            && st.cursor_pixels == 11,
        "input: the cursor is the compositor's - session-moved, fully-off refused by name, partially-off clipped exactly"
    );

    // 11 - the cursor paints ABOVE every surface (a zero-ink window under it cannot
    //      unpaint it — its ink is the only source), moves are visible the SAME frame at
    //      an exact measured cost, and hiding reveals what was below, the same frame.
    comp.move_surface(2, tok2, 56, 24).unwrap();
    comp.compose_frame(&mut shadow); // settle the surface move; cursor repaints on top
    comp.move_cursor(sess, 60, 28).unwrap();
    let st2 = comp.compose_frame(&mut shadow);
    let above = shadow.get(63, 28);
    comp.hide_cursor(sess).unwrap();
    let st3 = comp.compose_frame(&mut shadow);
    check!(
        above && !shadow.get(63, 28)
            && st2.pixels_blitted == 118
            && st2.cursor_pixels == 14
            && st3.pixels_blitted == 32
            && st3.cursor_pixels == 0,
        "input: the cursor paints above every surface, moves are visible the same frame, and hide reveals what was below"
    );

    // 12 - input is not pixels, and the whole contract is deterministic: a keystroke on a
    //      quiet frame writes nothing, and two engines fed the same input sequence land
    //      bit-identical with identical counters.
    comp.set_focus(sess, 2).unwrap();
    comp.post_key(sess, b'm').unwrap();
    let quiet = comp.compose_frame(&mut shadow);
    let run = || {
        let mut c = Compositor::new(0x51CA_1E57, 64, 32);
        let mut sh = Shadow::new(64, 32);
        let s = c.open_input_session().unwrap();
        let t1 = c.mint_surface(1, 16, 16).unwrap();
        let t2 = c.mint_surface(2, 16, 16).unwrap();
        c.attach(1, t1, 4, 4).unwrap();
        c.attach(2, t2, 30, 10).unwrap();
        c.fill_rect(
            1,
            t1,
            Rect {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
            true,
        )
        .unwrap();
        c.set_focus(s, 1).unwrap();
        c.post_key(s, b'a').unwrap();
        c.post_key(s, b'b').unwrap();
        c.move_cursor(s, 44, 20).unwrap();
        c.compose_frame(&mut sh);
        c.set_focus(s, 2).unwrap();
        c.post_key(s, b'c').unwrap();
        let st = c.compose_frame(&mut sh);
        (
            sh.bits.clone(),
            st,
            c.focus(),
            c.cursor(),
            c.input_counters(),
        )
    };
    let r1 = run();
    let r2 = run();
    check!(
        quiet.pixels_blitted == 0 && quiet.cursor_pixels == 0 && r1 == r2,
        "input: a keystroke is not a pixel - a quiet frame stays quiet, and identical input sequences land bit-identical"
    );

    // 13 - a pointer CLICK is a routing decision (ALET-P2-021, ADR-080): the topmost placed
    //      surface under the point takes focus (the loser told through its own queue), a
    //      click on empty space CLEARS focus, a click on the already-focused surface queues
    //      nothing, and a wrong session token is the same refusal every input op gives.
    //      Scene at this point: 1 at (0,0), 2 at (56,24) focused, z-order [1, 2]. 3 is
    //      attached at (8,16) -> z-order [1, 2, 3], so 3 is the TOP surface. The point
    //      (60,28) is covered by 2 only, so the click on the already-focused surface is
    //      idempotent; (12,20) is covered only by the TOP 3, so the focus is STOLEN there
    //      (and 2 is told through its own queue); (4,4) only by 1; (63,0) by nobody.
    let tok3 = comp.mint_surface(3, 16, 16).unwrap();
    comp.attach(3, tok3, 8, 16).unwrap();
    let already = comp.focus_at(sess, 60, 28);
    let queued_noise = comp.drain_input(2, tok2).unwrap();
    let steal = comp.focus_at(sess, 12, 20);
    let told_two = comp.drain_input(2, tok2).unwrap();
    let underneath = comp.focus_at(sess, 4, 4);
    let told_three = comp.drain_input(3, tok3).unwrap();
    let empty = comp.focus_at(sess, 63, 0);
    let stays_cleared = comp.focus().is_none() && comp.drain_input(3, tok3).unwrap().is_empty();
    let forged = comp.focus_at(sess ^ 0x5A, 0, 0);
    check!(
        matches!(already, Ok(Some(2)))
            && queued_noise.iter().any(|e| e.kind == EventKind::Key(b'm'))
            && matches!(steal, Ok(Some(3)))
            && told_two.iter().any(|e| e.kind == EventKind::FocusLost)
            && matches!(underneath, Ok(Some(1)))
            && told_three.len() == 1
            && told_three[0].kind == EventKind::FocusLost
            && matches!(empty, Ok(None))
            && stays_cleared
            && matches!(forged, Err(CompFault::NotInputSession)),
        "input: a pointer click focuses the topmost surface under the point and a click on empty space clears focus"
    );

    Ok(n)
}
