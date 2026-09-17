# ADR-139 — Where TCP meets the link: a bounded pump, and a driver with no opinion about TCP

- **Status:** accepted
- **Date:** 2026-09-17
- **Requirement:** REQ-NET-005, ALET-P2-020 (the network rung), Lethe integration stage N1
- **Supersedes:** nothing. Extends ADR-041/059 (virtio-net) and ADR-138 (TCP as a bounded state
  machine).

## Context

ADR-138 delivered TCP as a model with no device in it: the connection is fed parsed segments and
hands back bytes to transmit. That is what makes it provable on three CPUs with no NIC attached,
and it leaves one thing undone — nothing carries those bytes.

The obvious place to put the carrying is the driver, and that is the wrong place. A driver that
knows about handshakes and retransmission cannot be reasoned about as a driver, and a state machine
that can only be exercised through a device is a state machine proved on one machine and hoped for
on the others. The obvious alternative — a socket layer with its own buffers — is a second copy of
every bound ADR-138 already states.

## Decision

Three methods are all a TCP client needs from a network. Name them, put them in a trait, and give
the join its own module.

`kernel-core/src/tcpnet.rs` holds:

- **`Ipv4Link`** — `send_ipv4`, `recv_ipv4`, `local_ip`. Implemented by the virtio-net driver on a
  live machine, and by a scripted test double in this module's own suite.
- **`exchange`** — one request/response conversation: open, send the request once the handshake
  allows it, collect the reply until the peer closes, then close. It takes a **budget of poll
  turns** and a clock closure.

`kernel-core/src/virtionet.rs` gains exactly two public methods — `send_ipv4_to` and
`recv_ipv4_into` — and no more. The driver carries a datagram to a MAC and hands one back; it has
no opinion about sequence numbers, and `grep tcp kernel-core/src/virtionet.rs` finds only the
protocol number.

Three properties are deliberate:

1. **Every loop is bounded.** A network is the one place in this kernel where "wait until it
   answers" means "wait on a peer's decision". The budget is spent, then the refusal is named
   (`BudgetSpent`), and a caller who wants to wait longer asks for more turns rather than getting
   an unbounded wait by accident.
2. **The reply buffer is the caller's bound, not the peer's.** A reply larger than the buffer is
   truncated; nothing is written past it. This is asserted with a guard pattern in the suite
   because it is the shape of the classic remote overflow.
3. **Received bytes already in hand are not thrown away by a timeout.** A budget that expires after
   the peer answered returns what arrived, rather than converting a short answer into a failure.

## The invariants (`tcpnet=3`, proved on all three CPUs at boot)

- A request goes out over the link and the peer's answer comes back (the join, end to end, over a
  connection and a parser that are the real ones).
- A deaf peer costs exactly the budget and is refused by name, never waited on forever.
- A reply larger than the caller's buffer is truncated, never overflowed.

## Alternatives considered

**Put the pump in the driver.** Rejected: it makes the transport unprovable without hardware and
the driver unreviewable as a driver.

**A socket layer with its own send and receive buffers.** Rejected for now: the connection already
owns bounded buffers, and a second set would be a second place for the bound to be wrong. A socket
layer becomes worthwhile when more than one connection exists at a time, which is not this rung.

**Let the pump read the clock itself.** Rejected: a pump that reads a clock cannot be tested, and
this tree's habit is that time, like the frame allocator's reading and the filesystem's listing, is
handed in.

## Consequences

The transport and the device can now be joined, and the join is proved. What still does not exist
is a live machine opening a socket: no console command and no boot-time conversation uses these
methods yet, so the first real TCP conversation on this kernel remains the next rung's work. That
is named in the register rather than implied here.
