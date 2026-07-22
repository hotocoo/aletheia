# ADR-023: Device and driver architecture

**Status:** Proposed (deferred — phased plan, no blind code) · **Date:** 2026-07-22

## Context

A complete OS needs a real device model + driver architecture for storage, networking, graphics,
input, and accelerators (gap-register Issue 5). Aletheia's constraint is that drivers, like
everything, get **no ambient authority** — a driver operates only within its assigned capabilities.

## Decision

Drivers are **user-space services** reached through capability IPC, not privileged kernel blobs:

```text
Application → Capability IPC (ADR-020) → Device Service → Driver → Hardware
```

*Delivered (REQ-DRV-002, 2026-07-22, `kernel-core/src/device.rs`).* The capability-authorization core:
`DeviceGuard` wraps any `BlockDevice` and gates every read/write/flush on the same `CapEngine` — no
ambient device authority. Proved over the real `MemBlockDevice` (`kernel-core/tests/device.rs`): no
capability ⇒ no I/O; a read-only capability reads but cannot write (attenuation); a write capability's
bytes land. Hardware discovery + the concrete virtio-blk driver (implementing this same `BlockDevice`
trait) remain the deferred slices below.

**Phase 1 — discovery + device capabilities.** A device manager enumerates hardware via ACPI
(x86-64) / device-tree (aarch64, RISC-V), abstracted behind the `Hal`. Each device becomes an entity
with a capability; a driver is granted a `device.<class>` capability scoped to exactly its device
(MMIO region + IRQ line). No driver can touch a device it was not granted.

**Phase 2 — bus + class abstractions.** PCIe/USB enumeration; a `BlockDevice` trait (the first
persistent-storage path, feeding ADR-024) and a `NetDevice` trait; input and audio device classes.
DMA is IOMMU-constrained so a driver cannot DMA outside its granted buffers (defends the DMA-attack
class in gap-register Issue 11).

**Phase 3 — isolation + recovery.** Drivers run isolated (a driver crash cannot corrupt the kernel or
other drivers); the supervisor (ADR-026) restarts a failed driver; hotplug + power management.

## Virtio-blk driver — implementation plan (the named next slice; REQ-DRV-001, VM-gated)

The concrete first real driver: a **virtio-blk** device over **virtio-mmio** on QEMU `virt`
(aarch64/RISC-V), implementing the delivered `kernel_core::storage::BlockDevice` trait so the journaled
store (REQ-STOR-002) runs over real emulated hardware. This is a fresh-context brick (intricate MMIO +
ring code); the plan below is exact so it can be executed and VM-gated in one focused pass.

**Environment wiring.** Add to the aarch64 runner (`kernel/.cargo/config.toml`) and `scripts/vm-e2e.sh`
(and the RISC-V equivalents): generate a small raw backing image (e.g. `truncate -s 1M`), seed sector 0
with a known pattern, and attach `-drive if=none,format=raw,file=<img>,id=blk0 -device
virtio-blk-device,drive=blk0`. On `virt`, virtio-mmio transports sit at `0x0a00_0000 + i*0x200` (32
slots) inside the Device-mapped peripheral GiB — already mapped by `vm::build_identity`. The probe must
be graceful: under bare `cargo run` (no disk) it logs `[virtio] no device (skipped)` and boots green;
the VM gate attaches the disk and asserts discovery + I/O.

**Driver protocol (modern/v2 split virtqueue).**
1. *Discovery:* scan the 32 mmio slots for `MagicValue==0x7472_6976` ("virt") and `DeviceID==2` (block).
2. *Init handshake:* write status `ACKNOWLEDGE|DRIVER`; read `DeviceFeatures` (accept the minimal set,
   clear `VIRTIO_F_*` we don't implement), write `DriverFeatures`, set `FEATURES_OK`, read it back.
3. *Queue 0 setup:* allocate frame-backed, aligned descriptor table (16 B × N) + avail ring + used
   ring; program `QueueSel=0`, `QueueNum`, and the `QueueDesc/Avail/Used` low/high registers; set
   `QueueReady=1`. Read capacity from config space (`sectors = read64(config+0)`).
4. *Request:* a 3-descriptor chain — header (RO: `type` READ=0/WRITE=1, reserved, `sector`), a
   `BLOCK_SIZE` data buffer (device-writable for READ / driver-readable for WRITE), and a 1-byte status
   (device-writable). Post the head index to the avail ring, `QueueNotify=0`, poll the used ring for
   completion, check `status==0` (OK). `flush` uses a `VIRTIO_BLK_T_FLUSH` request.
5. *BlockDevice impl:* `read_block`/`write_block` issue one request each; `num_blocks` from capacity.

**Capability gating (reuses REQ-DRV-002).** The driver's device object is wrapped in a `DeviceGuard`
so every block op is authorized by the `CapEngine` — the driver holds only its `device.blk` capability,
no ambient authority.

**VM-gated invariants (planned):** device discovered; capacity read matches the attached image; write
sector then read-back returns the written bytes across a real virtqueue round-trip; and — end to end —
`Journal::commit` runs a transaction over the virtio-blk `BlockDevice` and a subsequent `recover`
reproduces it (crash-consistency over real emulated storage). Contract-honest (ADR-010): a wrong ring
layout faults/hangs into the VM watchdog, never a silent pass.

## Consequences

- Drivers require no ambient authority beyond their assigned capabilities — a compromised driver is
  bounded to its device.
- At least one persistent-storage path (`BlockDevice`) becomes available for ADR-024.
- Entirely hardware-bound; stays `deferred` (`REQ-DRV-001`) until a device service + one real driver
  are brought up and (where emulable) VM-gated. Driver failure is contained + diagnosable by design.
