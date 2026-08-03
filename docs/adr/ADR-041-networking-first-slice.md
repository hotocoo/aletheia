# ADR-041 — Networking: a second device kind, and something that answers back

**Status:** Accepted (2026-08-03)
**Context:** GAPS4 ALET-P2-020 · REQ-NET-001 (virtio-net + multi-queue substrate) / REQ-NET-002 (ARP + ICMP)
· builds on ADR-036 (one driver per bus, CPU seam) and ADR-037 (bus seam)

## Context

Networking was the largest remaining "an operating system does this" hole: architecture text and nothing
else. It could not simply reuse the block driver, for a reason worth naming precisely — a block device has
**one** queue with one request in flight, polled to completion, and `virtioblk` encodes exactly that in a
fixed layout inside a single frame. A network device cannot work that way:

* it needs **two** queues (receive and transmit), and
* the receive queue must have buffers **posted before the device is allowed to run**, because a packet
  arrives whether or not the driver is ready and a queue with no buffer simply drops it.

## Decision

**1. The ring mechanics become reusable — `kernel_core::virtq`.** A `Virtqueue` parameterized by queue
index, over the existing `VirtioHal` (CPU) and `Transport` (bus) seams: one frame per queue, `add` /
`kick` / `poll_used`. `virtioblk` keeps its own single-queue path untouched — this is a second, general
implementation for devices that need more, not a rewrite of a working driver.

**2. `last_used` lives in the queue, not in each caller.** The used ring's index only increases; a driver
that forgets how far it has consumed re-reads a completion — on a receive queue that means handling one
packet twice, on a transmit queue reusing a buffer still in flight. That state belongs with the ring.

**3. A second device KIND is two more ids, not a second framework.** `probe_nth_kind` (mmio) and
`find_virtio_nth` (pci) take the device kind; `MmioTransport::new_for` checks it, so no driver body
re-checks what it is talking to.

**4. `identity()` reports the VIRTIO device kind on every bus.** This is where the first attempt failed:
the PCI transport returned the *PCI* device id (`0x1041`), so the network driver's "am I talking to a
network device?" check refused a perfectly good NIC. A seam whose meaning differs per bus is not a seam.
Modern virtio-pci ids are `0x1040 + kind`, plus a short transitional list.

**5. Receive buffers are posted before DRIVER_OK, and re-posted before the frame is returned.** Both
orderings are load-bearing: the first stops the device dropping a frame that arrives during setup; the
second stops the queue running dry after `RX_BUFFERS` frames.

**6. The proof is that something ANSWERS.** A transmit-only driver is indistinguishable from a frame that
vanished, so the suite talks to QEMU's user-mode gateway (`10.0.2.2`):

* **ARP** — broadcast "who has 10.0.2.2?", and require a *reply carrying that address and a MAC*. This is
  the receive path's first real proof.
* **ICMP echo** — then a real IPv4 packet with two correct checksums (the header's and the message's) must
  come back as an echo reply with the same identifier, sequence and payload. A wrong checksum is dropped by
  the peer in silence, so the reply arriving proves the packet was well **formed**, not merely intended.
* **A second echo** must be matched on *its own* sequence, proving the driver reads the reply rather than
  assuming the next frame is the answer.

Four behaviors, on all three targets, in the `conformance.sh` core contract (64 → **68**): two targets run
virtio-mmio and one runs virtio-pci, so this also demonstrates the bus seam carries a second device kind.

**7. Frames that are not the answer are COUNTED, not queued.** `dropped()` exists so that a failing wait
beside a nonzero count distinguishes "the peer said nothing" from "the driver threw the answer away".

## Consequences

* Aletheia can reach the network: it resolves an address and completes an ICMP round trip on aarch64,
  RISC-V and x86-64, `ALL 4 NETWORK INVARIANTS HOLD`, boot failing `220 + i`.
* The multi-queue substrate is now available for any further device (a console, an entropy source).
* **Not claimed, and it is a lot:** no TCP, no UDP, no DHCP, no routing, no fragmentation, no ARP cache, no
  socket layer — the guest address is the fixed `10.0.2.15` QEMU expects and every reply is matched
  synchronously by the code that sent the request. There is no receive path that hands frames to anything
  else, which is the next slice rather than a hidden one. Completion is polled (no interrupts anywhere in
  this kernel), one frame is transmitted at a time, and no offload or checksum-offload feature is
  negotiated. ALET-P2-020 therefore stays **open** as "networking stack", with this slice named in its row.

## Alternatives considered

* **Extend `virtioblk` to N queues.** Rejected: its layout and its "one request, polled" model are correct
  for a block device, and generalizing them in place would put a network device's needs inside the storage
  path. A separate, general queue type leaves the proven driver alone.
* **Write a TCP stack now.** Rejected: without a socket layer, a timer-driven retransmit path and a receive
  demultiplexer, a "TCP" would be a demo of a handshake. ARP + ICMP is small enough to be complete and real.
* **Assert the driver transmitted, without a peer.** Rejected — that is the proof that proves nothing.
* **Use a fixed, invented MAC.** Rejected: the address comes from the device's config space (the one
  feature negotiated besides VERSION_1), so the suite can assert it is a real unicast address.
