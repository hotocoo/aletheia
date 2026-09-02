# ADR-080: the input hardware rung — real devices through the input session, and the desktop goes LIVE

**Status:** Accepted · **Date:** 2026-09-02 · **Advances:** ALET-P2-021 (the input HARDWARE rung; the live pump is x86-64's for now, device-level GPU isolation, alpha and text/IME stay scoped) · **Builds on:** ADR-079 (the input session, focus as routing, the cursor plane), ADR-078 (the composed frame on the real scanout), ADR-077 (the composition contract), ADR-057 (virtio-gpu on the shared virtqueue substrate), ADR-049/050 (the keyboard's fail-closed console alphabet), ADR-041 (the multi-queue virtqueue substrate), ADR-073/075 (VT-d per-device DMA windows), ADR-064 (the machine measures itself)

## Context

ADR-079 built the input path as an authority question — exactly one session, focus as the
routing decision, the owner alone reading — and named its own limit in the register: no REAL
input device was wired through it. The console's PS/2 and serial bytes still fed the serial
console, the cursor moved only through the session API, and a desktop nobody could steer with
hardware was a mockup with good manners. This wave closes that rung the way every driver
here is closed: a device the machine actually has, one driver body over the transport seam on
both buses, a decoder whose output alphabet is a SECURITY BOUNDARY, a boot suite that proves
the path against the real device on all three targets, and — on x86-64 — a LIVE desktop the
timer tick pumps, driven in CI by events the emulator itself injects.

## Decision

### One driver body, both buses, one config WRITE

A keyboard is a PS/2 controller on x86-64 and nothing at all on the ARM and RISC-V `virt`
machines; a USB stack does not exist here. virtio-input (VIRTIO 1.2 §5.8, device id 18)
exists on all three targets behind the transports this kernel already speaks, so
`kernel-core/src/vinput.rs` puts the SAME decode contract under the SAME gates on every CPU.
Two queues: the **eventq** the device fills with `virtio_input_event { type, code, value }`
records as fast as the driver re-posts buffers, and the **statusq** — created, armed, never
fed: the device under QEMU never sends on it, and buffers the driver does not understand how
to harvest are buffers it should not offer. Device identity, event-type bits and the
pointer's axis geometry come from the config space through a select/subsel register pair —
the one config WRITE this kernel's transports had never needed, and therefore the one the
new `ConfigWrite` seam names: a read-modify-write of the ALIGNED word on virtio-mmio
(neighbouring bytes written back exactly as read), a volatile store into the device-config
capability on virtio-pci. Devices are classified by their OWN declared names, never by slot
order (an attachment artifact of the emulator command line): `QEMU Virtio Keyboard` and
`QEMU Virtio Tablet` are the pair this rung brings up; a lone device, a device that fails
bring-up, and a device that will not say what it is are all refused BY NAME.

### The decoder's output alphabet is a security boundary

`keymap::Keymap` states the console's byte alphabet against `shell::editor_accepts`, and the
PS/2 decoder proved "everything I emit, the editor understands" over its entire input space
(ADR-049/050). virtio-input hands the driver **Linux keycodes** — a different wire alphabet —
so `KeyDecoder` is a second decoder with the same ONE rule: it emits only through
`keymap::Keys`, only bytes the editor has a rule for, and the host sweep proves it over the
whole keycode space in every reachable modifier state. A device an attacker may be holding
cannot manufacture a control byte the console has no answer for. The two decoders share the
GRAMMAR (`keymap::csi` / `csi_delete` are public now, because two decoders must not grow two
grammars) but NOT state: a virtio keyboard's modifiers are held separately from the PS/2
`Keymap`'s shift, because two devices sharing one modifier bit would let either one hold the
other's shift down. Records the decoder does not model are refused by name and COUNTED —
they change nothing.

### The pointer is the cursor; the click is a routing decision

`PointerDecoder` accumulates absolute-axis samples and commits them on `EV_SYN` (a
half-batch is refused, never guessed); the axis range is the DEVICE'S OWN, read from
`abs_info` and pinned (0..32767), and the mapping `(v * span) / (max + 1)` clamped to
`span - 1` is exact and monotone at every edge — proved on the host over the corners and
against the boot suite's oracle. A committed batch moves the compositor's cursor plane
through the session (the plane ADR-079 reserved for exactly this). A LEFT button press is
not a cursor move: it is a FOCUS decision, so `Compositor` gains `focus_at` — the topmost
PLACED surface whose VISIBLE area covers the point takes focus (the same visible-rect math
`blit_region` uses: what is on screen is what can be clicked), a click on empty space CLEARS
focus because "nowhere" is a place the user pointed at, the loser is told `FocusLost`
through its own bounded queue, a click on the already-focused surface queues nothing, a
forged session token is the same `NotInputSession` every input op gives, and a button
autorepeat is not a click (a held button never machine-guns the focus). `route_key` and
`route_pointer` are the shared functions the boot suite drives with synthetic records AND
the live desktop pumps with hardware records — what the suite proves is what the machine
runs. Routing is not pixels: a click changes no byte of the scanout.

### The live desktop (x86-64)

`kernel-x86_64/src/desktop.rs` is where the machine finally RUNS all of it at once: one
compositor over the framebuffer console's geometry, ONE input session, a wallpaper panel and
a window under their owner tokens, the cursor the tablet steers, and a pump the IRQ0 (PIT)
handler calls at 100 Hz that drains the devices (bounded: at most one queue depth per device
per tick), routes what they say through the session, composes ONLY when the model reports it
wrote pixels, and hands a changed frame to the display device as exactly one TRANSFER plus
one FLUSH. The idle tick is two used-ring reads and one ALLOCATION-FREE damage check
(`Compositor::has_pending_damage`) — no compose, no heap, no device command (ADR-056's GUI
twin, measured by `compose_suite`). The check exists because `compose_frame` clones the
z-order to walk it, and on the boot heap — which never frees, ADR-063 — an allocation per
quiet tick at 100 Hz would have been a leak by another name; a CHANGED frame still pays that
clone once, per event, never per tick. The concurrency posture is stated, not
implied: after `install` the desktop has exactly ONE writer (the PIT context); the main
thread reads only `AtomicU64` facts, one aligned machine word each, so a concurrent pump
cannot tear a readout. `install` runs BEFORE the VT-d gate turns enforcement on (the ADR-073
ordering) and returns the GPU function's FULL grant list; the input functions' grants are
captured beside it, so every page the desktop will ever touch is inside a per-device window
the pump never widens. On any failure the desktop does not come up: the machine continues
on the serial console and the failure is NAMED on the boot log.

The tick has to REACH the pump, and the first live run showed it did not: the ring-3 suite
re-points IRQ0 at its register-exact preemption entry (which resumes a scheduler that exists
only while that suite runs) and the console's interrupt bring-up masked IRQ0 as a matter of
course — so the desktop installed, published its install-time facts once, and was never
pumped again. Two named steps close that: `idt::restore_timer` gives IRQ0 back to the plain
tick handler (PIT count + pump) right after the ring-3 suite, before anything can re-enable
interrupts; and `conirq::init` now asks `desktop::is_live` — a machine that installed a
desktop keeps IRQ0 unmasked through the console (`pic::unmask_timer`, read-modify-write like
its sibling), a console-only machine quiets it exactly as before. The pump is bounded, so the
console's tick costs are the 100 used-ring reads per second the module already stated.

### The console can ask

`shell::InputFacts` is the session's ledger, rendered: events posted, dropped, refused,
queued behind the focused surface, the cursor position, the focused surface id, and the raw
event counts per device. The new `input` console command (classified `Safe` in the hosted
console operator, like every other readout) prints it LIVE; a target with no desktop says
`no machine input session on this target` instead of rendering zeros that would look like a
session nobody can steer.

### Host proofs

`kernel-core/tests/vinput.rs` (10 tests): the full keycode space in every modifier state
never leaves the editor's alphabet; the US layout lands on the same keys the PS/2 decoder
types; modifiers are held state and ctrl chords obey the editor; pointer mapping is exact,
monotone and fail-closed; batches commit on SYN and half-batches are refused; `route_key`
delivers to the focus and propagates its refusals; `route_pointer` moves the cursor and
clicks decide focus; clicks route but only the owner reads; unknown records are counted and
change nothing; identical event sequences land bit-identical. `kernel-core/tests/input.rs`
gains the click-to-focus routing table swept over the reachable z-order × point space
(overlaps answer to the top surface, raising changes the answer, a partially-off surface
answers only for its visible part, a click is a route and not a pixel) and the quiet-tick
proof (`has_pending_damage` is true for a placement, ink, a cursor move; false after every
compose, for a no-op cursor move, and for a click or keystroke — routing owes no repaint).

### Boot gate

`vinput_suite` (10 invariants, in `vinput.rs`, running against the REAL devices on all three
targets — aarch64 and RISC-V over virtio-mmio, x86-64 over virtio-pci): the devices answer
for their identity by name from their own config space and reach DRIVER_OK with VERSION_1
negotiated; the event path is DMA-gated on both queues (an unregistered address is refused
as a descriptor, live regions are exactly ring+ring+events per device, grants carry the
driver's owner name); an armed device sends NOTHING uninvited over a bounded 10 000-poll —
the silence is MEASURED; a real keyboard record routes through the decoder into the focused
surface's queue with the modifier rules holding on this wire; a real pointer record moves
the cursor to the exactly mapped position; a click focuses the surface under the point and
an empty click clears focus; an unknown record is refused by name and counted; no keycode in
any modifier state emits a byte the console refuses; the axis mapping is exact at the edges
and clamps out-of-range samples; a keystroke composes nothing and a pointer move costs
exactly its glyph regions. Device bring-up failure exits 679; suite failure exits 680+i. A
machine with no input device attached says so and continues (the graceful-skip path
VirtualBox takes). `input_suite` grows 12 → 13 with the click-as-routing invariant.

### The live gate

`scripts/vinput-e2e.sh` boots x86-64 under OVMF with a virtio-gpu, a virtio-keyboard and a
virtio-tablet, COM1 a socket the harness attaches to BEFORE the guest runs (`-S`, then
`cont`) so the log holds every byte the machine printed and nothing anyone else wrote, and
drives the LIVE pump through QMP while typing the console over the serial wire: the boot's
input-hardware suite passed against the real devices; the desktop came up and reports its
session through `input` with the hardware wire SILENT (nothing posted, nothing queued); an
absolute pointer event moves the machine's cursor to the position the device's own axis range
implies (computed by the same formula, never hardcoded per point); a click focuses the surface
under the point, a click on empty space clears focus with the loser TOLD (exactly one `FocusLost`
waits in the window's queue), and clicks post no keystroke; one virtio
keystroke posts exactly ONE byte to the session, queued behind the focused window until its
owner drains it, and the console sees NOTHING in between — the hardware wire and the console
wire are distinct; and with nothing happening the counters hold still. Two wires on purpose:
QEMU routes injected key events to its ACTIVE keyboard handler, and the virtio keyboard
becomes that handler the moment the driver sets DRIVER_OK — a first version of this gate
typed `input` on the i8042 and watched every keystroke land in the window's queue instead,
which is the routing contract holding, not a bug. It runs as its own CI job (`vinput-e2e`)
and SKIPS loudly, never silently, when the host lacks QEMU/OVMF/python.
Every e2e script that builds the UEFI image now puts the rustup shim first on `PATH`
(`vinput-e2e.sh`, `keyboard-e2e.sh`) — a Homebrew cargo earlier on the path ignores
`rust-toolchain.toml` and fails cross-compilation with E0463, which is exactly how this
gate first failed on the workstation.

### Conformance and marker maps

Seven behaviors join the cross-CPU contract (158 → 165): the click as a routing decision,
and six of the hardware suite's — identity by name, the DMA-gated event path, armed silence,
the real keyboard record through the decoder, the real pointer record to the exact position,
and the whole-keycode-space alphabet proof. Marker maps changed deliberately: `input=13` and
`vinput=10` on the three QEMU gates; VirtualBox REQUIRES the 13-invariant routing marker and
lists the hardware marker SKIP — VirtualBox emulates no virtio-input device, and the gate
says so rather than pretending.

### The unsafe audit CHANGED

Unlike ADR-079, this rung adds unsafe sites, and `docs/UNSAFE-AUDIT.md` counts them:
kernel-core +14 (the config-space select/subsel reads, the `ConfigWrite` seam over both
transports, the `init` bring-up and `next_event` used-ring harvest, the suite's silence
poll), kernel +1 and kernel-riscv64 +1 (the virtio-mmio slot probe and transport
construction inside `input_pair`), kernel-x86_64 +12 (the PCI input-function probe, the
transport bring-up, the live desktop's single-writer static plus its device ops, and the two
PIT sites — `idt::restore_timer`, `pic::unmask_timer` — that keep the pump alive). Every
site carries its SAFETY argument; `scripts/check-boundary-docs.sh` holds the counts to the
tree.

### Named non-claims, in the register

The LIVE pump is x86-64's: aarch64 and RISC-V prove the driver, the decoder and the routing
against their real devices in the boot suite, but install no live desktop yet (no timer-tick
pump on those targets). Input is POLLED from the timer tick, not interrupt-driven. The PS/2
wire is still the console's alone — i8042 bytes do not enter the session. Relative pointing
devices (a mouse) are not decoded; only absolute axes are. Hotplug is not modeled: the
unplugged flag is read at bring-up. The statusq is armed and never used (no LED writes).
Right-button and other buttons are decoded and counted, not routed. A changed frame's
compose still allocates (the z-order clone) on a heap that never frees — bounded by events,
not by time, and named here rather than hidden. Device-level GPU
isolation between surfaces, alpha, and text rendering/IME inside surfaces stay open in the
row, as before.
