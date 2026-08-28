//! The composition contract: pixels are AUTHORITY, the scanout is a HARD BOUND (ALET-P2-021,
//! ADR-077).
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
