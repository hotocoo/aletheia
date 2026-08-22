# ADR-057 — Graphics: a device that parses us, and pixels that reach the host

**Status:** Accepted (2026-08-22)
**Context:** GAPS4 ALET-P2-021 (first slice) · REQ-GFX-001 · builds on ADR-041 (the reusable virtqueue)
and ADR-036/037 (one driver per device kind over the CPU and bus seams)

## Context

Graphics/compositor was the largest remaining "an OS you can sit in front of" hole: every gate asserted
its markers over a serial line, and the framebuffer half of the experience layer existed nowhere outside
architecture prose. The networking wave (ADR-041) established the shape this slice follows: reuse the ONE
virtio substrate, add the device KIND as data not framework, and prove the path against something that
ANSWERS — because a driver that only pushes bytes outward is indistinguishable from one whose bytes
vanished.

A virtio-gpu control request differs from everything the substrate already served in one structural way:
it is a **two-descriptor chain** — a driver-written request half followed by a DEVICE-WRITABLE response
half. `Virtqueue::add` deliberately supported single descriptors only ("a caller needing a header plus a
payload posts them as one contiguous buffer") — correct for block and net framing, impossible for a
request/response protocol, where the two halves have opposite directions and cannot share a buffer.

## Decision

**1. Chains enter the substrate, not the driver.** `Virtqueue::add_chain` publishes a linked
(request → response) pair: the request descriptor carries `F_NEXT`, the response is `F_WRITE`, and BOTH
halves pass the queue's DMA gate before anything is written, so a refused chain leaves no partial state.
The first live boot found the exact bug this design invites: without `F_NEXT` on the request half, QEMU
read a terminal out-only descriptor and answered by writing **zero** bytes (`response size incorrect 0 vs
408` in its log). A chain API that silently produced a one-descriptor request was worse than none — the
lesson is recorded in the code next to the flag.

**2. Every optional feature is DECLINED at negotiation.** VERSION_1 only; VIRGL, EDID and blob resources
are refused features, not ignored ones — a behavior the driver does not understand is a behavior it has
no proof for.

**3. The resource table exists to refuse BEFORE the device hears anything.** CREATE_2D geometry bounds
(extent ≤ 4096, area ≤ 4 Mi px), rect containment against the LIVE resource extents, scanout-id range,
rid 0 reserved, detach-before-unref order: each is a named refusal counted locally, and the suite PROVES
the silence — `cmds_sent` is compared across whole refusal batteries, so "no device traffic" is measured,
never assumed.

**4. The lifecycle proof is the DEVICE's own error grammar.** GET_DISPLAY_INFO exercises the response
path; but only an ERROR can prove the device PARSES commands rather than echoing them. After DETACH +
UNREF, the SAME flush is sent again via a suite-only probe (documented as such: it deliberately bypasses
the local table, names nothing the driver owns, and still passes the DMA gate) — and the device must
answer ERR_INVALID_RESOURCE_ID both for an id NEVER created and for the id just destroyed. An echo would
have said OK twice.

**5. Geometry is PINNED from measurement, not from spec recall.** The first boot reported scanout 0 as
1280x800 enabled — not the 1024x768 the author expected from memory. The invariant pins what THIS
repository's gates qualify and says so at the pin: a silent change there is a different machine than the
one these gates qualify. The same doctrine as the VirtualBox frame-pool invariant: when firmware facts
differ from expectations, encode the FACTS.

**6. One suite, three targets, two buses.** aarch64 and RISC-V drive virtio-mmio, x86-64 drives
virtio-pci through the existing PCI seam (`find_virtio_gpu_nth`; modern id 0x1050). Thirteen invariants,
marker `ALL 13 VIRTIO-GPU INVARIANTS HOLD`, boot failing `301 + i` — required by all three VM gates with
the device attached. Host tests pin the wire layout field-for-field against the UAPI header (a typo'd
offset is a conversation with nobody), the geometric rules including u32-overflow edges, and the
fail-closed display-info parser (malformed enabled flags poison the WHOLE answer).

## Consequences

* Aletheia talks to a display device end to end: discovery, display info, resource creation, backing
attachment (sixteen DMA-gated pages), scanout binding, transfer, flush — and proven destruction — on
all three targets, exit `301 + i` on failure.
* The substrate now serves chained requests for any future control-plane device.
***Not claimed, and it is a lot:*** no visible picture yet (transferred pixels land on the host surface,
which under `-display none` is nowhere); no framebuffer console text renderer; no cursor queue; no
interrupt-driven completion; backing pages stay DMA-registered for the queue lifetime (bounded by
MAX_REGIONS) — revocation-on-detach is named follow-on work; the compositor, window/surface model and
GPU isolation of ALET-P2-021 remain open. The row stays OPEN with this first slice delivered, exactly
like networking before its stack.
