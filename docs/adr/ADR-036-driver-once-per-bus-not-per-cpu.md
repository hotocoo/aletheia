# ADR-036 — A driver belongs to its bus, not to a CPU: virtio-blk defined once

**Status:** Accepted (2026-08-03)
**Context:** GAPS4 ALET-P2-019 · REQ-DRV-004 · generalizes REQ-DRV-003 (ADR-023); the mechanism that
lets REQ-FS-001 (ADR-035) be proved on real storage by more than one target

## Context

The first real driver (REQ-DRV-003) was written inside the **aarch64** crate, which ADR-019 designates
the *bootstrap/dev* backend. The two **first-class** targets — AMD64 and RISC-V — had no block device
at all. So the situation was exactly inverted: every claim that touched real storage (the journal's
crash consistency, and after ADR-035 the filesystem namespace) was proved against hardware only on the
target that matters least, while the targets a user would run proved them against a RAM model.

The obvious fix — copy the driver into the RISC-V crate and change one base address — is the failure
mode gap-register Issue 1 exists to prevent. A split virtqueue, a feature handshake, a descriptor
chain and a request header are facts about **virtio**, not about an instruction set. Two copies means
two places for a ring-layout bug, and a *silent* divergence in a security-relevant path (what the
device is told it may write into).

## Decision

**The driver lives once, in `kernel-core`, behind a seam that contains only what is genuinely
per-target.**

1. **`kernel_core::virtioblk` owns the protocol.** Reset, negotiation (accept `VIRTIO_F_VERSION_1`,
   plus `VIRTIO_BLK_F_FLUSH` when offered, clear everything else), queue setup, descriptor chains, the
   bounded completion poll, and the `BlockDevice` impl.

2. **`VirtioHal` is the whole seam — two functions.** `alloc_frame()` (a zeroed, *identity-mapped*
   frame, so the address the driver writes through is the address the device DMAs to) and `barrier()`
   (`dsb sy` on aarch64, `fence iorw, iorw` on RISC-V). A target additionally supplies an `MmioLayout`
   — where its platform puts the transports (aarch64 QEMU `virt`: 32 slots 0x200 apart at
   `0x0a00_0000`; RISC-V QEMU `virt`: 8 slots 0x1000 apart at `0x1000_1000`). That is the complete list
   of what differs.

3. **`init` returns facts, it does not log them.** An `InitReport` carries the version, device id,
   feature halves, queue size and capacity back to the caller, which prints them with its *own*
   console macro. A shared driver cannot call a per-target `kprintln!`, and inventing a logging trait
   for four lines would be worse than returning the four values.

4. **The invariant suite is shared too — and it ends in the filesystem.** `device_suite` proves
   discovery, the attached geometry, a write→read-back round-trip, a journal commit plus recovery from
   device bytes alone, **the entire twelve-behavior filesystem namespace over that device**, and
   capability-gated I/O through `DeviceGuard`. Seventeen invariants, identical on every target that has
   a disk — so "a create is atomic across a crash" is now proved through a real virtqueue on a
   first-class target, not only on the dev backend.

5. **Geometry is asserted, not assumed.** The suite takes the block count the gate attached and refuses
   to proceed if the device disagrees. A wrong sector↔block mapping is caught before any byte is
   trusted, instead of surfacing later as corrupt data.

6. **Absence stays graceful.** With no `-drive`, `probe` finds no block transport, the target logs
   `[virtio] no device (skipped)` and boots green; the VM gate attaches a disk and *requires* the
   marker. A skip is therefore never available to CI — only to a developer running bare `cargo run`.

## Consequences

* **RISC-V, a first-class target, now has real storage** (REQ-DRV-004) and proves the journal and the
  filesystem over it: `ALL 17 VIRTIO-BLK INVARIANTS HOLD`, boot fails `180 + i`.
* **A ring-layout bug has one home.** Fixing it fixes every target; it cannot be fixed on one and
  linger on another.
* **The aarch64 crate shrank to a backend** (layout + HAL + report printing), and its invariant count is
  unchanged at 17 — the same behaviors, from shared code.
* **What this does NOT do — the driver *model* is still incomplete (ALET-P2-019 stays deferred).** No
  hotplug, no interrupt-driven completion (the poll is synchronous), one request in flight, no
  multi-queue, no device restart/recovery, and **no DMA isolation** — the device is handed raw physical
  addresses with nothing but our own bookkeeping between it and the rest of RAM (that is ALET-P1-018,
  and an IOMMU/SMMU is the real answer). x86-64 still has no block device because its transport is
  virtio-**pci**, which needs PCI enumeration rather than an MMIO window — a real difference in the
  bus, tracked as its own slice, not papered over here.

## Alternatives considered

* **Copy the driver per target.** Rejected: two implementations of one bus protocol, and the divergence
  would be silent until it corrupted data.
* **Keep the driver in the aarch64 crate and have RISC-V depend on that crate.** Rejected: it makes a
  first-class target depend on the dev backend, inverting ADR-019's own hierarchy.
* **A full device-manager abstraction (bus enumeration, driver registry, hotplug) up front.** Rejected
  as speculative generality: there is exactly one driver and one bus today. The seam introduced here is
  the smallest one that makes the second target real; the registry earns its ADR when a third device
  exists.
* **A logging trait in the seam so the shared driver can print.** Rejected — `InitReport` gives the
  caller the same information with no trait, no formatting machinery in `no_std`, and no per-target
  behavior hidden inside shared code.
