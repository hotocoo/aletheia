# ADR-037 — The bus is a seam, and a driver maps its own registers

**Status:** Accepted (2026-08-03)
**Context:** GAPS4 ALET-P2-019 · REQ-DRV-005 · completes ADR-036 (one driver, per-CPU seam) by adding a
per-BUS seam; extends REQ-MM-001 / ADR-029 with a device-memory admission rule

## Context

ADR-036 made virtio-blk one driver with a two-function CPU seam, and RISC-V got a real disk. x86-64 —
the *other* first-class target — still had none, and for a reason no CPU seam could fix: q35 has no
virtio-mmio window at all. Its virtio devices are **PCI functions**, and the registers the protocol
needs are not at fixed offsets from a documented base; they live inside BAR regions that a *capability
list* in configuration space points at.

Then the first attempt failed in a way worth recording: QEMU places the virtio BAR **above 4 GiB**
(`0xc000000000` in the boot gate), and the kernel's own address map (ALET-P1-031) deliberately covers
only sub-4 GiB MMIO. The driver's registers were simply not mapped. Two bad answers were available —
map all of physical space, or extend the boot map to some larger arbitrary bound — and both replace a
precise statement about what is mapped with a vague one.

## Decision

**1. The bus is a second seam, beside the CPU seam.** `kernel_core::virtioblk::Transport` names what a
bus must provide: feature halves, status, queue select/size/addresses/ready, notify, and device config.
`MmioTransport` (in `kernel-core`) implements it for virtio-mmio; `kernel-x86_64/src/pci.rs` implements
it for virtio-pci. The queue logic, the descriptor chains, the bounded poll and the `BlockDevice` impl
are untouched and shared — one protocol, two buses, three CPUs.

**2. `notify` may not disturb `queue_select`.** virtio-pci's notify address is
`notify_base + queue_notify_off * notify_off_multiplier`, and `queue_notify_off` is a register *of the
selected queue*. Reading it inside `notify` would mean touching `queue_select` with a request in
flight. So `Transport` has one hook — `after_queue_select`, a no-op by default — which the driver calls
exactly once after selecting its queue, and which the PCI transport uses to **latch** the offset. The
MMIO transport ignores it.

**3. PCI enumeration stays as small as the job.** Configuration space through the legacy ports
(`0xCF8`/`0xCFC`) — no ACPI/MCFG dependency, works on every x86 machine QEMU emulates. Bus 0 only.
Vendor `0x1AF4` with a block device id. The capability walk takes COMMON_CFG, NOTIFY_CFG and
DEVICE_CFG; a device missing any of the three is **refused**, never poked at a guessed offset. Exactly
two command bits are enabled: memory space and bus master. No MSI/MSI-X (completion is polled on every
target), no BAR assignment (the firmware programmed them), no bridge recursion.

**4. A driver maps its own registers — through an admission check whose physical rule is INVERTED.**
`vm::map_device_range` identity-maps a BAR region as RW+NX in the live tree, and every page goes
through `AddrPlan::validate_map_device`: all the ordinary VA rules (alignment, canonical form,
non-null, not the kernel image), and then the physical rule turned around. `validate_map` requires the
physical page to be **inside** the frame-allocator window; `validate_map_device` requires it to be
**outside**.

That inversion is the security content of this ADR. A driver legitimately reaches physical addresses
the allocator does not own — that is what a BAR is. What must never happen is the reverse: mapping RAM
as MMIO, which would give a frame some task owns a second mapping with different cacheability and side
effects, through a path the ownership model (ADR-030) never sees. Neither call can express the other's
mistake, and `MapFault::PhysIsRam` names the refusal. Proved as a property, not by example: the host
sweep in `kernel-core/tests/vmaddr.rs` walks the whole window plus a margin on all three plans and
asserts no page is ever mappable as both, that each rule matches its window exactly, and that the sweep
really produced both outcomes. Three x86-64 boot invariants (53–55) prove the live API refuses a RAM
range while the same page remains a legal RAM mapping — the rule is inverted, not merely stricter.

**5. The gate attaches a SECOND disk.** The boot medium stays untouched; the scratch disk arrives on the
virtio-pci bus, so the shared 17-invariant suite (driver → geometry → round-trip → journal → the whole
12-behavior filesystem namespace → capability gating) runs over a real device on x86-64 too.

## Consequences

* **All three targets now prove the filesystem on real storage.** aarch64 and RISC-V over virtio-mmio,
  x86-64 over virtio-pci: `ALL 17 VIRTIO-BLK INVARIANTS HOLD` on each, boot failing `120 + i` /
  `180 + i`. Crash atomicity is a hardware claim on every CPU Aletheia targets.
* **The boot map keeps its precise statement.** It covers what it always covered; anything beyond it is
  mapped deliberately, by the driver that needs it, through a check that refuses RAM.
* **A third bus is now cheap** — a transport impl, not a driver.
* **What this does NOT do.** Still no DMA isolation: the device receives raw physical addresses and
  nothing but our bookkeeping stands between it and the rest of RAM (ALET-P1-018; bus-master is
  *enabled* here, which makes that gap more concrete, not less). Still no interrupts, no multi-queue,
  no hotplug, no bridge recursion, and a BAR that does not fit an address is refused rather than
  handled. A 64-bit BAR is read as a pair but device pages are mapped 4 KiB at a time, so a very large
  BAR is a correspondingly large number of leaves — fine for the ~16 KiB virtio regions, not a general
  MMIO strategy.

## Alternatives considered

* **Map all of physical memory (or a huge fixed MMIO span) at boot.** Rejected: it trades a precise,
  auditable map — the thing ALET-P1-031 was about — for a vague one, and it would map RAM and MMIO with
  one attribute choice.
* **Force QEMU to place the BAR below 4 GiB.** Rejected as testing to the harness. Real machines put
  64-bit BARs high; a driver that only works when the firmware is generous is not a driver.
* **Let `validate_map` accept any physical address when a "device" flag is passed.** Rejected: a flag
  that relaxes a check is a check that can be relaxed by mistake. Two named functions cannot be
  confused, and the fault names differ (`PhysOutOfRange` vs `PhysIsRam`).
* **A general PCI bus manager (enumeration, resource assignment, driver binding).** Rejected as
  speculative: one bus, one driver, one device kind. It earns its ADR when a second PCI device exists.
