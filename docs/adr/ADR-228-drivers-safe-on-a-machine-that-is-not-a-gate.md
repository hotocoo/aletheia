# ADR-228 — Drivers that are safe on a machine that is not a gate

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-DRV-009, REQ-DRV-010, REQ-DRV-011 (tightened)
**Amends:** ADR-223 (NVMe), ADR-224/227 (e1000), ADR-225 (AHCI).

## Context

Virtio devices exist only inside a VM, so a boot suite that wrote to them or demanded a
particular network could not hurt anyone. NVMe, e1000 and AHCI exist on real machines and in
the VMware package. Three defects followed from reusing the virtio pattern:

1. **The NVMe suite wrote to and reformatted whatever namespace 1 it found.** On a machine with
   an NVMe SSD, booting Aletheia would have destroyed that disk. Nothing had been released with
   it, and no physical machine had booted it.
2. **Suites failed the boot when the gate's environment was absent.** AHCI required two disks
   and a scratch serial. Every x86-64 CI job that boots one disk failed at `ahci` invariant 1 on
   the ADR-225 push, and so would the VMware package (one `sata0` disk). e1000 required
   10.0.2.2 to answer ARP and the link to be up.
3. **A driver nobody held could still do DMA** (the e1000 fault of ADR-227). NVMe stayed enabled
   and AHCI ports kept `ST|FRE` on after their suites.

## Decision

* **Writes need a mark, on every storage driver.** NVMe now writes, flushes and runs its
  write group only on a controller whose Identify Controller serial is `ALETHEIA-SCRATCH`, the
  same rule AHCI has. `write_block` and `flush` on any other controller return an error before
  a command is built.
* **Each suite is a local group plus a gate group.** The local group holds on any machine with
  the device: NVMe 5 read-only checks (enable and identify, DMA gate, unregistered PRP refused,
  out of range refused, queue wrap over reads), AHCI 5 (a disk identified, first sectors read,
  DMA gates, out of range, non-scratch writes refused), e1000 3 (MAC, DMA gate, MTU). The gate
  group runs only when the gate's environment is there: scratch serial for storage, the
  10.0.2.2 gateway answering ARP for e1000 (then link, ARP, ring wrap, and TCP when the echo
  peer answers). Missing environment means a shorter suite, never a failed boot.
* **Gates stay strict.** Each gate pins the full count in its marker map (`nvme=23`, `ahci=24`,
  `e1000=7` on QEMU; `ahci=24`, `e1000=6` on VirtualBox), so a gate whose scratch disk or peer
  went missing still fails.
* **Drop stops the device.** `Nvme` clears `CC.EN`, `AhciDisk` stops `ST` then `FRE`, and the
  e1000 stops RX and TX (ADR-227).

## Proof

* Hosted: a non-scratch NVMe controller gets 5 invariants, no write or flush reaches it, its
  disk stays all zero, and it is left disabled. A one-disk AHCI machine gets 5 with zero writes
  and a stopped port. An e1000 network without the gateway gets 3.
* `scripts/keyboard-e2e.sh` (one boot disk, no NVMe or e1000): `[ahci] ALL 5 AHCI INVARIANTS
  HOLD`, `KEYBOARD-E2E: PASS`. Both QEMU gates: `nvme=23`, `ahci=24`, `e1000=7`, PASS.

## What this does not claim

* The e1000 wire group still sends one ARP request claiming 10.0.2.15 on whatever network it is
  plugged into. On a real LAN that request goes unanswered and the group is skipped. A DHCP
  lease is the real fix.
