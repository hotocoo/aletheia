# ADR-085 — One desktop, three CPUs

* Status: accepted
* Date: 2026-09-03
* Register: ALET-P2-021 (graphics / compositor), REQ-GFX-008
* Extends ADR-080 (real devices, the live desktop), ADR-083 (the terminal window),
  ADR-084 (windows are a managed set)

## Context

Every contract under the desktop was arch-neutral and proved on all three CPUs: composition
(ADR-077/078), the input session (ADR-079), the virtio-input driver (ADR-080), the text grid
(ADR-083), the window manager (ADR-084). The thing that RAN them was not. `kernel-x86_64`'s
`desktop.rs` held the compositor, the session, the manager, the two windows, the pump and the
console's second surface; aarch64 and RISC-V proved every invariant and then showed a black
screen. "Aletheia has a GUI" was true of one target and a claim about the other two.

A second copy of that module per target would have been the wrong answer twice: three
implementations to keep honest, and the difference between CPUs would live in code nobody
compares.

## Decision

`kernel-core/src/desktop.rs` — the desktop as a value, generic over the SAME seams the drivers
already cross: `Desktop<H: VirtioHal, T: Transport + ConfigWrite>`. It owns the compositor, the
input session, the window manager, both windows' grids, the GPU resource and the pump. Each
kernel keeps only what a CPU can answer for:

* **Ownership and the concurrency posture.** The static and the door into it stay per target:
  x86-64 masks IF (`without_interrupts`), aarch64 masks `DAIF.I`, RISC-V clears `sstatus.SIE`.
  The shared desktop is a plain value with `&mut self` methods — it takes no lock and knows no
  interrupt flag, so it cannot impose one platform's rule on another.
* **The wake-up.** A pump that never runs is a dead desktop, and WHEN it runs is a timer
  question each CPU answers differently: the PIT on x86-64 (IRQ0), the generic timer PPI
  (INTID 30, armed from `CNTFRQ_EL0/100`) on aarch64, and the S-mode timer through SBI
  `set_timer` on RISC-V. All three tick at 100 Hz, so the desktops behave alike.
* **The frame allocator's reading.** `pump(free, total)` is handed the memory ledger rather than
  calling an allocator: the machine's own numbers are the platform's to report.

The DT targets also gain the console's SECOND SURFACE: `emit` writes to the UART and to the
terminal window, and `getc` reads the UART ring and then the window's queue — one shell, two
surfaces, exactly as ADR-083 defined it on x86-64. `input` answers on all three.

## Consequences

* All three gates now assert `[desktop] LIVE: ... 2 managed windows`; the aarch64 and RISC-V
  boots compose their first frame through their own virtio-gpu and hand it to the device.
* On aarch64 the desktop is installed BEFORE the SMMUv3 gate, so its backing frames sit inside
  the stage-2 identity domain enforcement covers — the ADR-073/074 ordering, unchanged.
* The IRQ paths of both DT targets were fatal-by-default for every source but one. Each gained
  exactly one more named source, gated to interactive builds, rearmed BEFORE the pump runs so a
  slow pump cannot silently stop the clock; everything else stays fatal.
* `kernel-x86_64/src/desktop.rs` shrank from 627 lines to a ~120-line owner: the statics, the
  IRQ0 pump, the frames, and the door. Its atomics ledger went with it — the facts are read from
  the model itself through the same door the console writes through.

## Named non-claims

* **The DT desktops pump only on interactive builds.** A non-interactive gate installs the
  desktop, composes the first frame and stands still: nothing is claimed about motion nobody is
  there to see. `scripts/console-e2e.sh` boots the interactive kernels WITH graphics and input
  devices and asks `input` mid-session, so the pump is proved to run on those CPUs — the readout
  comes from the model the pump is mutating, and an IRQ path that took a wrong branch would have
  exited 102 instead of answering.
* **Pointer motion and clicks are proved LIVE on x86-64 only.** `scripts/vinput-e2e.sh` injects
  real device events through QMP on that target; on aarch64 and RISC-V the driver, decoder,
  routing and window manager are proved in the boot suites and the pump is proved to tick, but no
  injected event crosses it.
* Everything ADR-084 named as missing (resize, minimise/maximise, window lists, keyboard focus
  cycling, user-mode applications owning windows) stays missing here.
