# ADR-254 — The e1000 driver drives an 82574L (e1000e)

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-DRV-013 (new)
**Builds on:** ADR-224 (e1000 driver), ADR-226 (drivers on aarch64), ADR-227 (TCP over the
e1000), ADR-228 (drivers safe outside a gate), ADR-223 (queued drivers: e1000e/igc).

## Context

ADR-223 queued e1000e/igc for wired networking on real machines. The Intel 82574L is the
PCIe successor of the 8254x the e1000 driver speaks to, sits on many desktop and server boards, and
is what QEMU models as `e1000e`. In the legacy-descriptor mode the driver uses (RFCTL.EXSTEN
clear, legacy RX and TX descriptors) the 82574L keeps the 8254x register map for every register
the driver touches: CTRL, STATUS, IMC, RCTL, TCTL, TIPG, the RX/TX ring registers, MTA and RA[0].

## Decision

* **Bind the 82574L (device id 0x10D3) with the unchanged driver.** `e1000::DEVICE_IDS_82574` is
  a separate list, so the console's network device (ADR-235) still binds only an 8254x and the
  82574L gets its own suite and family.
* **Its MAC comes from RA[0].** The 82574L loads RA[0] from its NVM with Address Valid set at
  reset, so the driver's first source answers. Its EERD layout differs from the 8254x's (DONE in
  bit 1, address from bit 2); the driver's EEPROM fallback speaks the 8254x layout, so on an
  82574L without a valid RA[0] it times out and init is refused rather than reading a wrong
  address.
* **Boot**: `e1000e_selftest` on x86-64 and aarch64 finds the first 82574L, runs the same
  `e1000::device_suite` (MAC, DMA gate, ARP for the gateway, ICMP echo, DHCP, TCP echo through the
  gate's `guestfwd`) and drops the device, which turns RX and TX off. On aarch64 its BARs are
  assigned 80 MiB above the virtio-blk-pci window, clear of NVMe, e1000, AHCI and xHCI.

## Evidence

* New family `e1000e` = 6 on aarch64 (00:06.0, mac 52:54:00:12:34:58, 204 ms) and x86-64
  (00:07.0, mac 52:54:00:12:34:57, 42 ms); both gates attach `-device e1000e` on its own user
  network with the same TCP echo forward. `e1000` = 6 unchanged; VT-d 14/14 after the dropped
  82574L.

## What this is not

* Not igc (I225/I226): a different register model, still queued.
* No 82574 features: no extended or packet-split descriptors, no MSI-X, no multiple queues, no
  checksum offload. Polled, eight descriptors per ring, as ADR-224.
* Not the console's network device; that stays the 8254x (or virtio-net).
