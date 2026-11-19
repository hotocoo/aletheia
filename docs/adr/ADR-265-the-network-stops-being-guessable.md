# ADR-265: The network stops being guessable

**Status:** Accepted (2026-11-19)
**Requirements:** REQ-NET-009 (new)
**Builds on:** ADR-138 (TCP as a bounded state machine; the ISN is the platform's choice), ADR-140
(the live conversation), ADR-060 (the ARP cache), ADR-153 and ADR-236 (entropy: virtio-rng, RDRAND),
ADR-176 (DNS), ADR-235 (one NIC door per command). Closes the network items ADR-263's "What this is
not" deferred.

## Context

ADR-263's audit named what a host on the same network could predict or forge:

* the TCP initial sequence number was the timer's low 32 bits XOR the local port shifted left 16;
* the local port was a counter starting at 49152, printed by the console's `net`;
* the DNS query id was the timer folded on itself, and its source port the same counter;
* a TCP segment was matched on its port pair alone, and any RST with that pair ended the
  connection, whatever its source address or sequence number;
* an ARP reply was believed if it named the asked address, whoever it was addressed to and whatever
  sender it claimed, and the virtio-net cache kept it until reboot.

An off-path attacker who knows roughly when the machine booted could therefore aim a reset, a
forged DNS answer or a forged segment at a conversation without ever seeing it, and an on-link one
could poison the cache for the rest of the boot.

## Decision

* **One secret, three numbers.** `kernel-core/src/netguard.rs` holds `NetSecret`, 32 bytes drawn
  once, on the first conversation, through each platform's existing `random` (virtio-rng, or RDRAND
  on x86-64 without one; ADR-236), which already refuses an all-equal draw. Every number is
  `SHA-256(key || domain || ...)` with a distinct domain byte, so the three uses cannot produce
  each other's values:
  * the ISN is RFC 6528's `M + F(local, lport, remote, rport, key)`, with `M` the platform timer in
    4-microsecond units (`iss_clock`), so one 4-tuple still moves forward across reconnects;
  * the local port is RFC 6056 algorithm 3: a keyed offset per (local, remote, remote port) plus a
    counter, in the IANA dynamic range 49152..65535. The port used towards one host says nothing
    about the port used towards another;
  * a DNS question's id and source port come from a keyed counter (RFC 5452), so a forger must
    guess both.
* **No entropy source is named, not hidden.** A machine whose `random` refuses seeds the secret
  from the boot clock: no weaker than before, and `net` says so (`ports and sequence numbers keyed
  from the clock (no entropy source answered)`). `net` no longer prints the next local port.
* **A connection is a 4-tuple.** `TcpView` carries the IPv4 source the checksum covered, and
  `Connection::on_segment` refuses a segment from any host other than its peer as `NotOurs`, before
  counting it.
* **A reset must prove it saw the connection.** In SYN-SENT a reset is believed only when it
  acknowledges our SYN (RFC 793). Once synchronized (RFC 5961 section 3.2) only a reset at exactly
  `rcv_nxt` ends the connection; one elsewhere in the receive window is refused as
  `ResetNotExact` and answered with a challenge acknowledgement, which a real peer answers with the
  exact reset; one outside the window is dropped without a reply.
* **ARP answers are checked once, for every NIC.** `netguard::arp_request` and `arp_reply` replace
  the two drivers' private copies (virtio-net, e1000). A reply is believed only when it is an
  Ethernet/IPv4 reply about the asked address, addressed to this machine's hardware and protocol
  address, from a sender that is unicast, non-zero and the frame's own source. Anything else is a
  frame that was not the answer, dropped like any other.
* **ARP bindings age.** The virtio-net cache records when the wire told it each binding and forgets
  it after `MAX_AGE_NS` (60 s, RFC 1122's order); a clock that reads earlier than the binding counts
  as expired. The e1000 keeps no cache and asks every time, as before.

## Evidence

* `[netguard] ALL 9 NETWORK-GUARD INVARIANTS HOLD` on aarch64, x86-64 and riscv64 (60-135 ms
  under TCG), and `[tcpconn] ALL 18 TCP-CONNECTION INVARIANTS HOLD` (15 before): a segment from
  another host with the right ports is refused; a reset inside the window but off `rcv_nxt` is
  challenged, one outside it dropped silently, only the exact one ends the connection; in SYN-SENT
  only a reset acknowledging our SYN does. The suites use fixed keys and fixed frames, so every check
  is exact, not statistical. VirtualBox requires the `netguard` line (no device in it).
* `kernel-core/tests/tcp.rs` `a_blind_reset_must_hit_the_exact_sequence_number`: 65 540 forged resets
  across the sequence space from the peer's own address; none ends the connection, the two inside
  the window each earn exactly one challenge ACK naming `rcv_nxt`, every other is dropped without a
  segment, and the exact reset from another host is still not the connection's.
* `console-e2e.sh` types `net` on all three CPUs and requires `ports and sequence numbers keyed from
  the entropy source` (added after the wave: the network gates pass equally on the old counter and
  clock ISN, so this is the live check that fails on the old code). A peer cannot see the source
  ports, which QEMU's user-mode NAT rewrites.
* `arpcache.rs` `a_binding_expires_after_its_max_age_and_a_backwards_clock_expires_it_too`;
  `netguard.rs` unit tests over the suite and `iss_clock`'s 4-microsecond scale.
* Live gates, all on this wave's tree: `vm-e2e.sh`, `vm-e2e-x86.sh` (three boots; its second boot
  once hit the 240 s watchdog at host load ~9 with clippy running beside it, and passed rerun
  alone, the flake recorded before), `vm-e2e-riscv.sh`, `tcp-e2e.sh`, `dns-e2e.sh`, `tls-e2e.sh`,
  `https-e2e.sh`, `net-fuzz-e2e.sh` (the hostile peer's seeded resets and lies), `browser-e2e.sh`,
  `console-e2e.sh` and `conformance.sh` (420 named behaviors, 12 new), all PASS with no SKIP; the
  quality gate (fmt, clippy on every crate and CPU, audit, licenses) and every `check-*.sh` doc gate.

## What this is not

* **Not a defence against an on-path attacker.** A host that sees the traffic sees the sequence
  numbers, the ports and the ids; only TLS (ADR-151) protects a conversation from it.
* **Not gratuitous-ARP or ARP-request handling.** This machine still answers nobody's ARP requests
  and learns only from replies to its own questions.
* **One conversation at a time.** Algorithm 3's counter never has to skip a port in use because the
  console holds one connection; a stack with concurrent connections must check the port against
  those it holds.
* **The clock-seeded secret is only as unpredictable as the boot moment.** It is reported, and a
  machine that wants better attaches an entropy source.
* **The e1000 boot suite's own ISN stays the clock.** It talks to the test echo peer at boot, not to
  the network a console conversation crosses.
