# Aletheia — Implementation Status

**As of:** 2026-09-03, night (THE DESKTOP UNDER A MERCILESS STORM — `kernel-core/src/wmstorm.rs` is a
boot suite on all three CPUs measured on the platform's own heap: a thousand open/close cycles close
exactly, a window that stops draining backs up to exactly its cap with every loss named and counted,
a drain restores exactly that capacity, four thousand pointer events in the steady state move the
heap watermark by ZERO bytes (printed pass or fail), a settled desktop repaints once and then writes
nothing, and the same storm twice lands bit-identically. The storm FOUND two per-event allocations on
a heap that never frees — the manager's `z_order()` per event and the pump's `drain_input()` per tick
— and both are now allocation-free; ADR-086); before that: (ONE DESKTOP, THREE CPUs — `kernel-core/src/desktop.rs` makes the live
desktop a value generic over the same `VirtioHal`+`Transport` seams the drivers cross, so aarch64,
RISC-V and x86-64 install and pump the SAME compositor, input session, window manager, terminal and
monitor: each kernel keeps only its own static, its own way of shutting the pump out (IF / `DAIF.I` /
`sstatus.SIE`), its own 100 Hz timer (PIT / generic timer PPI / SBI `set_timer`) and its own frame
ledger, handed to `pump(free, total)`; the console's second surface reaches all three; all three
gates assert `[desktop] LIVE ... 2 managed windows`; ADR-085); before that: (WINDOWS ARE A MANAGED SET, NOT ONE PRIVILEGED SURFACE — `kernel-core/src/wm.rs`
gives the desktop a window manager that owns every window's owner token (unknown, duplicate and
over-ceiling opens refused BY NAME and counted), chrome whose geometry is the painter's (the close box
`textgrid` paints and `wm::hit_at` hit-tests from the same constants), a press that is a routing
DECISION reported (`Closed`/`Dragging`/`Focused`/`Empty` over the compositor's own z-order and
visible-rect math — what is clipped away cannot be clicked — touching no pixel), and CLOSE as a
LIFECYCLE: surface, input queue and owner token die together, focus falls to the topmost survivor or is
CLEARED so the next keystroke is refused `NoFocus`, and a re-opened id is a NEW window. ONE authority
decides a click (`vinput::route_pointer_motion` moves the cursor and hands the press back undecided).
The x86-64 live desktop runs TWO managed windows — the console's terminal and a system MONITOR that
repaints when a FACT CHANGES rather than on a timer, so an idle machine still composes nothing — with
12 boot invariants on all three targets (`wm=12`), `textgrid` 6 -> 7, six cross-CPU conformance
behaviours (178 -> 185), 11 host proofs, the live gate 22 -> 30, and the x86-64 heap 8 -> 12 MiB;
ADR-084); before that: (THE TERMINAL WINDOW IS THE CONSOLE'S SECOND SURFACE — the live
desktop's window shows the console and types at it: `kernel-core/src/textgrid.rs` is a pixel-exact
grid of the console's own alphabet (printable bytes in cells, exact scroll, wrap as newline, backspace
erases one, the editor's `ESC [` sequences consumed unpainted, unknown bytes refused and counted)
rendered by a pure function into the 1-bpp buffer `fill_packed` consumes, allocated once; the x86-64
desktop's window is that grid under a title band, every byte the console emits lands in it
(`desktop::term_write`), every keystroke the input session routed to the focused window is drained by
its OWNER into the console's `getc` (`desktop::term_getc`) — one shell, two surfaces — and a LEFT press
on the title band drags the window by the pointer's delta and raises it (`route_pointer_batch`,
`Compositor::placement`); two contexts (IRQ0 pump, main thread under `without_interrupts`) serialized
by the interrupt flag, no lock; `input` prints the window's placement and the terminal's last line —
6 boot invariants on all three targets (`textgrid=6`, fails 700+i, VirtualBox requires it), three
cross-CPU conformance behaviors (175 -> 178), 7 host proofs, the live gate grows 17 -> 22 (a virtio
keystroke ECHOED by the console, `help` typed on the virtio keyboard answered, the window's last line
carries the prompt, a drag by the title band moves the window by exactly the mapped pointer delta), unsafe audit +1,
ADR-083); before that, the same evening: (RECLAIM UNDER PRESSURE — the allocator triggers, the policy chooses,
the forest advises: `kernel-core/src/reclaim.rs` wires the eviction-event forest (REQ-ML-005,
`memrisk`, verified by the SAME loader and contract as the risk forest) into a reclaim round that
only a `MemoryMeter` under the watermark opens (`NotUnderPressure` refused by name otherwise), whose
NEED is the shortfall to twice the watermark, whose candidates are ranked in a TOTAL order — protected
never chosen and counted even when it leaves a named SHORTFALL, then the forest's tier
(eviction-likely first, completion-likely last, abstain/out-of-box/no-model in between so a machine
without the blob ranks bit-identically to one whose forest abstains), then largest footprint, lowest
priority, oldest, id — and whose evictions go through one `ReclaimOps` seam the policy asks once per
chosen task and whose returned frame count it COUNTS rather than trusting the candidate; and on every
target a REAL storm takes frames from the machine's own allocator until the meter is under pressure,
the reclaimer takes them back through the real ownership table, and the free count is restored
EXACTLY — 9 boot invariants on all three targets (`reclaim=9`, boot fails 700+i / 699 on a storm
that did not come back), five cross-CPU conformance behaviors (170 -> 175), 7 host proofs, unsafe
audit UNCHANGED, ADR-082); before that, the same afternoon: (MEMORY IS A BOUNDARY THE ADVISOR CANNOT CROSS — the
allocator decides admissibility, the forest advises order: `kernel-core/src/mlsched.rs` gains
`MemoryMeter` (the frame allocator's own reading, refused `MeterInvalid` by name when it cannot be
true), a pressure ledger (last reading, exact low-water mark, crossings of the 10 % watermark counted
once per entry) and the BOUNDED admission door — `Unmetered` while no allocator has reported (fail
closed), `MemoryExhausted { requested, free }` when a task asks for more than is free, both refused
BEFORE the model is consulted with the scheduler and the feature history untouched, exactly-free
admissible; the resident seam IS that door, every target reads its allocator before each real
admission, commissioning is sized by the machine's own free frames and refused nothing (boot fails
187 otherwise), `mlstat` prints the ledger — 5 more boot invariants on all three targets
(`mlsched=17`), five cross-CPU conformance behaviors (165 -> 170), 6 host proofs, unsafe audit
UNCHANGED, ADR-081; the same day v0.1.0 was RELEASED as the first stable version with a boot-verified
VMware package on its GitHub release, and three CI jobs red since ADR-074 for runner-only reasons —
QEMU 8.2's stage-1-only SMMUv3, a newer clippy's `chunks_exact` lint, GNU grep reading `\r` as `r` —
were fixed, giving the first fully green pipeline on this lineage); before that, the same morning:
(REAL DEVICES THROUGH THE INPUT SESSION — the input HARDWARE rung, and the
desktop goes LIVE: `kernel-core/src/vinput.rs` is one virtio-input driver over both buses (virtio-mmio on
aarch64/RISC-V, virtio-pci on x86-64) with the one config WRITE the transports never needed named as the
`ConfigWrite` seam, devices classified by their OWN declared names (a lone or unclassifiable device refused
by name), a Linux-keycode decoder whose WHOLE output space is the console's alphabet (host-swept over every
keycode in every modifier state, modifier state held separately from the PS/2 decoder's), an absolute-axis
pointer decoder committing on SYN with the device's OWN pinned axis range mapped exactly onto the scanout,
and the click as a ROUTING decision — `Compositor::focus_at` focuses the topmost placed surface under the
point, an empty click clears focus, the loser is told, a forged session is refused — with `route_key` /
`route_pointer` shared between the boot suite and the live pump; `kernel-x86_64/src/desktop.rs` installs
the LIVE desktop before the VT-d gate (one compositor, one session, panel + window, the tablet steering the
cursor) and pumps it from the PIT tick at 100 Hz, bounded, composing only when pixels changed and issuing
exactly one TRANSFER + one FLUSH per changed frame, its facts published as single machine words the new
`input` console command reads LIVE — 10 boot invariants against the REAL devices on all three targets
(`[vinput] ALL 10 INPUT-HARDWARE INVARIANTS HOLD`, init fails 679, suite fails 680+i; `input_suite` grows
12 -> 13 with click-as-routing), 10 + 2 host proofs, seven cross-CPU conformance behaviors (158 -> 165), a
LIVE gate `scripts/vinput-e2e.sh` driving the pump with QMP-injected real device events, unsafe audit
CHANGED (+14/+1/+12/+1) and counted, ADR-080); before that: 2026-08-29 (THE COMPOSED DESKTOP MEETS INPUT — focus is AUTHORITY and the cursor is
the COMPOSITOR'S OWN: `kernel-core/src/compositor.rs` mints exactly ONE possession-based input
session whose every event post, focus change and cursor move answers to it (`InputSealed` on a
second opener, `NotInputSession` on absent/wrong/forged), focuses at most ONE placed surface
and tells the loser `FocusLost` through its own bounded queue, routes keystrokes to the focused
surface only and empties them only through the surface's OWNER token — the input path decides
WHERE events go, the owner decides WHO reads them, and neither can act as the other — with
seq-monotonic exactly-once delivery, `MAX_INPUT_EVENTS`-bounded queues whose overflow is
refused `Backlogged` AND counted, capacity restored exactly on drain, queues and focus dying
with detach and never resurrected under a re-minted id, and the cursor as a compositor-owned
8x8 transparent crosshair that no token names and no surface can cover: session-moved only,
fully-off refused `CursorOffScanout` with the position named, partially-off clipped EXACTLY,
painted above every surface, hide visible the same frame, its cost REPORTED in the new
`FrameStats::cursor_pixels` — and input is not pixels: a keystroke with no repaint writes
nothing — 12 boot invariants on all three targets (`[input] ALL 12 INPUT-ROUTING INVARIANTS
HOLD`, boot fails 660+i), 8 host proofs, ADR-079); before that: 2026-08-28 (THE COMPOSITION
CONTRACT IS ON THE SCANOUT — the model's sink is now
REAL backing pages and the display device carries each composed frame: `kernel-core/src/fbcon.rs`
gains `ComposeSink`, a `Raster` over the framebuffer console's scatter-gather backing frames
whose put/refusal counters make "the model's bound and the real raster's bounds agree" a
MEASURED zero, and `kernel-core/src/virtiogpu.rs` gains `compose_suite`: the composed frame's
virtio-gpu resource is created, its 150 DMA-gated backing pages attached and scanout 0 bound,
the first compose is read back pixel-exact from real memory and handed to the device as one
TRANSFER plus one FLUSH (exactly two commands), a QUIET frame writes zero pixels AND issues
zero device commands (the idle desktop moves nothing — measured on the driver's command
counter), a wrong token changes no real byte and no device traffic, a move is visible the same
frame with no ghost, the z-order flips on real pixels, an overhanging surface lands only its
intersection and never asks the raster for a pixel it does not have, and the teardown revokes
every page's DMA registration with the DEVICE confirming the end — 8 boot invariants on all
three targets (`[compose] ALL 8 REAL-PIXEL COMPOSITION INVARIANTS HOLD`, boot fails 640+i), 5
host proofs, ADR-078); before that: THE COMPOSITION CONTRACT is modeled, not assumed — pixels are AUTHORITY
and the scanout is a HARD BOUND: `kernel-core/src/compositor.rs` mints surfaces with
possession-based owner tokens that gate every op, clips placements exactly to the scanout
(proved against a guard-band raster), keeps the painter's order as the owner-controlled
z-order, refuses size-dishonest buffer fills with nothing touched, makes placement changes
visible the same frame through screen-space damage (clear-then-repaint through the z-order,
so vacated areas lose the pixels of whoever left), coalesces its bounded damage ledgers,
writes ZERO pixels on an unchanged frame with the cost of every frame REPORTED, and lands
bit-identical under identical op sequences — 14 boot invariants on all three targets, six
behaviors pinned cross-CPU, 11 host proofs, ADR-077); before that: THE POWER/PERFORMANCE CONTRACT is modeled, not assumed — frequency is
AUTHORITY and heat is a HARD CEILING: `kernel-core/src/pm.rs` gives every domain an honest
discrete ladder, keeps the governor range free to any caller, gates the overclock band behind
live per-domain elevation grants that attenuate on delegation and clamp the domain back to
nominal the moment their grant dies, makes the thermal envelope absolute BY CONSTRUCTION,
answers a thermal trip with a machine-wide clamp and a tick-exact cooldown that refuses even
valid grants, never lets the governor overclock or park demanded silicon, accounts idle
residency and wake latency exactly, moves device power along legal arcs only, and audits every
act in a bounded monotonic ledger — 14 boot invariants on all three targets, six behaviors
pinned cross-CPU, 19 host proofs, ADR-076); before that: 2026-08-26 (PER-DEVICE DMA WINDOWS are enforced by the real VT-d unit - each driven function
translates ONLY the frames its own driver registry granted; a revoked PAGE is denied by name with measured
reason 6 while sibling windows keep serving - ADR-075); before that: the IOMMU contract crosses the ARM fence — on x86-64 the kernel
discovers the VT-d unit through ACPI DMAR/DRHD, programs an identity domain over owned frames with
the kernel image punched out of it, adopts that domain via SRTP and turns enforcement ON, then
proves live enforcement from the unit's own fault bank: the granted function walks clean, a
revoked function is denied with an ACTIVE record naming its source-id and reason CONTEXT_ENTRY_P,
a restored grant returns to silence, and enforcement stays latched until halt — ADR-073); before that: the custody anchor crosses the platform boundary — the vault root is DELIVERED over the firmware configuration channel on all three targets (QEMU fw_cfg: ioports under q35+OVMF, MMIO on both virt machines), through one door that names every impostor — absent, firmware-absent, wrong-size, foreign-root, rolled-back — with a THIRD rootless boot in each gate proving absence seals the vault while the machine continues, and the combined-transaction question DECIDED: paired commits write the vault generation into the durable entity-store record so even a consistent older VAULT-pair rollback is caught BY NAME (ADR-072); before that: authority custody is a LIFECYCLE, not a caller-supplied key — the persisted registry gains `capvault`: a versioned data-key keystore sealed with in-tree RFC 8439 ChaCha20-Poly1305 under a root-derived subkey the vault alone retains, one-way rotation whose retirement DESTROYS the retired key, constructed prefix||counter nonces reserved before use because the kernel has no boot entropy, a three-commit rekey pivot crash-proved at EVERY recorded device-op position, and 17 custody invariants on every boot of all three targets — the custody half of ALET-P1-034 over authority, ADR-070; before that: encryption at rest is a LIFECYCLE, not a key file — the hosted semantic store gains versioned data keys under a root-derived keystore with rotation/rekey/retirement, constructed prefix||counter nonces whose ledger is the authenticated log itself, position-bound AEAD frames that refuse reordering/deletion/duplication with the position named, plaintext-SHA-256 identity semantics proved in both directions, and transparent wholesale migration of pre-ADR-069 logs detected by trial-authentication — closing the P1-028/029/030 trio over the store, ADR-069; before that: the supply chain is VERIFIED, LIVE, and RECORDED — chain verification crosses the installation boundary: root→signing-key→component provenance is enforced at install against public keys only, admitted entities record their full evidence, the launch gate re-judges that evidence against CURRENT trust so signer revocation goes live at the next launch, all faults are named per link, and the spawn path — found skipping provenance entirely, ALET-P2-050 — now passes the same gate, ADR-067; before that: the component DECLARES what it speaks — the ABI is explicitly versioned: a custom-section declaration enforced at BOTH gates, install refusing undeclared/malformed/foreign-version modules before their bytes are stored and run re-checking on every path, refusals naming both sides of a version disagreement, in... (line truncated to 2000 chars)
**Milestone delivered:** M1 — Hosted System-Core Reference (Rust); **P2 (start)** — WASM capability-secure component runtime; **P4 (start)** — bootable microkernel on THREE CPU targets, VM-tested: aarch64 (bootstrap) + AMD64/x86-64 (first-class) + **RISC-V/RV64GC (first-class)**; **P5 (start)** — real memory management: physical page-frame allocator + MMU virtual memory (identity map + dynamic map/unmap) + **EL0 user-mode with a capability-gated syscall boundary, hardware address-space isolation, per-process address spaces (separate TTBR0), and preemptive multitasking (full trap-frame context switch + round-robin scheduler + GICv2/generic-timer IRQ preemption)**, VM-tested on the aarch64 dev backend
**Maturity:** `docs/MATURITY.md` grades every subsystem Proved / Implemented / Architecture and states
plainly that **nothing here is production-ready** — read it before quoting any claim below.
**Sources of truth:** `docs/Aletheia_Product_Requirements_Document.md` (PRD-003),
`docs/Aletheia_Software_Architecture_Document.md` (SAD-002), `docs/adr/ADR-001..083`.
**Releases:** every stable version (`vX.Y.Z`, from v0.1.0) ships the x86-64 VMware package as GitHub
release assets, boot-verified from its own VMDKs before publishing — `docs/RELEASING.md`.

## Current wave — the desktop under a merciless storm (2026-09-03, ADR-086)

Every window invariant so far was proved on a handful of events: one press, one drag, one close.
That proves the RULES; it says nothing about the ten-thousandth event, when a queue nobody drained
is full and a heap that never frees (ADR-063) has had every chance to grow by a few bytes per
event forever. `kernel-core/src/wmstorm.rs` is the storm, and it is a BOOT suite on all three
CPUs measured on the platform's OWN heap — not a host benchmark.

Six claims: a thousand open/close cycles return the compositor to EXACTLY its starting surface,
placement and window counts with zero refusals; a window that stops draining backs up to exactly
`MAX_INPUT_EVENTS` with every further event refused `Backlogged` AND counted, the drop ledger
equal to `flood - cap` to the event; a drain restores exactly that capacity and not one event
more; four thousand pointer events in the STEADY STATE move the heap watermark by ZERO bytes
(both numbers printed on the boot log, pass or fail); a settled desktop repaints once and then
writes nothing at all, damage ledger empty; and the same storm told twice lands bit-identically,
down to the frame's cost.

**What the storm found.** Claim 4 failed on its first run. The window manager asked
`Compositor::z_order()` on every pointer event — a `Vec` per event — and the live desktop's pump
called `drain_input()` every tick — another `Vec`, thrown away immediately. On a heap that never
frees those are leaks with polite names. Both paths are now allocation-free: `placed_len`/
`placed_at` walk the compositor's own placement table in place, and `pop_input` takes one event
under the same owner-token authority `drain_input` enforces. Measured on x86-64 afterwards:
`[wmstorm] heap watermark across the storm: 7550448 -> 7550448 bytes (0 moved)`.

Proofs: `[wmstorm] ALL 6 WINDOW-STORM INVARIANTS HOLD` on all three targets (marker `wmstorm=6`,
boot fails 740+i), five cross-CPU conformance behaviours, and a host proof under a counting
allocator that IGNORES frees — because the kernel heap cannot give bytes back, and a net-bytes
measurement would let a doubling `Vec` through. NAMED: opening a window allocates by design (its
pixels and its queue), so the allocation round does not press close boxes and a user who opens and
closes windows without end still grows the never-freeing heap — ADR-063's posture, stated rather
than hidden by a suite that avoids it.

## Previous wave — one desktop, three CPUs (2026-09-03, ADR-085)

ALET-P2-021's portability rung. Every contract under the desktop was arch-neutral and proved on
all three CPUs; the thing that RAN them was one target's module. aarch64 and RISC-V proved every
invariant and then showed a black screen, so "Aletheia has a GUI" was true of one target and a
claim about the other two.

`kernel-core/src/desktop.rs` is the desktop as a VALUE, generic over the same seams the drivers
already cross (`Desktop<H: VirtioHal, T: Transport + ConfigWrite>`): the compositor, the input
session, the window manager, both windows' grids, the GPU resource and the pump. Each kernel
keeps only what a CPU can answer for:

* **Ownership and the concurrency posture** — x86-64 masks IF, aarch64 masks `DAIF.I`, RISC-V
  clears `sstatus.SIE`. The shared desktop takes no lock and knows no interrupt flag, so no
  platform's rule is imposed on another.
* **The wake-up** — the PIT (IRQ0) on x86-64, the generic timer PPI (INTID 30, armed from
  `CNTFRQ_EL0/100`) on aarch64, the S-mode timer through SBI `set_timer` on RISC-V. All three
  tick at 100 Hz. Both DT targets' fatal-by-default IRQ paths gained exactly ONE new named
  source, gated to interactive builds and rearmed BEFORE the pump runs, so a slow pump cannot
  silently stop the clock; everything else stays fatal.
* **The frame allocator's reading** — `pump(free, total)` is handed the machine's memory ledger
  rather than calling an allocator.

The DT targets also gained the console's SECOND SURFACE (ADR-083): `emit` writes to the UART and
to the terminal window, `getc` reads the UART ring and then the window's queue, and `input`
answers on all three machines. On aarch64 the desktop is installed BEFORE the SMMUv3 gate, so its
backing frames sit inside the stage-2 identity domain enforcement covers.

Proofs: all three VM gates assert `[desktop] LIVE: ... 2 managed windows` beside the suites they
already ran; `console-e2e` (a scripted operator on all three targets) and `vinput-e2e` (real
QMP-injected device events on x86-64) stay green; `kernel-x86_64/src/desktop.rs` shrank 627 ->
~120 lines and its atomics ledger went with it. NAMED non-claims: the DT desktops pump only on
interactive builds (a gate build composes the first frame and stands still — nothing is claimed
about motion nobody is there to see), and the live injected-input gate still runs on x86-64 only.

## Previous wave — windows are a managed set, not one privileged surface (2026-09-03, ADR-084)

ALET-P2-021's window rung. After ADR-083 the desktop had exactly ONE window, and the code that
decided what a click meant lived in `kernel-x86_64/src/desktop.rs`, written for that one surface
id: a second application could not exist, nothing could be closed, and TWO authorities decided a
single click (`route_pointer_batch` focused the surface under the pointer before anyone could
observe that the pointer was over a close box). `kernel-core/src/wm.rs` is the layer that was
missing between "a compositor with surfaces" and "a desktop with windows".

* **The manager owns the tokens.** `WindowManager::open` mints the surface, places it, and KEEPS
  the owner token, so "who may move, raise or close this window" has one answer. `UnknownWindow`,
  `DuplicateWindow` and `TooManyWindows` are refused BY NAME and counted; a window that could not
  be placed is detached rather than left minted as an unreachable id.
* **Chrome is geometry, and the geometry is the PAINTER's.** `textgrid` paints the close box
  (`CLOSE_W`, `has_close_box`) and `wm::hit_at` classifies a window-local point from the SAME
  constants — painted and clickable cannot disagree — and a window too narrow to carry a name
  beside a box carries no box at all, because chrome that is nearly all close box destroys a
  window on every press near its top.
* **A press is a routing DECISION, reported.** `press` walks the compositor's own z-order front
  to back with the compositor's own visible-rect math (what is clipped away cannot be clicked)
  and returns `Closed` / `Dragging` / `Focused` / `Empty`. It touches no pixel.
* **Close is a LIFECYCLE.** The surface, its input queue and its owner token die together; focus
  falls to the topmost survivor the manager owns, or is CLEARED when none is left so the next
  keystroke is refused `NoFocus` rather than routed at a corpse; a re-opened id is a NEW window
  (fresh token, empty queue, dead old token).
* **One authority per click.** `vinput::route_pointer_motion` moves the cursor — the session's
  own plane — and hands the press back UNDECIDED for the manager. `route_pointer_batch` keeps its
  ADR-080 behaviour for the boot suites.

The x86-64 live desktop now runs TWO managed windows: the console's terminal (ADR-083) and a
system MONITOR reporting free frames, device events, keystrokes posted, drops, refusals, windows
open and closed, drags and focus. **The monitor carries no clock, deliberately** — a panel that
repaints once a second is a compose, a TRANSFER and a FLUSH every second forever on a machine
where nothing happened, which would quietly end ADR-080's quiet desktop. It repaints when a FACT
CHANGES (the pump compares every number the panel prints, whole — not a hash), so an idle machine
still costs two used-ring reads and one damage check per tick. The wallpaper panel stays a plain
surface, not a window: no chrome, no focus, no close, and a press on it is a press on empty
desktop.

Proofs: 12 boot invariants on all three targets (`[wm] ALL 12 WINDOW-MANAGER INVARIANTS HOLD`,
marker `wm=12`, boot fails 720+i), `textgrid` 6 -> 7 for the painted close box, six cross-CPU
conformance behaviours (178 -> 185), 11 host proofs in `kernel-core/tests/wm.rs` (partially-off
windows clicked only where visible, a window closed mid-drag, FocusLost across a press, queues
dying with their window, the ceiling under a long open/close story, and the motion route proved
NOT to take the click decision), and the live gate `scripts/vinput-e2e.sh` 22 -> 30 checks: two
managed windows up, a click focusing the second, a keystroke queued behind it while the console
sees nothing, the CLOSE BOX ending that window, focus falling to the surviving terminal, and the
keyboard typing there again. `InputFacts` gained `windows`/`closes`/`drags` and now reports the
FOCUSED surface's backlog rather than the terminal's. The x86-64 heap grew 8 -> 12 MiB (the
ADR-072 posture): this heap never frees, and at 8 MiB the vt-d gate's page tables were the
allocation that found the ceiling. NAMED non-claims: no resize (a window's grid and surface are
allocated once on a heap that never frees), no minimise/maximise/window list, no keyboard focus
cycling, no live desktop on aarch64/RISC-V (they prove the manager in their boot suites), no
user-mode application owning a window, and close is immediate — what a window held is gone.

## Previous wave — the terminal window is the console's second surface (2026-09-02, ADR-083)

ALET-P2-021's text rung. ADR-080 left the live desktop with a window nobody could read:
keystrokes reached its queue, the queue was drained by nobody, and the window showed a border.
This wave makes the window a TERMINAL without inventing a second shell, session or alphabet.
`kernel-core/src/textgrid.rs` is a `cols x rows` grid of the console's OUTPUT bytes: printable
ASCII lands in a cell, `\n` ends the line (scrolling exactly one row on the last row), `\r`
returns to the column, a wrap at the right edge is a newline, backspace erases exactly one cell,
the editor's `ESC [ ... <final>` sequences are consumed unpainted (a teletype for the console's
stream, not a terminal emulator — named), anything else refused and COUNTED. `render_packed` is
a pure function of the cells into the row-major 1-bpp buffer `Compositor::fill_packed` consumes
(one 8x8 `font8x8` glyph per cell) into a caller-owned buffer allocated once — a changed grid is
one `fill_packed` and no allocation on a heap that never frees; above the text a `TITLE_H` band,
solid ink with the name knocked out, is the strip the pointer drags by. The x86-64 desktop
(`kernel-x86_64/src/desktop.rs`) makes the window that grid: every byte the console emits also
lands in it (`term_write` from the console's `emit`), the console's `getc` asks the serial/PS-2
ring and then the window (`term_getc` — the keystrokes the session routed to the focused window,
drained by their OWNER token into a bounded line), so a virtio keyboard types at the same shell a
serial line does and the answer is painted where the keystroke landed. `vinput::route_pointer_batch`
hands the committed batch back after the cursor move and the click-as-focus, so a LEFT press in
the title band starts a drag (offset from the window's origin) and raises the window, motion
moves it by exactly the pointer's delta (`move_surface`; a fully-off placement is refused and the
window stays), a release ends and counts it; `Compositor::placement` reports where it sits. Two
contexts touch the desktop and never overlap — the IRQ0 pump (IF=0) and the main thread through
`with_desktop` inside `without_interrupts` — serialized by the interrupt flag, no lock, because
the main thread holds console locks an IRQ path must never spin on. `input` prints `window: at
(x, y)` and `terminal: N lines, last "..."` from the grid the compositor paints. Proof: 7 host
tests in `kernel-core/tests/textgrid.rs`; `textgrid_suite` — 6 invariants on all three targets
(`textgrid=6`, fails 700+i; VirtualBox requires the marker); three cross-CPU conformance
behaviors (175 -> 178); the live gate `scripts/vinput-e2e.sh` grows 17 -> 22 — a virtio
keystroke is ECHOED by the console (its owner drained it, `queued` back to zero), `help` typed on
the virtio keyboard is answered (`commands:` on the serial log), the window's last non-blank line is the console's own at (300, 60), and a press on the title band at (400, 64), a move to (420, 90), a
release move the window by exactly the mapped pointer delta with focus kept. Unsafe audit +1 (kernel-x86_64
271: `with_desktop`). Named non-claims, in the register: the grid is a teletype (no cursor
addressing, colors, attributes); one window, no resize/close/second application; the live pump
and the terminal are x86-64's; the i8042 wire still reaches the console directly; input polled
from the tick; alpha, IME, device-level GPU isolation open.

## Previous wave — reclaim under pressure (2026-09-02, ADR-082)

REQ-ML-005, wired. ADR-081 refused a task at the door; the machine ALREADY under pressure still had
to decide whose frames go, and the register had carried the raw material for weeks: `memrisk`, the
second forest trained on the eviction event, measured, exported, "NOT CLAIMED AND NOT WIRED".
`kernel-core/src/reclaim.rs` wires it the way ADR-056 wired the first forest — as an ORDERING, never
a verdict with authority. Three parties, three jobs: the ALLOCATOR triggers (only a `MemoryMeter`
under the watermark opens a round, `NotUnderPressure` refused by name otherwise, the NEED being the
shortfall to `HEADROOM_FACTOR` x the watermark); the POLICY chooses (a total order — protected
candidates never chosen and counted even when that leaves a SHORTFALL the round names rather than
hides, then tier, then largest footprint, then lowest priority, then oldest, then id — so two rounds
over the same inputs evict the same tasks in the same sequence); the FOREST advises the tier
(`Elevated` = the trace says this task would have been evicted anyway, cheapest to lose, first;
`Low` = likely to complete, taking its frames destroys work, last; abstain / out-of-box / degenerate
/ NO MODEL = the middle tier, so a machine without the blob or with one the loader refused by name
ranks bit-identically to one whose forest abstains about everyone). The blob is verified by the SAME
`RiskAdvisor::load` — no second loader, no second contract hash. Execution is one seam,
`ReclaimOps::evict(task, owner) -> frames`, asked exactly once per chosen task, whose ANSWER the
policy counts (a stingy seam makes the round keep evicting until the need is met). On every target
a REAL storm: a storm owner takes frames from the machine's own allocator until the meter is under
the watermark (the resident advisor's pressure ledger counts the crossing), the reclaimer is handed
the storm as its one candidate and the real ops seam walks the ownership table, and the free count
is restored EXACTLY — `StormReport::holds` is the verdict, and a storm that took nothing proves
nothing (~25 800 frames on the 128 MiB `virt` machines, ~58 000 on the 256 MiB q35). Proof: 7 host
tests in `kernel-core/tests/reclaim.rs`; `reclaim_suite` with 9 invariants on all three targets
(`reclaim=9`, boot fails 700+i / 699; the three QEMU gates also require the storm's verdict line);
five cross-CPU conformance behaviors (170 -> 175). Unsafe audit UNCHANGED. Named non-claims, in the
register: the reclaimer is not yet RESIDENT (it runs at boot — suite and storm — and is not consulted
by a running machine's allocator on its own pressure; that seam, and which live tasks are candidates
or protected, is the next rung); the forest sees submission-time vectors only (frozen contract); whole
tasks only, no swap, no compression; constants for watermark and headroom; the storm's candidate
carries a zero vector so the live path exercises the policy, the seam and the allocator while the
forest's opinion is exercised by invariant 7 over ADR-056-shaped tasks.

## Previous wave — memory is a boundary the advisor cannot cross (2026-09-02, ADR-081)

REQ-ML-006. The resident advisor (ADR-056) answered "is this task going to die if I admit it?"
with an ordering hint; nothing asked whether the task could be admitted at all — its requested
frames were a FEATURE, never a BOUND. This wave makes memory reach the decision the only honest
way available today: as a hard boundary the allocator states and the forest cannot cross.
`MemoryMeter { total_pages, free_pages }` is one reading of the machine's own frame allocator;
`RiskService::observe_memory` records it or refuses it by name (`MeterInvalid` — zero frames, or
more free than exist — records nothing), keeping the last reading, the exact low-water mark and a
pressure ledger that counts a crossing INTO the band below `LOW_WATERMARK_PERMILLE` (10 %) once
per entry. `RiskService::admit_bounded` — and the resident seam `resident::admit`, which IS that
door, there being no second admission path — refuses `Unmetered` while no allocator has reported
(fail closed, never "unbounded until told") and `MemoryExhausted { requested, free }` when the task
asks for more frames than are free, both BEFORE the model is consulted (the advice census does
not move), without touching the scheduler (no state, nothing dispatched) and without touching the
feature history (a refused task never existed); exactly-free is admissible (`<=`). Every target
reads `frames::free_count`/`total_count` into the service before anything is admitted and prints
the reading the door judges by; the real ring-3 tasks read the allocator immediately before each
admission and a refusal there is printed and fails the suite; `commission` sizes its synthetic
workload against the machine's OWN free frames (a rounding error to every free frame) and reports
refusals — a target whose allocator never reported exits 187 rather than reporting a census of
nothing. `mlstat` prints the ledger or names the unmetered state. Proof: 6 host tests in
`kernel-core/tests/mlsched.rs` (unmetered door admits nothing; invalid meter recorded nowhere;
by-name refusal with scheduler and history untouched and exactly-free admitted; ledger with exact
low-water and once-per-entry crossings; deterministic latest-reading boundary; the resident seam
carries the same door with a 256-task commissioning refused nothing); `mlsched_suite` 12 -> 17 on
all three targets (`mlsched=17`; the three QEMU gates also require the allocator's reading line
and a commissioning refused nothing); five cross-CPU conformance behaviors (165 -> 170). Unsafe
audit UNCHANGED: the rung adds no unsafe site. Named non-claims, in the register: RAM pressure is
not a forest feature (the 20-column contract is frozen and hash-checked; a column means
retraining); the eviction-event forest (REQ-ML-005, `memrisk`) stays UNWIRED — nothing reclaims
on the advisor's opinion; the boundary is per-admission against the latest reading and reserves
nothing (the allocator refuses the actual allocation, ADR-030); the watermark is a constant; the
service is still installed with the suite machine's capacity for feature normalisation.

Same day, before this wave: v0.1.0 was cut on `a52d48b` and RELEASED — `scripts/release-vmware.sh`
built both disks, booted the packaged VMDKs under QEMU+OVMF on the runner and
`.github/workflows/release.yml` published the zip, its digest and SHA256SUMS as the tag's release
assets (REQ-REL-001, `docs/RELEASING.md`); and three CI jobs red since ADR-074 for reasons only the
runner could see were fixed — the ubuntu-24.04 runner's QEMU 8.2 creates the virt machine's SMMUv3
stage-1-only (`-global arm-smmuv3.stage=2` in `scripts/vm-e2e.sh`), a newer stable clippy rejects
`chunks_exact(2)` (`kernel-core/src/udpv4.rs` uses `as_chunks`), and GNU grep reads `\r` in an ERE
as the letter `r` (`scripts/comparative-bench.sh` carries a real CR byte) — the first fully green
`aletheia-ci` on this lineage.

## Previous wave — real devices through the input session (2026-09-02, ADR-080)

ALET-P2-021's input HARDWARE rung. ADR-079 built the input path as an authority question and
named its own limit: no REAL device was wired through it. This wave wires two, on every CPU,
and on x86-64 lets the machine RUN the whole graphics stack at once. `kernel-core/src/vinput.rs`
is one virtio-input driver body (VIRTIO 1.2 §5.8, device id 18) over the existing transport
seam on both buses: the eventq the device fills with `{type, code, value}` records, the statusq
created, armed and never fed, identity/event-bits/axis geometry read from the config space
through the select/subsel pair — the one config WRITE this kernel's transports had never
needed, now the `ConfigWrite` seam (a read-modify-write of the aligned word on virtio-mmio, a
volatile store into the device-config capability on virtio-pci). Devices are classified by
their OWN names, never by slot order; a lone device, a failed bring-up, an unclassifiable
device are refused by name. `KeyDecoder` turns Linux keycodes into `keymap::Keys` under the
ONE rule the PS/2 decoder already obeys — only bytes the editor has a rule for — swept on the
host over the whole keycode space in every reachable modifier state, sharing the grammar
(`csi`/`csi_delete` public now) but never the modifier STATE with the PS/2 decoder.
`PointerDecoder` commits absolute-axis batches on SYN (half-batches refused), maps the
device's OWN pinned range (0..32767) onto the scanout exactly and monotonically, and treats a
LEFT press as a FOCUS decision: `Compositor::focus_at` focuses the topmost PLACED surface
whose VISIBLE area covers the point, clears focus on empty space, tells the loser through its
own queue, queues nothing on the already-focused surface, refuses a forged session, and
ignores button autorepeat. `route_key`/`route_pointer` are the shared functions the boot suite
drives with synthetic records and the live desktop pumps with hardware records.
`kernel-x86_64/src/desktop.rs` installs the LIVE desktop BEFORE the VT-d gate (its GPU and input
grants inside the per-device windows), one compositor over the console's geometry, one
session, a panel and a window under owner tokens, the cursor over the window; the IRQ0 (PIT)
handler pumps it at 100 Hz — at most one queue depth per device per tick, a compose only when
something owes a repaint, a device command only when the model reports pixels written, exactly
one TRANSFER + one FLUSH per changed frame, nothing at all on a quiet tick — with exactly ONE writer and facts published as single machine words the new
`input` console command (`shell::InputFacts`, `Safe` in the hosted operator) reads LIVE; a
target without a desktop says so instead of printing zeros. Proof: 10 host tests in
`kernel-core/tests/vinput.rs` plus the click-to-focus routing table and the quiet-tick proof in
`tests/input.rs` (the pump composes only when `has_pending_damage`, allocation-free — an idle
tick touches no heap on a heap that never frees); 10 boot
invariants against the REAL devices on all three targets (`[vinput] ALL 10 INPUT-HARDWARE
INVARIANTS HOLD` — identity pinned from config space, DMA-gated queues, armed silence
MEASURED over 10 000 polls, keyboard record through the decoder into the focused queue,
pointer record to the exactly mapped position, click routes focus, unknown records counted,
whole-alphabet proof, axis edges and clamps, keystroke composes nothing; init fails 679, suite
fails 680+i; `input_suite` 12 -> 13); seven cross-CPU conformance behaviors (158 -> 165); the
LIVE gate `scripts/vinput-e2e.sh` (its own CI job) drives the pump with QMP-injected events —
typed over the serial wire (COM1 a socket the harness attaches before the guest runs) — the
hardware wire starts silent, the cursor lands where the device's axis range says, click focuses
/ empty click clears with the loser told (one FocusLost in its queue) / clicks post no keystroke,
one virtio keystroke posts exactly one byte
queued behind the focused window while the console sees nothing (QEMU hands injected keys to
the virtio keyboard once it is DRIVER_OK, so the two wires are distinct by construction), quiet
counters hold still. Marker
maps changed deliberately (`input=13`, `vinput=10` on the three QEMU gates; VirtualBox requires
the routing marker and lists the hardware marker SKIP — it emulates no virtio-input). The
unsafe audit CHANGED and is counted: kernel-core +14, kernel +1, kernel-x86_64 +12,
kernel-riscv64 +1, every site with its SAFETY argument. The first live run found the tick
never reached the pump (the ring-3 suite leaves IRQ0 on its preemption entry and the console
masked it): `idt::restore_timer` re-points IRQ0 at the plain tick handler after that suite, and
`conirq::init` keeps IRQ0 unmasked when `desktop::is_live` — a console-only machine quiets it
as before. Both UEFI e2e scripts now put the
rustup shim first on PATH (E0463 under a Homebrew cargo — how this gate first failed locally).
Named non-claims, in the register: the live pump is x86-64's (aarch64/RISC-V prove the path in
the boot suite, install no desktop yet), input is polled from the tick, the PS/2 wire stays the
console's, no relative pointers, no hotplug, statusq unused, right button counted not routed,
no device-level GPU isolation, no alpha, no text/IME.

## Previous wave — the composed desktop meets input (2026-08-29, ADR-079)

ALET-P2-021's input-routing rung. ADR-078 made the composed frame visible; this wave makes the
desktop ANSWER, by decomposing input into the two questions this kernel already knows how to
answer plus one new possession. The input path is ONE principal: `open_input_session` mints
exactly one session token per compositor and every event post, focus change and cursor move
answers to it — a second opening is refused `InputSealed`, absent/wrong/forged tokens are all
`NotInputSession`, refused by name and counted. At most ONE surface is focused, only a placed
one; refocusing tells the loser `FocusLost` through its own bounded queue (dropped and counted
if it stopped draining); refocusing the already-focused surface queues nothing. `post_key`
routes a decoded keystroke — the keymap's own alphabet, never a raw device byte — to the
focused surface's `MAX_INPUT_EVENTS`-bounded queue; `drain_input` empties it, OWNER TOKEN
ONLY: the input path decides WHERE events go, the owner decides WHO reads them, and a wrong
owner token here is the same refusal a forged draw token is. Delivery is seq-monotonic and
exactly-once; a keystroke with nothing focused is refused `NoFocus` and exists nowhere; an
overflow is refused `Backlogged` and counted without evicting the backlog; draining restores
capacity exactly; detaching the focused surface clears focus and the queue dies with the
surface — a re-minted id starts empty and the old token is dead. The cursor is the
compositor's OWN plane — not a surface, no token, no z-order slot: an 8x8 transparent
crosshair that only the session may move or hide, refused `CursorOffScanout` with the position
named when its glyph could never show a pixel, clipped exactly when partially off (the
guard-band proofs hold through every edge), painted LAST above every surface as a mask (window
ink shows through the transparent bits; raising a window changes nothing), visible the SAME
frame through the same damage machinery, free when it does not move, and REPORTED —
`FrameStats` gained `cursor_pixels`. And input is not pixels: a keystroke with no repaint
damages nothing; a quiet frame stays quiet.

Proofs: 8 host tests in kernel-core/tests/input.rs (the boot suite host-run first; the session
table swept fail-closed; the focus decision table over every reachable state × every op target;
the routing/reading split swept over a four-surface alphabet delivery with zero raster puts
across a whole post/drain cycle; the bounded-queue matrix at capacity; the cursor's authority
and exact clipped geometry against a guard-band raster; the punch-through matrix; determinism
over a mixed input sequence) plus 12 boot invariants on all three targets (`[input] ALL 12
INPUT-ROUTING INVARIANTS HOLD`, boot fails 660+i), four pinned cross-CPU in the conformance
contract (154 -> 158). Marker maps changed deliberately (`input=12` on the three QEMU gates;
VirtualBox adds the family to its REQUIRED list — the suite is arch-neutral and proves there
too, unlike the virtio-gpu families it lists SKIP, ADR-061). The unsafe audit is UNCHANGED:
the rung adds no unsafe site. Named non-claims, in the register: no REAL input device is wired
through the session yet (the console's PS/2/serial path still feeds the serial console; the
hardware rung is next), no pointer hardware (the cursor moves through the session API), no
flow control beyond the bounded queue, no text rendering, no IME, no alpha, no device-level
GPU isolation, no interrupt-driven completion.

## Previous wave — the composition contract meets the scanout (2026-08-28, ADR-078)

ALET-P2-021's real-pixel rung. ADR-077 defined who may draw and where; this wave puts the
verdict where a human can see it, without giving the device any authority it did not have.
`ComposeSink` (`kernel-core/src/fbcon.rs`) implements the compositor's `Raster` over the
framebuffer console's scatter-gather backing frames — the same page list a virtio-gpu 2D
resource names as its backing store — and counts every put and every REFUSAL: the model's
structural bound and the raster's bounds must agree EXACTLY, and a non-zero refusal would name
the disagreement. `compose_suite` (`kernel-core/src/virtiogpu.rs`) then drives the real device:
resource created, 150 DMA-gated pages attached, scanout bound; the first compose read back
pixel-exact from real memory and flushed as exactly TWO commands; a quiet frame writing ZERO
pixels and issuing ZERO device commands; a wrong token changing no real byte and no traffic; a
move visible the same frame with no ghost; the z-order flipping on real pixels; a window pushed
160 px past the right edge landing only its intersection with the sink never asked for an
out-of-bounds pixel; and the teardown revoking every page's DMA registration with the DEVICE
itself confirming the end (`ERR_INVALID_RESOURCE_ID`).

Proofs: 5 host tests in kernel-core/tests/compfb.rs (composed frames read back pixel-exact,
moves leave no ghost in real bytes, overhang never asks outside the raster, a wrong token
changes no real byte, and the device legs — move, clip and z-flip — are visible in real host
pages without QEMU) plus 8 boot invariants on all three targets (`[compose] ALL 8 REAL-PIXEL
COMPOSITION INVARIANTS HOLD`, boot fails 640+i), two pinned cross-CPU in the conformance
contract (152 -> 154). Marker maps changed deliberately (`compose=8` on the three QEMU gates;
VirtualBox lists the family SKIP with the rest of the graphics stack — no virtio-gpu,
ADR-061). The unsafe audit grew ten named sites (the live-device ops inside the suite).
Named non-claims, in the register: the device still enforces nothing about who composes — the
enforcement lives in the kernel-side contract layered over the DMA registry; one command pair
per changed frame is not interrupt-driven completion; no alpha beyond the 1-bit model depth, no
cursors, no input routing, no device-level GPU isolation between surfaces.

## Previous wave — the composition contract is modeled, not assumed (2026-08-28, ADR-077)

ALET-P2-021's compositor rung. The GUI question decomposes into the two questions this kernel
already knows how to answer: WHO may put pixels on the scanout (an authority question) and
WHERE those pixels may land (a bounds question). `kernel-core/src/compositor.rs` defines the
contract as a complete software model: surfaces are minted with unforgeable possession-based
OWNER tokens and every op — attach, move, raise, lower, detach, and each pixel write — is
refused `NotOwner` without the right one; placements are clipped to the scanout EXACTLY
(hangs off any edge -> intersection only; could-never-show-a-pixel placements refused at
attach AND move; the write loops only visit pixels that exist in both surface and scanout —
the host proofs run against a GUARD-BAND raster whose out-of-bounds-put counter must stay
zero); the painter's order is the z-order and only the owner may change it; packed buffer
fills are SIZE-HONEST (short can never overread, long can never smuggle, refused fills leave
the surface untouched); placement changes are VISIBLE the same frame — attach/move/detach/
raise/lower damage the screen regions they vacate and cover, and compose clears each damaged
region to background before repainting it through the z-order, so a moved surface leaves no
ghost and a damaged bottom surface cannot paint over the windows above it; damage ledgers
are bounded and coalesce (summarized, never lost); an unchanged frame visits NO region and
writes ZERO pixels, with every frame's cost REPORTED (`FrameStats` — the measured shape of
"maximum performance", ADR-064's posture); and identical op sequences land bit-identical.

Proofs: 11 host tests in kernel-core/tests/compositor.rs (four-edge clip sweep with
per-pixel oracles, the ownership table over every op, the buffer-honesty matrix,
placement-damage visibility, exact damage accounting, bounds/capacity, token non-reuse,
determinism) plus 14 in-kernel invariants booting on all three targets (`[compositor] ALL 14
COMPOSITION-CONTRACT INVARIANTS HOLD`, boot fails 600+i), six pinned cross-CPU in the
conformance contract (146 -> 152). Marker maps changed deliberately (`compositor=14`,
ADR-061). Named non-claims, in the register: no real-pixel compositor leg over the virtio-gpu
flush path yet (QEMU's virtio-gpu can SHOW a frame but enforces nothing about who composes
it — the ADR-071/076 posture), no alpha blending beyond the 1-bit depth the framebuffer
console already runs, no cursors, no input routing, no device-level GPU isolation between
surfaces.

## Previous wave — the power/performance contract is modeled, not assumed (2026-08-28, ADR-076)

ALET-P2-022 leaves the deferred column. The wave answers the OS's overclocking promise the way
this kernel answers every privileged act: frequency is AUTHORITY, heat is a HARD CEILING.
`kernel-core/src/pm.rs` defines the contract as a complete software model: every core belongs to
a frequency DOMAIN with an honest discrete ladder (registration refuses dishonest ladders by
name); the governor range (at or below nominal) is free to any caller; the OVERCLOCK band above
nominal exists only through a LIVE, per-domain elevation grant — attenuated on delegation
(a child ceiling never widens its parent, `Amplification`/`CrossDomain` refused), revoked with
cascade, and clamping the domain back to nominal the moment its grant dies (a governor-range
grant clamps nothing); the thermal ENVELOPE is absolute BY CONSTRUCTION — no ladder point above
it can register and no grant past it can mint, so no reachable state exceeds it, whatever
authority says; a thermal TRIP clamps every domain to its lowest point and latches a tick-exact
cooldown that refuses elevation BY NAME even with a valid grant while the governor range keeps
serving; the demand governor never enters the OC band and never parks demanded silicon
(`DomainBusy`), parking zero-demand domains instead (the idle machine costs nothing, ADR-056);
idle residency and wake latency are accounted exactly, with a clock change CLOSING a parked
span so real time is never lost; device power moves only along legal arcs (D3→D1 refused — wake
through D0 or not at all); and every accepted act and every refusal lands in a bounded audit
ledger under a monotonic sequence, the holder named on grant acts.

Proofs: 19 host tests in kernel-core/tests/pm.rs — the full OC-band decision table over every
point × ceiling × authority state, a 5^3 attenuation-chain sweep, revocation clamps and
idempotence, envelope absoluteness from registration and mint, cooldown tick-exactness across
the whole window, idle accounting under transition interference, the complete device-arc table,
ledger completeness with wraparound, capacity bounds, and bit-identical determinism — plus 14
in-kernel invariants booting on all three targets (`[pm] ALL 14 POWER-PERFORMANCE INVARIANTS
HOLD`, boot fails 560+i), six of them pinned cross-CPU in the conformance contract. The marker
maps changed deliberately (`pm=14`, ADR-061). Named non-claims, in the register: no
MSR/CPPC/ACPI programming (QEMU TCG exposes no frequency control to the guest — a hardware rung
attempted today could only prove code ran, not that anything enforced; the ADR-071 posture),
no battery, no system sleep/wake, no voltage rail enforcement beyond recording mV, no
thermodynamic simulation — callers report temperatures, the contract decides.

## Previous wave — per-device DMA windows (2026-08-26, ADR-075)

ALET-P1-018 advances to its third hardware rung. The registry-driven narrowing lands on VT-d:
every DRIVEN function now gets its OWN second-level tree containing exactly the frames ITS
driver registry vouches for (leaf-set equality audited live against sorted spans), ungranted
functions get NO context entry and the gate reads their absence back from the live context
table, grant sets are pairwise disjoint or the boot refuses, and revocation granularity drops
to ONE PAGE: the block device data-frame leaf is revoked under enforcement and the unit answers
with an ACTIVE record naming source-id AND address with MEASURED reason 6 (PAGING_NOT_PRESENT,
pinned beside the ADR-073 codes 2/4/5) while sibling windows keep serving; restore returns
read-back equality and silence; enforcement stays latched layered over the software registry.
dmar 12 -> 14; host proofs tests/vtd.rs 12 -> 15. En route the wave EXPOSED and FIXED a
repo-wide boot breaker: the ADR-074 seam mapped [bar_base, len) instead of bar_base+offset, so
q35 device-cfg ran unmapped and every target gate died in an infinite mis-labelled ring-3 fault
loop (commit fix(pci), found by bisect plus CR3/translate instrumentation). Named at the
boundary: SMMUv3 per-stream windows, device-side walk probes on ARM (QEMU 11.1 artifact),
interrupt remapping, queued invalidation and pass-through types stay open in the gap register.
## Previous wave - the IOMMU contract crosses the ARM fence (2026-08-26, ADR-074)

ALET-P1-018 advances to its second hardware rung. DELIVERY on aarch64: kernel_core::smmu programs
the ARM SMMUv3 QEMU emulates on virt - discovered through the machine's own device tree, delivered over
the firmware configuration channel (the same door as the custody anchor; direct -kernel ELF boots get
NO DTB pointer at all - measured x0=0), stage-2-only identity domain over OWNED frames minus image, every
present PCI function granted an STE under its DECLARED iommu-map stream id, stream table + command/event
queues published with readback, enforcement enabled through CR0->CR0ACK and latched layered over the software
DMA registry: a 10-invariant boot gate (smmu=10) on top of 15 host proofs against a simulated unit and a
device-side walker built from the emulator's own decoder shapes. The virtio-pci transport moved ONCE into
kernel-core (PciEnv seam) when this wave became its second consumer; the aarch64 kernel became its own PCI
firmware (BAR sizing + assignment) because bare-metal boots run none. NAMED at the boundary: CLI-attached
virtio-pci DMA does not traverse the legacy iommu=smmuv3 unit on QEMU 11.1 (abort-canary measured), so
grant-serves/revocation-events stay open in the gap register beside ADR-073's completion-loss artifact.


## Previous wave — the IOMMU contract is programmed into real silicon (2026-08-25, ADR-073)

ALET-P1-018 advances to the first hardware rung. DELIVERY on x86-64:
`kernel-core/src/vtd.rs` (the wire: register map, root/context encodings, second-level domain
builder, auditor, controller with named refusals) + `kernel-x86_64/src/vtd.rs` (the platform:
ACPI DMAR/DRHD discovery, UEFI-map spans minus the kernel image, per-bus-0-function context
entries, and a 12-invariant live gate). The gate adopts the root (SRTP), turns enforcement ON
(TES observed), kicks the LIVE block functions this boot already drives, and takes its evidence
from the unit's own fault bank: the granted function walks CLEAN; revoking a function's context
and kicking produces an ACTIVE record naming its source-id with reason CONTEXT_ENTRY_P; restoring
the grant returns that function to silence; enforcement stays latched until halt
(`[dmar] ALL 12`, marker dmar=12). Drivers negotiate VIRTIO_F_IOMMU_PLATFORM whenever offered.
Two register-interface facts were forced by the live unit and are documented in ADR-073: the
fault bank is WRITE-ONE-TO-CLEAR, and QEMU serves FSTS at 0x34 where the spec puts 0x30 — so
enforcement EVIDENCE comes from the fault-record BANK (exact everywhere), not from FSTS.PPF.
Boot order changed deliberately: every DMA-dependent suite runs BEFORE the vt-d gate (devices are
brought up before enforcement — how real platforms meet an IOMMU) and the gate is last, because
what it turns on stays on until halt. Named non-claims: SMMUv3 delivery, per-device windows,
interrupt remapping, queued invalidation, pass-through types, and post-enable completion
assertions — QEMU 11.x TCG loses virtio completions across a mid-run enablement ('bogus descriptor
or out of resources'); the full evidence trail is in ADR-073.

## Previous wave — the custody anchor crosses the platform boundary (2026-08-24, ADR-072)

ALET-P1-034 closes completely. DELIVERY: \`kernel-core/src/bootroot.rs\` + per-target fw_cfg
transports hand the vault its 32-byte root over the platform channel; only Delivered(exactly 32)
opens a vault, and RootNotProvided / FirmwareAbsent / MalformedRoot are refused BY NAME. DECISION:
image and entity store stay two commits but are mutually detectable — each paired commit writes
the vault generation inside the durable entity record, and custody-open enforces
witnessed_generation <= keystore_counter, converting ADR-070's pinned undetectable residual into
a named refusal. Proofs: host sweeps in tests/bootroot.rs (lying directories, truncations,
wrong sizes, constructed pair-rollback, fault-at-every-pair-position) plus [vault] ALL 14
CUSTODY-DELIVERY INVARIANTS HOLD on real firmware + real persistent media on all three targets.
Every QEMU gate gained a THIRD rootless boot proving absence seals the vault while the machine
continues; marker maps gained vault=14 deliberately (ADR-061). Heap grew 8 -> 12 MiB on the DT
targets to hold the resident custody state (ADR-063 posture).

## Previous wave — the IOMMU contract is modeled, not assumed (2026-08-23, ADR-071)

ALET-P1-018 advances: `kernel-core/src/iommu.rs` defines and proves the full enforcement semantics
of a hardware IOMMU as a software model (`SoftIommu`), so every proof runs on the host today and
a hardware implementation must satisfy the same contract. Nine invariants boot on all three
targets; seven are pinned cross-CPU. The gate-marker map changed deliberately (`iommu=9`).
Hardware realization (VT-d/SMMUv3 programming) stays scoped in the gap register.


## Current wave — authority custody is a lifecycle, not a caller-supplied key (2026-08-23, ADR-070)


The custody and rotation halves of ALET-P1-034 close, because they were one gap: `capstore` could
authenticate a persisted registry only under a key the CALLER handed in on every call, so custody
was nobody's, rotation was impossible, and every boot re-asked the question a keystore exists to
answer. The constraint that shaped everything: the kernel has NO entropy source at boot, so
randomness could not be the mechanism — the lifecycle had to be safe BY CONSTRUCTION.

* **The root is custody; working keys are derived.** `CapVault::open` takes the 32-byte root once,
## Gates executed in CI

Both pipelines (GitHub Actions and GitLab CI) execute exactly these scripts, each asserted by
scripts/check-ci-parity.sh against this file: scripts/build-all.sh (every crate on its own
toolchain, host crates tested), scripts/check-boundary-docs.sh, scripts/check-ci-parity.sh,
scripts/check-register.sh, scripts/check-traceability.sh, scripts/comparative-bench.sh,
scripts/conformance.sh (the cross-CPU core contract), scripts/console-agent-e2e.sh,
scripts/console-ai-e2e.sh, scripts/console-e2e.sh, scripts/keyboard-e2e.sh, scripts/vinput-e2e.sh (the live
input-hardware rung),
scripts/quality-gate.sh, scripts/release-vmware.sh (the VMware package — both disks built,
packaged and BOOTED from their own VMDKs on every push; on every `vX.Y.Z` tag
.github/workflows/release.yml publishes the same package as GitHub release assets, REQ-REL-001,
docs/RELEASING.md), and the four VM gates — scripts/vm-e2e.sh (aarch64),
scripts/vm-e2e-riscv.sh (RISC-V), scripts/vm-e2e-x86.sh (x86-64 under OVMF) and
scripts/vm-e2e-vbox.sh (VirtualBox, the second-hypervisor rung).
