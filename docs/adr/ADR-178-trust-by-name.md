# ADR-178 — Trust by name

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-WEB-009 (new), REQ-NET-004 (advanced)
**Builds on:** ADR-156 (the browser navigates), ADR-159 (Lethe's policy contract), ADR-176 (a name
becomes an address).

## Context

ADR-176 gave the console `resolve` and deliberately wired it into nothing: `trust NAME IP PIN` still
needed the operator to type the address. The deferred question was whether pinning a host should
ask the network for its address, and under which rules.

## Decision

* **`trust NAME PIN`** (no IP) asks the navigator's nameserver for NAME and pins the first address
  it answers, under the PIN the operator typed. `trust NAME IP PIN` is unchanged.
* **`nameserver [ADDR] [PORT]`** sets where it asks (default 10.0.2.3:53, QEMU user-net's resolver);
  with no arguments it prints the current one. It is configuration, like the trust and block
  lists: `forget` keeps it. Setting it is authorized as a WRITE.
* **A blocked host is refused before the question is asked** (ADR-159's "blocked hosts refused
  before lookup"). The console says so, and the name server never sees the name.
* **A failed lookup pins nothing** and says why, in the resolver's own words.
* **The pin stays the whole of the trust.** The console prints that it asked, and that the answer
  only says where to dial. A lying server can make `go` dial the wrong machine; that machine cannot
  present a chain the pinned root signed for the typed name.

## SYN backoff, found on the way

The live gate failed once on x86-64: a TCP connect sent 5 SYNs 200 ms apart, the emulator's NAT on a
host at load average 46 answered none of them in that second, and the connection was declared gone.
`tcpconn` now backs off SYN retransmissions exponentially (RFC 6298 section 5.5): each unanswered
SYN doubles the wait, so the same five tries cover about 31 RTOs (~6 s at the targets' 200 ms)
instead of 5. Boot invariant `tcpconn` 3 now proves the SPACING (each gap double the last) as well as
the budget. Data retransmission is unchanged.

## Proof

* Host: `trust_by_name_asks_the_nameserver_but_never_for_a_blocked_host` (`kernel-core/tests/shell.rs`)
  counts every question the host is asked: a blocked name costs zero, an unknown name pins nothing,
  `nameserver` changes where the next question goes, `trust NAME IP PIN` asks nothing.
* Live: `scripts/https-e2e.sh` runs a DNS stub beside the HTTPS peer. The session sets
  `nameserver`, blocks `evil.test` and tries to trust it (refused, and the stub's log shows no query
  for it), tries an unknown name (refused by name, nothing pinned), then pins `aletheia.test` by name
  and browses it over TLS 1.3 exactly as before. PASS on aarch64, riscv64 and x86-64.

## Consequences

**Good.** The operator names a host and a root; the network supplies the address.

**Costs.** One more outward-facing command. The pinned address is a snapshot: if the host moves,
the operator re-trusts it (the TTL is not honoured).

**Not claimed.** `go` still dials only pinned hosts and never resolves on its own.
