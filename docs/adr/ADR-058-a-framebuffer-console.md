# ADR-058 — The framebuffer console: text an operator could actually see

**Status:** Accepted (2026-08-22)
**Context:** GAPS4 ALET-P2-021 · REQ-GFX-002 · builds on ADR-057 (the virtio-gpu control path)

## Context

The previous wave proved the machine can talk to a display device; nothing it said was worth
displaying. The gap between "the protocol works" and "an OS you can sit in front of" is a renderer, a
console state machine, and a backing store big enough to draw on. One structural fact shapes all of
it: a virtio-gpu backing store is **scatter-gather by design** — `ATTACH_BACKING` lists arbitrary
frames, and this kernel's frame allocator hands out exactly such arbitrary single pages. Pretending
the framebuffer is contiguous would mean inventing a contiguity the substrate neither needs nor gets.

## Decision

**1. The Surface is a page LIST, not a buffer.** `fbcon::Surface` maps pixel (x, y) through byte
offset → page index + intra-page offset. Six hundred kilobytes of console across 150 non-contiguous
frames, attached in ONE command (32 + 150×16 = 2432 bytes of request — the bound that shapes
MAX_BACKING_ENTRIES is our command buffer, not the device, which accepts 16384 entries).

**2. The registry learns handles, and DETACH REVOKES.** Last wave stated honestly that backing pages
stayed DMA-registered for the queue's lifetime. That limitation is now closed at the same choke
point: `register_buffer_h` keeps the `Handle`, the resource table holds one per backing page, and
detach drains them through `revoke_buffer`. After detach returns, the gate no longer vouches for
memory the driver releases — proved live by the region counter returning to exactly ring + two
buffers. Attach failures roll back their own partial registrations: no refused call leaves anything
behind.

**3. A public-domain font, embedded verbatim.** Daniel Hepper's font8x8 (itself aggregated from
public-domain IBM VGA fonts), 128 glyphs covering basic Latin, each 8-byte row LSB-leftmost, drawn
double-strike into 16-pixel lines. Provenance and license are recorded IN the source file — the only
honest annotation for bytes nobody owns. No non-ASCII, no style variants.

**4. Control bytes are fail-closed — the serial console's rule, again.** Printable ASCII blits; LF,
CR and BS have rules; ANY other control byte is refused BY NAME (`UnknownControl`) with the cursor
and every pixel untouched. The same doctrine as INV-CONSOLE-EDIT, one layer up: a byte the console
has no rule for never silently becomes ink.

**5. The renderer is PURE, so it is provable where proofs are cheap.** No device, no locks, no
allocation — pixel-level host tests assert blitting against THE TABLE ITSELF (ink counts computed
from `FONT8X8`, so test and font cannot drift), wrap/scroll/backspace semantics, and every refusal;
the VM suite then proves the rendered frame reaches hardware: create 640x240, attach 150 pages,
render "Aletheia OS", TRANSFER + FLUSH the whole extent, detach-with-revocation, unref — six
invariants, marker `ALL 6 FRAMEBUFFER-CONSOLE INVARIANTS HOLD`, required by all three gates.

## Consequences

* Aletheia draws its own text into its own frames and hands complete frames to the display pipeline
  on all three targets over both buses.
* DMA registrations are now lifecycle-managed everywhere the GPU touches memory.
* **Not claimed:** with `-display none` nobody sees the picture — visibility needs a host display
  backend or real hardware, which is the next honest rung. No cursor-address escape sequences (the
  console draws its own cells), ASCII only, scrolling copies pixels rather than tracking damage, one
  console per machine. The compositor, window/surface model and GPU isolation of ALET-P2-021 remain
  open behind this slice.
