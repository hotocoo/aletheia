# ADR-176 — A name becomes an address

**Status:** Accepted (2026-09-25)
**Requirements:** REQ-NET-007 (new)
**Builds on:** ADR-060 (DHCP: the machine asks where it lives), ADR-151 (TLS over TCP),
ADR-156 (the browser navigates), ADR-159 (Lethe's policy contract).

## Context

Every peer's address was typed by the operator: `tls ADDR …`, `https ADDR …`, `trust NAME IP PIN`.
The machine had a UDP round trip (ADR-060) and no way to ask a name server anything. "No DNS" was
an open item on the browser's list.

## Decision

* **`kernel_core::dns`.** One query shape: one name, type A, class IN, recursion desired
  (RFC 1035). One bounded reader of the answer. It refuses by name: a bad name (empty, over-long,
  a label outside `a-z A-Z 0-9 -`, a dash at a label's edge, a trailing dot), a short or truncated
  message, a question sent back, an answer to someone else's question (foreign id, or the right id
  with a different question section), TC set, NXDOMAIN, any other server error, a malformed
  compressed name, and an answer with no address for our name.
* **Compression cannot loop.** A pointer must point strictly backward, and one name may follow at
  most 16 pointers. A CNAME chain is followed inside the answer, at most 8 links. At most 4
  addresses are kept; more are counted.
* **The device path** is `virtionet::resolve_name`: the query goes out through the existing
  `udp_exchange` (IPv4 and UDP checksums verified end to end), to the gateway's MAC when the server
  is off this /24. The query id and source port derive from the machine's clock and its port
  counter, so an off-path spoof must guess both.
* **The console's `resolve NAME [SERVER [PORT]]`** prints the addresses, the TTL, the CNAME links
  followed and how many addresses were not kept. The default server is QEMU user-net's resolver,
  10.0.2.3. It is authorized as a WRITE to the world, like `tcp`, and the hosted planner classifies
  it as outward-facing (approval required).

## What an answer is worth

Nothing here is authenticated (no DNSSEC). An answer says where to DIAL, never whom to BELIEVE:
`trust` still takes its address from the operator, and TLS still verifies the peer's chain against
the pinned root for the typed name. A spoofed answer can cost availability, never authenticity.
This ADR deliberately does not wire `resolve` into `go` or `trust`.

## Proof

* Boot: `dns_suite`, 9 invariants (the 9th a real CNAME chain) on aarch64, riscv64 and x86-64 (`dns=9` in all three expected
  maps, and the 9 rows in `conformance.sh`), exit base 1060. Host tests add a
  self-referencing CNAME that stays bounded, the address cap, and case-insensitive names.
* Live: `scripts/dns-e2e.sh` (a CI job) asks a Python stub on the runner six questions whose answers
  are known (two A records; a CNAME chain; NXDOMAIN; a spoofed id; TC; a self-pointing name) plus a
  malformed name, then one real name through QEMU's resolver. PASS on all three CPUs; the spoofed
  address never reaches the console. The real-name leg SKIPs by name on a host that cannot resolve.

## Consequences

**Good.** The operator can learn a host's address from the network, and every way an answer can
lie is refused by name.

**Costs.** One more outward-facing command. UDP only: a name whose answer needs TCP is refused, not
half-read.

**Not claimed.** No AAAA, caching, EDNS, DNSSEC or search domains. `go` still dials only pinned
addresses; whether it should resolve pinned names itself is a later decision.
