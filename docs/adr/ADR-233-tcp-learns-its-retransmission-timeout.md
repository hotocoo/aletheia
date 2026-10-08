# ADR-233 — TCP learns its retransmission timeout (RFC 6298)

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-NET-004 (tightened)
**Amends:** ADR-138 (TCP connection: "no RTT estimation, a fixed retransmission timeout"), ADR-178
(SYN backoff).

## Context

Every connection the console opens (`netstatic.rs` on all three CPUs) uses a fixed 200 ms
retransmission timeout. Only the SYN backed off. On a path whose round trip is longer than 200 ms
(a satellite link, a continent away, a loaded emulator's NAT), every data segment was sent again
before its acknowledgement could return, and a peer that went quiet was declared gone after about
one second (five retries of 200 ms). That is acceptable for the gate's local peers and wrong for a
real network.

## Decision

`kernel_core::tcpconn::Connection` implements RFC 6298:

* **Estimate.** SRTT and RTTVAR in Jacobson's fixed point (SRTT x8, RTTVAR x4); the first sample
  sets SRTT = R, RTTVAR = R/2; later ones move them by 1/8 and 1/4. RTO = SRTT + max(1 tick,
  4 RTTVAR).
* **Bounds.** The caller's timeout is the initial value AND the floor (200 ms at the console, the
  same as Linux's minimum); the ceiling is 300 times it (60 s at the console). On a fast path the
  connection therefore behaves exactly as before.
* **One timed segment at a time.** New data starts timing when nothing is being timed; an
  acknowledgement that covers it is the sample. A SYN that went once times the handshake.
* **Karn's rule.** Any retransmission cancels the timing: an echo that could belong to either send
  is not a measurement.
* **Back-off is kept** (section 5.5/5.7). A timeout doubles the RTO itself, and the doubled value
  stands until a clean sample recomputes it. Without this, on a path slower than the initial
  timeout, every segment is retransmitted before its echo, every echo is discarded by Karn's
  rule, and the estimate never learns the path. Data and FIN retransmissions now back off as the
  SYN already did, so five retries cover about 31 timeouts instead of 5.
* `Connection::rto()` reports the current value.

Callers are unchanged: the tick unit is still theirs.

## Evidence (2026-10-08)

`kernel-core/tests/tcp.rs`:

* A 26-tick round trip against a 10-tick initial timeout, 20 000 bytes (38 segments): every byte
  arrives in order, the RTO settles at 29, and the whole transfer costs 2 retransmissions (the
  SYN and the first data segment, both before any sample). A fixed 10-tick timeout sends every
  segment at least three times. With sampling disabled the RTO stays at the backed-off 40 and the
  test fails (checked by mutation).
* A 2-tick round trip keeps the RTO at its 10-tick floor with no retransmission.
* An acknowledgement of a retransmitted segment leaves the backed-off RTO in place; removing the
  Karn cancellation makes that test fail (checked by mutation).
* All earlier TCP tests and the per-CPU `tcpconn` boot suite pass unchanged, including the SYN
  doubling invariant.

## Not done

No congestion control (slow start, congestion window), no fast retransmit on duplicate
acknowledgements, no timestamps option. The send buffer (1 KiB) bounds in-flight data far below
any congestion window, so congestion control has nothing to govern until that buffer grows.
