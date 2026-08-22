# ADR-060: The machine asks the network where it lives

**Status:** Accepted · **Date:** 2026-08-22 · **Closes:** part of ALET-P2-020 (networking stack, second slice) · **Builds on:** ADR-041, ADR-043, ADR-057

## Context

ADR-041 delivered the first networking slice: a virtio-net driver on all three targets over both
buses, ARP resolution, and ICMP echo — proved against QEMU's user-mode gateway because a
transmit-only driver proves nothing. The register row honestly listed what was still missing, and
three items of it were the same hole seen from three sides:

* every ARP resolve **broadcast**, because there was no cache — an address answered a microsecond
  ago would be re-asked forever;
* the only protocol above IP was ICMP, so the driver had never carried a **datagram service** —
  no UDP, and therefore nothing a real network application could ride on;
* the guest address was the constant `10.0.2.15`, justified as "what QEMU expects" — **spec
  recall about one simulator**, not a fact the machine had ever verified.

## Decision

The second slice closes all three, each at the choke point the first slice built:

1. **An ARP cache that is observable, not folklore.** `kernel-core/src/arpcache.rs` — fixed
   capacity (8 bindings, no allocation on the resolve path), byte-exact keys (an ARP cache that
   "closely" matches is a spoof), refresh-in-place (bindings change; a refresh never consumes a
   second slot), LRU eviction by a monotonic tick. `VirtioNet::arp_resolve` consults it BEFORE any
   broadcast, and a wire-request COUNTER makes the cache's whole point testable: the suite resolves
   twice and requires the counter to stay at 1. A cache that "worked" while the wire still saw a
   second request would be a cache in name only.

2. **UDP over IPv4, verified at both ends.** `kernel-core/src/udpv4.rs` owns the internet
   checksum — ONE implementation, re-exported by the ICMP path so the two protocols cannot drift
   into two definitions of "well formed" — and both directions of the datagram:
   `build_datagram` (IPv4 header + UDP segment, both checksums correct, caller-chosen
   identification) and `parse_ipv4`/`parse_udp` (fail-closed: version, header length, total
   length, header checksum, UDP length in both directions, and the UDP checksum computed over the
   PSEUDO-HEADER, so a datagram re-addressed in flight fails verification instead of parsing). One
   deliberate strictness: a ZERO received checksum — RFC 768's "sender did not compute it" — is
   REFUSED by name rather than accepted, because the checksum is this module's only evidence that
   the bytes a device DMAd up are the bytes a peer sent; the peer on this wire (slirp) always
   computes it. The host suite proves the strong property exhaustively: EVERY single-bit flip
   anywhere in a received datagram is refused by SOME checksum, and every truncation is refused.

3. **DHCP DISCOVER → OFFER: the machine asks the network where it lives.** The guest keeps its
   static configuration; the OFFER is EVIDENCE, not a lease — nothing is REQUESTED, taken, or
   renewed. `dhcp::write_discover` builds the question (broadcast flag set, minimum-length padded
   per RFC 2131 §2); `dhcp::parse_offer` reads the answer fail-closed: op must be BOOTREPLY, the
   magic cookie must match, the transaction id must be OURS (an answer to someone else's question
   is not an answer to ours), the option walk is bounds-checked at every step with PAD and END
   honored, the message type must be OFFER, and an offer of NO address is refused. The suite's
   invariant is the point of the whole slice: **the address the network offers must equal the
   address the driver claims** — the constant is now cross-checked against the authority that
   assigns addresses, and a change on either side fails the boot instead of going silent.

The demultiplexer is stated honestly rather than over-built: a received frame is routed by
ethertype, IP protocol, and UDP port through the SAME `recv_until` the first slice used — frames
that are not what the caller waits for are counted and dropped. There is still no socket layer, no
interrupt-driven completion, no TCP; those remain the register row's open substance.

## Consequences

* The network suite grows 5 → 9 invariants, all four new ones in the cross-target conformance
  contract (a DHCP OFFER that verified on aarch64 must verify identically on RISC-V and x86-64 —
  "the network works" must not vary by CPU or bus).
* The gates now REQUIRE the machine to complete a real UDP transaction against a peer that was
  never hardcoded to answer it — a stronger receive-path proof than echo, because the OFFER's
  contents (yiaddr, server id, lease) are checked, not just its arrival.
* `virtionet::checksum` moves to `udpv4` (re-exported): one checksum implementation for the
  whole stack, tested where it lives.
* Host proofs live in `kernel-core/tests/netstack.rs`: the full receive path replayed on the host,
  the exhaustive bit-flip refusal sweep, the transaction-id binding, the re-addressing attack
  (recomputed honestly — the thief FIXES the IP header checksum, and the pseudo-header catches it
  anyway), and truncation at every prefix.

**Not claimed:** no TCP, no routing, no fragmentation, no DHCP REQUEST/ACK or renewal (the OFFER is
never taken), no option overload, no relay agents, no socket layer, no interrupt-driven completion.
The register row ALET-P2-020 stays OPEN with the slice delivered, exactly like the first one.
