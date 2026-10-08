# ADR-234 — The machine takes a DHCP lease

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-NET-003 (tightened)
**Amends:** ADR-060 (DISCOVER/OFFER as cross-evidence only).

## Context

The virtio-net driver spoke from a constant, `10.0.2.15`, with gateway `10.0.2.2`: QEMU's
user-network defaults. ADR-060 added a DHCP DISCOVER and checked that the network's OFFER matched
the constant, but the lease was never taken. On any other network (a VMware NAT, a bridged LAN,
QEMU with another `net=`), the machine would have ARPed for a gateway that does not exist and sent
packets from an address nobody routes to. The console's `resolve` also defaulted to QEMU's name
server, `10.0.2.3`.

## Decision

* `dhcp.rs` gains `write_request` (broadcast; option 50 names the offered address, option 54 the
  server that offered it, option 55 asks for mask, router, name server and lease time) and
  `parse_ack` (an ACK bound to the transaction id; a NAK is `DhcpError::Nak`). The option walk now
  also reads option 6 (first name server; a length that is not whole addresses is skipped).
* `VirtioNet` holds an `Addressing` (address, gateway, mask, name server, lease time, leased) in a
  `Cell`, starting at the QEMU defaults. `dhcp_lease` runs DISCOVER, REQUEST and ACK, accepts only
  an ACK that grants the offered address, and only then replaces the addressing. Every place that
  used the constant (ARP sender, ICMP and UDP source, receive filters, the `Ipv4Link` local
  address, the DNS next-hop) now uses the device's address; the next hop is chosen by the leased
  mask, not by comparing three octets.
* DISCOVER and REQUEST go out from `0.0.0.0` (RFC 2131 section 4.1), not from an assumed address.
* The boot suite takes the lease before its first ARP, so the gateway it proves is the one the
  network named. Invariant 8 changes from "the offer equals the constant" to "the lease is
  requested and acknowledged, and the driver speaks from the leased address" (the count stays 9).
  Each CPU logs `[net] lease A via G for N s (dhcp acknowledged)`.
* The console's `net` facts and every TCP connection use the device's address; `resolve` with no
  server asks the lease's name server (QEMU's `10.0.2.3` when no lease named one).

## Evidence (2026-10-08)

* Host: `dhcp.rs` tests for the REQUEST layout, ACK parsing (router, first of two name servers,
  86 400 s lease), an OFFER refused where an ACK is due, a foreign transaction id, a NAK refused
  by name, and a malformed name-server option skipped.
* `scripts/vm-e2e.sh`: the first boot logs `lease 10.0.2.15 via 10.0.2.2 for 86400 s`; the third
  boot runs on QEMU `net=192.168.76.0/24` and must log `lease 192.168.76.15 via 192.168.76.2`
  and still reach `[e2e] PASS`, so every network invariant holds on an address the constant never
  named. riscv64 and x86-64 boot gates pass with the lease taken.
* `dns-e2e`, `https-e2e` and `console-e2e` pass.

## Not done

* The e1000 driver (ADR-224) still uses the constant; it is the next driver to take a lease.
* No renewal: a lease shorter than a session would lapse silently. The lease time is recorded so
  a renewal timer can use it.
* The browser's pinned-host name server (`nameserver`, ADR-178) keeps its own default.
