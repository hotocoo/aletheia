# ADR-076: The power/performance contract — frequency is authority, heat is a hard ceiling

**Status:** Accepted · **Date:** 2026-08-28 · **Advances:** ALET-P2-022 (the contract rung; the
hardware rung stays scoped) · **Builds on:** ADR-003 (capability security), ADR-048 (authority
lattice), ADR-056 (the idle machine costs nothing; the ML risk advisor), ADR-059 (approval is
governance, separate from authority), ADR-063 (never-freeing heap discipline), ADR-071 (the
modeled-contract posture)

## Context

The OS promises overclocking "straight to the components". In an Aletheia-shaped OS that phrase
decomposes into exactly two questions the rest of the kernel already knows how to answer:

1. **Who may push a clock?** — an AUTHORITY question. Ambient access to frequency control would
   be the same hole ambient access to memory or devices is; the whole architecture exists to
   refuse it.
2. **How far may the push go?** — a PHYSICS question. Authority must stop at the silicon's
   thermal envelope, and it must stop even when the authority holder disagrees, because heat is
   the one authority the hardware holds over the software.

ALET-P2-022 ("power management is not yet delivered") was deferred as milestone work. This ADR
starts that work the way ADR-071 started the IOMMU's: by defining, once, the contract a
power-management subsystem must satisfy, as a complete software model every proof can run
against today — before any MSR/CPPC/ACPI programming puts the cart before the horse.

## Decision

### The contract is defined once, as software, in kernel-core

`kernel-core/src/pm.rs` defines the full enforcement semantics as `PmEngine`:

* **Frequency domains with honest ladders.** Every core belongs to a domain with a discrete,
  strictly-ascending ladder of operating points (kHz + mV). Real DVFS selects points, not
  arbitrary frequencies, so every request names an exact point or is refused
  (`NotAnOperatingPoint`). Registration itself refuses dishonest ladders: empty, non-ascending,
  a nominal that is not a ladder point, or any point above the envelope (`MalformedLadder`).
* **The governor range is free; the overclock band is grants only.** `nominal` — the top of the
  governor range — is reachable by any caller, including the demand governor. Points ABOVE
  nominal are the OVERCLOCK band: refused `NoAuthority` unless one of the offered tokens is a
  LIVE, per-domain ELEVATION GRANT whose ceiling reaches the point. A grant that exists but
  does not reach is refused `NotGranted`, naming both sides. This is "OC straight to the
  components" under the only discipline this OS recognizes: authority is an explicit, scoped,
  revocable capability, never ambient.
* **Attenuation and cascade, reused, not reinvented.** Grants delegate equal-or-narrower
  (`Amplification` refused, the ADR-048 law), never across domains (`CrossDomain`), and
  revocation kills the subtree. Tokens are possession-based (`next_serial ^ secret`) —
  presenting a dead token is presenting nothing, and revoking an unknown token is an
  undistinguishable no-op.
* **Revocation clamps immediately.** A domain running in the OC band under a now-dead grant is
  back at nominal BEFORE `revoke` returns — the same law an unmapped DMA window obeys (ADR-071).
  A grant that never reached past nominal clamps nothing.
* **The envelope is absolute, and STRUCTURALLY so.** No ladder point above the envelope may be
  registered, and no grant past the envelope may be minted (`AboveEnvelope` refused AT MINT,
  naming the envelope) — so no reachable state exceeds the ceiling. The envelope is not a
  policy anyone remembers to apply; it is a shape the data cannot have.
* **A thermal trip clamps the whole machine.** Reporting a temperature at or above a domain's
  trip point returns EVERY domain to its lowest point and latches a cooldown on every domain,
  during which elevation is refused `Cooldown { remaining_ticks }` — even with a valid grant —
  while the governor range keeps serving the machine. Elevation returns exactly at expiry,
  tick-exact.
* **The governor never overclocks and never parks demanded silicon.** One governor step maps
  demand deterministically onto the GOVERNOR range only; zero demand parks a domain at its
  lowest point (the idle machine costs nothing, ADR-056) and any nonzero demand refuses parking
  (`DomainBusy`, the pct named).
* **Idle accounting is real.** Parking opens a residency span; waking books the span per state
  plus the wake latency (C1 = 1 us, C2 = 10 us). Any act that moves a parked domain's clock —
  a request, a governor step, a trip — CLOSES the span and books it: residency is real time
  and is never lost. Wake latency is booked only by an actual wake.
* **Device power moves along legal arcs only.** D0↔D1, anything→D3→D0. D3→D1 is refused
  `IllegalDState` — a device wakes through D0 or not at all. Self-transitions are refused too.
* **Everything is audited.** Every accepted transition AND every refusal lands in a bounded
  ledger (AUDIT_CAP = 128) under a monotonic sequence number with the act's class and, for
  grant acts, the holder. The ledger wraps — it forgets bytes, never events.

All tables are capacity-bounded (MAX_DOMAINS 16, MAX_GRANTS 64, MAX_DEVICES 32) because the
boot heap never frees (ADR-063).

### Proof posture: host-exhaustive + boot-compact

Host proofs in `kernel-core/tests/pm.rs` (19 tests): the full OC-band decision table for every
point × every grant ceiling × with/without authority; attenuation monotonicity swept over all
5^3 grant-ceiling chains; revocation clamps with cascades and idempotence; envelope
absoluteness from both registration and mint; cooldown tick-exactness across the whole window;
idle accounting under transition interference; the full device-arc table; ledger completeness
with wraparound; registration and capacity refusals; and bit-identical determinism for
identical op sequences.

In-kernel: `pm_suite`, 14 invariants on every boot of all three targets
(`[pm] ALL 14 POWER-PERFORMANCE INVARIANTS HOLD`, boot fails 560+i). Six of them are pinned
cross-CPU in the conformance contract — elevation without authority refused by name,
attenuation, revocation clamp + cascade, envelope absoluteness, trip clamp + cooldown, and the
governor never overclocking — because a target whose PM let an ungranted caller into the OC
band would be a different machine, whatever its CPU.

### Why modeled first

The same reason as ADR-071, plus one specific to power: QEMU TCG exposes NO frequency control
to the guest — no P-state MSRs, no CPPC, no ACPI \_PSS objects whose contents a guest could
honor. A "hardware rung" attempted today could only prove that code RAN, not that anything
ENFORCED. The model rung proves the enforcement semantics exhaustively now; when a real
platform (or an emulator that grows the feature) exposes frequency control, the hardware
implementation must satisfy the same contract the software already proved — the SoftIommu
posture exactly.

## Consequences

* **Named non-claims.** This wave delivers the CONTRACT, not silicon: no MSR/CPPC/ACPI
  programming, no battery, no system sleep/wake (S3), no voltage rail enforcement beyond
  recording mV in the ladder, and no thermodynamic temperature simulation — callers report
  temperatures, the contract decides. All of it stays scoped in the gap register.
* **The cooldown is the governance axis in miniature.** During cooldown even a valid grant is
  refused: some refusals are about the machine's state, not the caller's authority. The ADR
  deliberately does NOT add an approval seam to lift a cooldown early — heat does not negotiate.
* **Marker map changed deliberately** (`pm=14` on all four gates, ADR-061); conformance
  contract grew six PM behaviors on all three targets.
