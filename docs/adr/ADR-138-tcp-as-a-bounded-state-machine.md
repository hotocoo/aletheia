# ADR-138 — TCP as a bounded state machine with no device in it

- **Status:** accepted
- **Date:** 2026-09-17
- **Requirement:** REQ-NET-004, ALET-P2-020 (the network rung), Lethe integration stage N1
- **Supersedes:** nothing. Extends ADR-059 (virtio-net), ADR-060 (UDP over IPv4) and ADR-063 (the
  heap that never frees).

## Context

`docs/LETHE-INTEGRATION.md` states the position without decoration: Lethe's engine does not run on
this kernel, and the decisive gap is not the engine at all — it is that **this stack has no TCP**.
ARP resolves, ICMP echoes, DHCP discovers and UDP carries datagrams; every protocol a browser needs
sits on a transport this tree did not have. Stage N1 of that page is the blocker for stages N2
through N6, and it is named as not started.

TCP is also the place where a `no_std` kernel most easily acquires two failure modes it has spent
every other rung avoiding. The first is unbounded memory: the textbook implementation grows a send
queue and a reassembly queue as the peer dictates, which on a heap that never frees (ADR-063) is a
peer-controlled leak. The second is unbounded waiting: a connection with no retransmission budget
waits forever on a peer that has gone, and "the machine is hung" is indistinguishable from "the
network is slow" to whoever is looking at it.

## Decision

Implement TCP the way this tree implements every other contract: as a model with no device in it,
bounded by construction, total, with named refusals, proved on every CPU at boot.

Two modules, split so neither needs the other to be provable:

- `kernel-core/src/tcp.rs` — **the wire.** Parsing that refuses rather than reads (a data offset
  that lies about the buffer, a checksum that does not verify over the pseudo-header, a zero port,
  an urgent pointer this stack has no channel for), building that writes both checksums correctly
  or writes nothing at all, and sequence arithmetic done as **wrapping differences**, never as
  integer comparison — `a < b` on `u32` is wrong exactly at the wrap, which is unreachable by
  casual testing and reachable by a peer.
- `kernel-core/src/tcpconn.rs` — **the connection.** RFC 793's client states (no LISTEN, no
  SYN-RECEIVED: this is a client), a fixed send buffer, a fixed receive buffer, a fixed
  retransmission timeout and a retransmission budget. It is **fed** parsed segments and it **hands
  back** bytes to transmit; it never touches virtio-net, so it holds on a machine with no NIC and a
  slow device can never be a stalled state machine.

The contract is proved in `kernel-core/src/tcpsuite.rs`, which drives **real bytes**: every step
builds a segment with the builder, parses it with the parser, and hands the view to the connection.
A connection cannot pass this suite while disagreeing with what is actually on the wire.

Four bounds are deliberate, and each is a refusal with a name rather than a silent drop:

1. **Nothing is reassembled.** An out-of-order segment is dropped and re-acknowledged, which makes
   the peer retransmit. Reassembly is a queue a peer fills; retransmission is the peer's memory,
   not ours.
2. **Data with no room does not advance the acknowledgement.** The bytes are dropped and the peer
   sends them again once the application has read. A stack that acknowledges data it could not keep
   has lost it silently, which is the one outcome worse than being slow.
3. **A peer that stops acknowledging is declared gone** after `MAX_RETRIES`, and the connection
   says so by name. A connection that waits forever is a hang with a state machine attached.
4. **No allocation, ever.** The buffers are fixed arrays sized at construction, and the boot suite
   measures the platform's own heap watermark across two hundred segments in and out to prove it.

## What this deliberately does NOT do

No selective acknowledgement, no window scaling, no RTT estimation (the timeout is fixed and handed
in), no simultaneous open, no listening socket, and no congestion control beyond the peer's
advertised window. Each is a thing stages N2-N6 can be built without on this wire, and each is
written here rather than discovered by someone reading the code looking for it.

## The invariants (`tcp=9`, `tcpconn=15`, proved on all three CPUs at boot)

The wire: a built segment parses back to exactly what was written; any flipped byte is refused by
checksum; a segment re-addressed in flight fails the pseudo-header; a data offset below the header
or past the buffer is refused; a short segment is refused; sequence comparison is wrapping and a
zero window contains nothing; a segment's sequence length counts SYN and FIN as well as its bytes;
an urgent segment and a zero port are refused by name; a buffer too small refuses and writes
nothing.

The connection: a closed connection transmits nothing and refuses to send; open sends one SYN and
stays quiet until the timer expires; an unanswered SYN is retransmitted to the budget and then the
peer is gone; only a SYN+ACK acknowledging our own SYN opens the connection; data goes out one MSS
at a time and an acknowledgement frees exactly its bytes; a zero window stops data without losing
it; an acknowledgement of data never sent is refused; in-order data is readable once and
acknowledged by exactly its length; an out-of-order segment is dropped, re-acknowledged and
counted; data with no room is dropped without being acknowledged; a reset ends the connection by
name; a close sends FIN after the staged bytes and walks to TIME-WAIT; the peer's FIN opens
CLOSE-WAIT and ours closes from there; a segment for another port pair is not ours; and two hundred
segments in and out allocate nothing at all.

Host tests attack the same code from the other side: every single **bit** of a valid segment is
flipped and must be refused, a scripted peer drops every third segment and every byte still arrives
in order, a peer that stops acknowledging ends the connection within the budget, and a full receive
buffer defers data rather than losing it.

## Alternatives considered

**Port an existing `no_std` TCP stack (smoltcp).** Rejected for this rung, not on quality: the
proof obligation in this tree is boot-time invariants on three CPUs with named refusals and a
measured allocation claim, and adopting a stack means adopting its allocation and error model
wholesale rather than proving ours. The decision is revisitable if stages N2-N6 need features this
model deliberately omits.

**Put the connection behind the virtio-net device, driven by the NIC's interrupt.** Rejected for
the same reason the file panel owns no block device (ADR-137): a state machine that can only be
exercised with hardware attached is a state machine that is proved on one machine and hoped for on
the others.

**Grow the send buffer on demand.** Rejected: on a heap that never frees, "grow on demand" is
"leak on demand", and the demand is the peer's.

## Consequences

Stage N1 of the Lethe integration is delivered, and stages N2 (TLS 1.3) through N6 (the policy
contract adopted natively) now have a transport to sit on. Nothing above TCP exists yet, and the
integration page still says so.

The connection is not yet attached to virtio-net: no socket is opened by a live machine in this
wave, so what is delivered is the transport's contract rather than a network conversation. That
attachment is the next rung's work, and it is named in the register rather than implied here.
