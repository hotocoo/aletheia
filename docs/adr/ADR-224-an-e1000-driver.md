# ADR-224 — An e1000 driver, and two drivers checked on a second hypervisor

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-DRV-010 (new)
**Builds on:** ADR-223 (NVMe), ADR-041 (virtio-net), ADR-043 (DMA registry).

## Context

The only network driver was virtio-net, which no physical machine has. The Intel 8254x family
(e1000) is the NIC that QEMU (`-device e1000`, 82540EM), VirtualBox (default 82540EM) and VMware
(82545EM) all emulate, and it is the base of the register model later Intel NICs keep. The
VirtualBox gate also ran with no NIC and no disk the kernel could drive, so every device family
there was proved only against QEMU's models.

## Decision

* **`kernel_core::e1000`**: reset (`CTRL.RST`, wait for it to clear), interrupts masked
  (`IMC`), link forced up (`CTRL.SLU | ASDE`), MAC from `RAL0/RAH0` when `RAH.AV` is set or from
  EEPROM words 0..2 through `EERD`. One RX ring and one TX ring of 8 legacy 16-byte descriptors
  (128 bytes, the `RDLEN/TDLEN` minimum), 2048-byte receive buffers, CRC stripped, broadcast
  accepted. Polled: transmit waits for Descriptor Done, receive consumes descriptors with
  `DD | EOP` and no error bits and hands each back by moving `RDT`. All seven frames pass the DMA
  registry. Offsets and bits are from the 8254x Software Developer's Manual and were checked
  against QEMU's `hw/net/e1000_regs.h`.
* Bound by PCI ids 8086:100E and 8086:100F only: other 8254x/8257x parts differ in ways this
  driver does not handle, so they are not claimed.
* **x86-64 only in this ADR.** The 82540EM's BAR1 is an I/O-port BAR and the aarch64 BAR
  assigner refuses I/O BARs; aarch64 e1000 waits for a BAR0-only assignment path.
* **The VirtualBox gate now attaches an 82540EM on NAT and an NVMe controller** with a 1 MiB
  namespace, and requires `ALL 23 NVME INVARIANTS HOLD` and `ALL 6 E1000 INVARIANTS HOLD`.

## Proof

* `kernel-core/tests/e1000.rs`: a simulated 8254x with an ARP-answering gateway that first
  sends a stray frame. The suite holds; the MAC comes from the EEPROM when RA is not loaded; a
  reset that never clears is refused; errored frames are never handed up; a transmit that never
  completes is a timeout; no link fails invariant 2. Mutation check: not returning RX
  descriptors makes the wrap test fail.
* `kernel-x86_64/scripts/smoke-test.sh` attaches `-device e1000` on its own user-mode network and
  requires `e1000=6`: unicast MAC after reset, link up, DMA gate, MTU limit, ARP answered by the
  gateway, both rings wrapping over three ring lengths. Measured on QEMU 11.1 TCG.
* `scripts/vm-e2e-vbox.sh` runs both drivers against VirtualBox's own controllers (CI).

## What this does not claim

* Not proved on silicon. Throughput is not measured; one frame is in flight at a time.
* The TCP/IP stack still runs over virtio-net only. Moving it onto a frame-level seam both NICs
  implement is the next network step.
