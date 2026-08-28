# ADR-077: The composition contract — pixels are authority, the scanout is a hard bound

**Status:** Accepted · **Date:** 2026-08-28 · **Advances:** ALET-P2-021 (the compositor rung;
the real-pixel rung stays scoped) · **Builds on:** ADR-003 (capability security), ADR-057
(graphics is a device that parses us), ADR-058 (the framebuffer console), ADR-056 (the idle
machine costs nothing), ADR-064 (the machine measures itself), ADR-076 (the modeled-contract
posture for a new subsystem)

## Context

The PRD's experience layer is composed dynamically around entities and intent — but whatever
renders it must still answer the two questions every privileged surface in this kernel answers:
WHO may put pixels on the scanout, and WHERE those pixels may land. Ambient screen access
would be the same hole ambient device access is. The framebuffer console (ADR-058) proved real
pixels over virtio-gpu; the compositor, the window-surface model and GPU isolation were named
untouched in the gap register (ALET-P2-021). A GUI that promises maximum performance AND
maximum security needs the contract first — the same posture ADR-076 took for power: define
the enforcement semantics once, as a complete software model every proof can run against
today, before wiring a device path.

## Decision

### The contract is defined once, as software, in kernel-core

`kernel-core/src/compositor.rs` defines the full enforcement semantics as `Compositor`:

* **Surfaces answer only to their OWNER token.** A surface is minted with a possession-based
  token (`next_serial ^ secret`); every placement, move, raise, lower, detach and pixel write
  requires THAT token — absent, wrong, foreign and forged tokens are all `NotOwner`, refused
  by name. Fail-closed, like the spine's capabilities and ADR-076's elevation grants.
* **The scanout is a structural bound.** A placement is clipped to the scanout EXACTLY: a
  surface hanging off any edge paints only its intersection, a placement that could never
  show a pixel is refused at attach AND at move (`OffScanout`), and the model's write loops
  only ever visit pixels that exist in BOTH the surface and the scanout — the host proofs run
  against a GUARD-BAND raster where any out-of-bounds put is counted, and the count is
  asserted zero.
* **The painter's order is the z-order, owner-controlled.** List order is z-order; raise and
  lower are owner-only; overlaps flip visibly the same frame.
* **Buffers are SIZE-HONEST.** A packed fill must be exactly ceil(w*h/8) bytes or it is
  refused with NOTHING touched — a short buffer can never overread, a long one can never
  smuggle, and a refused fill never leaves a half-painted surface.
* **Placement changes are VISIBLE the same frame.** Attach/move/detach/raise/lower damage
  the SCREEN regions they vacate and cover; compose clears each damaged region to background
  FIRST (a vacated area must not keep the pixels of whoever left) and then repaints it
  through the z-order — so a moved, raised or detached surface is correct without any client
  redraw, and a damaged bottom surface is repainted THROUGH the z-order rather than over the
  windows above it.
* **Damage is bounded and a quiet frame is FREE.** Damage rects coalesce to whole-surface
  (or whole-scanout) past MAX_DAMAGE_RECTS — summarized, never lost. An unchanged frame
  visits NO region and writes ZERO pixels; the cost of every frame is REPORTED
  (`FrameStats`: writes, and the pixels a damage-naive full-frame repaint would have cost —
  ADR-064's measurement posture applied to the GUI).
* **Bounded and deterministic.** MAX_SURFACES = 16, MAX_SURFACE_PIXELS = 1 MiB-pixel, bounded
  damage ledgers (the boot heap never frees, ADR-063); identical op sequences land
  bit-identical with identical counters.

### Proof posture: host-exhaustive + boot-compact

Host proofs in `kernel-core/tests/compositor.rs` (11 tests): clip exactness swept over all
four edges against the guarded raster with per-pixel oracles, the ownership table over every
mutating op, the buffer-honesty matrix (six wrong sizes refused, then a bit-exact fill),
placement-damage visibility (move erases the vacated area; detach reveals what was
underneath; raise/lower flip the overlap), exact damage accounting (attach = 2 writes/pixel,
quiet = 0, one pixel = its region), geometry/capacity bounds, token non-reuse on re-mint, and
bit-identical determinism with negative placements.

In-kernel: `compositor_suite`, 14 invariants on every boot of all three targets
(`[compositor] ALL 14 COMPOSITION-CONTRACT INVARIANTS HOLD`, boot fails 600+i). Six are pinned
cross-CPU in the conformance contract — because a compositor that let an ungranted caller
draw, or a surface write outside the scanout, would be a different machine whatever its CPU.

### Why modeled first

QEMU's virtio-gpu is a 2D-resource flush device, not a windowing system — it can show a frame
but enforces nothing about WHO composes it or WHERE pixels land, so a "device rung" attempted
today could only prove frames flushed, not that anything was enforced. The model rung proves
the enforcement semantics exhaustively now; composing onto REAL scanout pixels (routing the
sink at the live `fbcon::Surface` backing pages + one virtio-gpu flush per frame) and
device-level GPU isolation between surfaces stay scoped in the gap register, exactly as the
IOMMU contract preceded its silicon (ADR-073/074/075) and the power contract preceded any
frequency control (ADR-076).

## Consequences

* **Named non-claims.** No real-pixel compositor leg yet; no alpha blending (1-bit model, the
  same depth the framebuffer console runs today), no cursors, no input routing, no
  per-surface GPU address-space isolation at the device — all scoped in the register.
* **The damage ledger is the performance story.** The quiet frame costs literally zero
  writes and the counters prove it on every boot — "maximum performance" stated as measured
  numbers rather than adjectives.
* **Marker map changed deliberately** (`compositor=14` on all four gates, ADR-061);
  conformance contract grew six compositor behaviors on all three targets (146 → 152).
