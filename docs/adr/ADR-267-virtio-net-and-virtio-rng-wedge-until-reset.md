# ADR-267: virtio-net and virtio-rng wedge until reset

**Status:** Accepted (2026-11-19)
**Requirements:** REQ-DRV-021 (new)
**Builds on:** ADR-266 (NVMe and AHCI), the virtio-blk wedge (ADR-263's audit), ADR-150 and its
2026-11-19 addendum (virtqueue waits bounded in time), ADR-041 (the shared virtqueue), ADR-153
(virtio-rng).

## Context

virtio-net transmits from one buffer through descriptor slot 0 of its transmit queue, and virtio-rng
asks for bytes into one frame through slot 0 of its request queue. When the device did not hand the
buffer back inside the budget, both drivers returned a timeout and went on. The device still owned
the buffer: the next send rewrote a frame the device might be reading, and the next draw handed out a
frame it might still write. Worse, the late used entry stays in the ring, so the NEXT request's wait
harvests it as its own completion and returns while its own request is still pending, off by one for
the rest of the boot.

## Decision

* **Wedged until reset.** `VirtioNet::send` sets `wedged` when its transmit wait runs out;
  `VirtioRng::request` does the same. A wedged device offers the device nothing more: every send is
  refused with `NetError::Wedged`, every draw with a named `EntropyRefusal::Device`, before a
  descriptor is published (the avail index does not move).
* **`Virtqueue::reset`** re-publishes a queue on its SAME frame after a device reset: the descriptor
  table and both rings zeroed, `last_used` restarted at zero (the device's used index restarted with
  it, so a stale `last_used` would read every new completion as old), the three addresses told to the
  device again. Registered buffers stay registered.
* **`VirtioNet::reset` / `VirtioRng::reset`**: status 0 (which ends everything the device held), the
  same feature negotiation `init` uses (extracted as each driver's `negotiate`), the queues
  re-published, virtio-net's receive buffers re-posted, DRIVER_OK. The MAC, the DHCP lease and the
  ARP cache belong to the machine, not the device, and are kept. Nothing is allocated.
* **Scope by ownership.** virtio-net's receive path (`recv_until`) re-posts every buffer it harvests
  and leaves nothing with the device, so it does not wedge and keeps its spin bound.
* **A boot invariant each, deterministic.** The suite sets the wedge directly (provoking a lost
  completion would race the device, as ADR-266 learned on VirtualBox), requires the refusal with
  the avail index unchanged, resets, and requires the device to work: an ICMP echo answered with its
  own payload after the reset (`net=10`), and two fresh, different draws (`entropy=7`).

## Evidence

* `[net] ALL 10 NETWORK INVARIANTS HOLD` and `[entropy] ALL 7 ENTROPY INVARIANTS HOLD` on aarch64
  (virtio-mmio), riscv64 (virtio-mmio) and x86-64 (virtio-pci): after the reset the gateway answers
  an ICMP echo with its own payload, and two draws are fresh and different. Each new invariant
  compares against the refusal itself (`Err(NetError::Wedged)`, an `Err` from `fill`) and an
  unchanged avail index, so a driver without the check fails it.
* `kernel-core/tests/virtio_wedge.rs` (added after the wave): a simulated virtio-rng device on a fake
  transport that can answer after the budget. A late answer wedges the driver, lands in the used ring,
  and is NOT harvested by the next draw (nothing more is published); after `reset` the device
  answers from used index 0 and draws are fresh. Mutants: removing the wedge check, or the
  `last_used` restart in `Virtqueue::reset`, each fail it.
* Gates on this tree: `vm-e2e.sh`, `vm-e2e-riscv.sh`, `vm-e2e-x86.sh` (three boots), `conformance.sh`
  (422 behaviors), `console-e2e.sh`, `tcp-e2e.sh`, `dns-e2e.sh`, `tls-e2e.sh`, `https-e2e.sh`,
  `net-fuzz-e2e.sh` (the hostile peer, which exercises every send and draw path the console has),
  all PASS with no SKIP; quality gate; every `check-*.sh`.

## What this is not

* **virtio-gpu, HD Audio and xHCI still return a timeout and go on.** The GPU's reset also drops
  every resource and the scanout, so it needs the desktop to rebuild them; HD Audio's CORB/RIRB and
  xHCI's command and event rings carry their own indices. Next rungs, named.
* **The console resets them, nothing else does.** Each target's `netstatic` resets a wedged NIC
  (`recover_net`, before every NIC command) or entropy source (`rng`, before every draw) and prints
  that it did, so one slow transmit or draw costs one failed command, not the device until reboot.
  No live gate wedges them on purpose: the recovery path is the reset the boot invariants prove.
* **One request in flight per queue**, as before.
