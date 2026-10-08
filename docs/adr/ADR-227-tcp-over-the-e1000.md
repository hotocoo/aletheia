# ADR-227 — TCP over the e1000

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-DRV-010 (widened)
**Builds on:** ADR-138..140 (the TCP pump and `Ipv4Link`), ADR-224 (e1000), ADR-226.

## Context

ADR-224 left the TCP/IP stack running over virtio-net only. The stack already reaches a device
through one seam, `tcpnet::Ipv4Link` (send a datagram, wait for one, our address), so a second
NIC needs that seam implemented, not a new stack.

## Decision

* **`e1000::E1000Link`** implements `Ipv4Link` for one on-link peer: an Ethernet header to the
  peer's MAC on send; on receive, IPv4 frames for our address and the asked protocol, trimmed to
  the datagram's declared length (Ethernet padding is not data). `E1000::recv_until` takes a poll
  bound as well as the time budget, the same shape as virtio-net's.
* **An echo peer on the gate's wire.** The x86-64 and aarch64 gates add
  `guestfwd=tcp:10.0.2.100:7-cmd:cat` to the e1000's user-mode network: QEMU answers ARP for
  10.0.2.100 and pipes each TCP connection to `cat`, so the bytes come back. The e1000 suite
  runs one `tcpnet::exchange` to it when the peer answers ARP, making the family 7 invariants on
  those gates. VirtualBox's NAT has no such peer, so it stays 6 there. Each gate pins its count.
* **A dropped e1000 stops.** `Drop` clears `RCTL`/`TCTL` and masks interrupts. Found live: the
  echo peer's last segment arrived after the suite, the receive ring was still enabled, and the
  e1000 did DMA after VT-d enforcement turned on. No window was granted for it, so the unit
  faulted (`FRCD sid=0x0028 reason=2`, invariant 11).

## Proof

* Both QEMU gates: `[pass 7] e1000: a TCP conversation over the NIC reaches the echo peer and its
  bytes come back`, `e1000=7`. The x86-64 VT-d suite passes after it.
* Hosted tests: unchanged suite of 6 (no echo peer in the simulator).

## What this does not claim

* The console's `fetch`, TLS and DNS paths still use virtio-net. Pointing them at whichever NIC
  is present is a separate change.
