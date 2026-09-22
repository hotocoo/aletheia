# ADR-148 — The wall clock: a time this kernel reads for itself

- **Status:** accepted
- **Date:** 2026-09-22
- **Requirement:** REQ-SEC-TLS-008, Lethe integration stage N2 (seventh rung)
- **Supersedes:** nothing. Extends ADR-147 (the verifier that takes the time as an argument) and
  ADR-146 (whose date conversion this rung takes over).

## Context

ADR-147's `PinnedRoot` refuses a time of zero or less, because this kernel had no clock and neither
reading of "no clock" is safe: as the epoch it would find every certificate not yet valid, as
"skip" it would accept every expired one. That left the verifier correct and unusable. A TLS
client that cannot tell what day it is cannot judge a validity window, and Lethe's stage N2 needs
one.

Every target has a real-time clock QEMU keeps in the host's UTC: the PL031 on `virt` (aarch64), the
goldfish RTC on `virt` (RISC-V), and the CMOS RTC on every PC. None had been read.

## Decision

`kernel-core/src/clock.rs` is the contract; each target crate owns its device.

- **A reading is plausible or it is a refusal.** `plausible` accepts only seconds in
  `[2026-01-01, 2100-01-01)`. A clock that says 1970 is not a clock that happens to be wrong; it is
  the absence of a clock reporting itself as the epoch, and a verifier handed that number would be
  wrong in exactly the way ADR-147 refuses. So the refusal is named (`Implausible`) before any
  caller can call the number a time.
- **The device is checked before it is believed.** `kernel/src/rtc.rs` reads the PL031's PrimeCell
  identification registers and trusts the data register only once the page has answered as a PL031
  (`Absent` otherwise). `kernel-x86_64/src/rtc.rs` waits for the update-in-progress flag, takes
  every field twice and accepts only two identical snapshots (`Unsettled` otherwise), decodes BCD
  or binary and 12- or 24-hour form as Status B declares, and hands the civil fields to a
  range-checked conversion so a month of 13 is refused rather than wrapped into a plausible-looking
  day. `kernel-riscv64/src/rtc.rs` reads the goldfish count LOW-then-HIGH, the one order that
  latches a consistent 64-bit value.
- **One conversion, shared.** ADR-146's days-from-civil algorithm moves from `x509.rs` to
  `clock.rs`, gains its inverse for printing, and is now the only date arithmetic in the tree: the
  certificate reader and every clock driver agree because they cannot disagree.
- **The platform's time reaches the verifier through one door.** `verifier_at(clock, root)` builds
  a `PinnedRoot` from a clock's reading, and a clock that refuses gives a verifier that does not
  exist — never one judging at time zero.

Each kernel prints its reading at boot (`[clock] platform time: 2026-09-22T09:54:06Z ...`) so a
person at the serial console can check it against their own watch.

## The proof

`clock=7` on all three CPUs at boot, each against its own device: the clock is present and reads a
plausible time; two reads never run backwards and agree within seconds; an absent clock is a named
refusal that builds no verifier; **the platform's own time builds a verifier that accepts ADR-147's
pinned fixture** — the first certificate this kernel judges at a time it read itself; the epoch, the
year 2000 and the year 2100 are refused as no time at all; civil dates convert to the seconds they
mean and back, leap days and the 2038 boundary included; a field out of range is refused rather
than wrapped.

Host (`kernel-core/tests/clock.rs`): the same suite against the HOST's clock, an independent time
source this kernel never implemented; the host's own date and this conversion name the same day;
every second of one day and every day of the 2000–2100 century round-trip.

Conformance contract 323 -> 330 core behaviours. Unsafe inventory: one volatile register seam per
target (`docs/UNSAFE-AUDIT.md`).

## Alternatives considered

**Trusting the fixed address.** Rejected for the PL031: PrimeCells carry an identity, and reading
it costs eight loads. The goldfish part has none, so plausibility is the whole of its check, and
the ADR says so rather than pretending otherwise.

**Defaulting an implausible reading to "now-ish".** Rejected: a default time is a forged time.

**Network time.** Rejected for this rung: NTP is unauthenticated, and a TLS client that sets its
clock from the network it is about to authenticate has a circular trust root.

## Consequences

`PinnedRoot` can now be built from a time the platform read. What remains before this kernel can
speak TLS is the `CertificateVerify` check over the transcript with the key the verifier returns,
and the client's own `Finished`. The fixture leaf expires on 2036-01-01; invariant 4 will say so
on that day, which is the correct behaviour of a clock.
