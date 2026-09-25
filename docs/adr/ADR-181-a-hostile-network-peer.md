# ADR-181 — A hostile network peer, live

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-NET-008 (new), REQ-SEC-TLS-010 (advanced)
**Builds on:** ADR-151 (TLS over TCP), ADR-155 (the HTTP client), ADR-161 (the hostile page),
ADR-176 (DNS), ADR-180 (the console under hostile input), ADR-063 (the heap never frees).

## Context

The TLS, HTTPS and DNS gates prove the network against peers that behave. The hostile-page
campaign attacks the parsers on the host. Nothing attacked the live stack - driver, TCP, the TLS
pump, the HTTP reader, the resolver - from the far end of a real socket, and nothing measured
what a long run of conversations costs the heap.

## Decision: `scripts/net-fuzz-e2e.sh`

One TCP port and one UDP port on the host. Every connection draws a behaviour from a seeded
generator: close at once; reset (RST); random bytes; a TLS record header promising 16 MiB;
TLS-shaped garbage records; a one-byte trickle; a 256 KiB flood; accept and stay silent; a real
TLS 1.3 handshake followed by garbage records; a real handshake followed by a malformed HTTP head
(an absurd Content-Length, bad chunking, Content-Length AND chunked, a 9,000-byte header, 500
headers, not HTTP at all). Every UDP query draws: random bytes, a truncated header, a reply to
another id, a pointer loop, silence, an answer count of 65,535. A scripted operator types `tcp`,
`tls`, `https` and `resolve` at the peer, each followed by an `echo` sentinel, on aarch64, riscv64
and x86-64. PASS: every command answered (a refusal by name is an answer), no panic, the heap grew
less than 48 KiB + 400 B per command, and `halt` gives the clean exit code.

## What it found

Nothing crashed, hung or was accepted that should not have been. It found **heap leaks per
conversation** on the heap that never frees: about 1.3 KB per `tls` and 4.8 KB per `https`
command, measured with `mem`'s heap line (ADR-180) on real aarch64. The causes were in the crypto
primitives, which the TLS key schedule calls dozens of times per handshake:

* `hmac_sha256` concatenated the key pad and the message into a `Vec` (twice per call). It now
  streams through a new incremental `crypto::Sha256`.
* `ed25519::verify` concatenated R || A || M into a `Vec` per signature (the certificate and
  CertificateVerify, each handshake). It now streams through a new incremental `sha512::Sha512`.

`sha256()` and `sha512()` are now one-shot wrappers over the streaming forms, so every existing
known-answer test and boot invariant checks them; new host tests prove streaming in any split
equals the one-shot digest. After the fix, measured over 120 hostile conversations: `tcp`, `tls`
and `resolve` cost 0 bytes in steady state. `https` still costs 100-200 B on some conversations
(and 33 KB once, the record workspace ADR-143 builds the first time a handshake reaches traffic
keys). **Attributed later the same day** with ADR-182's `heaptrace` (`NET_FUZZ_ELF` +
`NET_FUZZ_LOG` + `scripts/heaptrace-symbolize.py`): after the console starts, the only allocations
in an 80-command storm are that one-time workspace and the command history filling to its 32
entries (each entry `MAX_LINE` bytes since ADR-182). The "https residual" was history first-fill,
charged to whichever command's line was remembered first; nothing is spent per conversation.

## Consequences

**Good.** The live network stack is attacked on every CPU by a peer that lies in every way the
generator knows, and a long browsing session no longer leaks kilobytes per page.

**Costs.** The gate takes about 30-40 s per CPU for 80 commands.

**Not claimed.** The peer does not attack the TCP layer below the socket (no forged segments,
no out-of-window data).
