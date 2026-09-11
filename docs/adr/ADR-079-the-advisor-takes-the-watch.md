# ADR-079 — The advisor takes the watch: Lethe as a resident governor

**Status:** Accepted (2026-09-12)
**Requirements:** REQ-PM-002 (new), REQ-ML-006 (advanced)
**Supersedes:** nothing. **Builds on:** ADR-076 (the power/performance contract), ADR-078 (Lethe,
the resident performance advisor), ADR-039 (re-entrancy is detectable and fatal), ADR-056 (the
honesty rule), ADR-061 (the gate counts itself), ADR-063 (the boot heap never frees).

## Context

ADR-078 delivered an advisor and proved it hard: a frozen integer model in one verified `ALTH1`
blob, ten named load-time refusals, twelve boot invariants on three targets, thirteen host proofs,
and a published comparative benchmark that says where the model wins and where it honestly loses.

It also published a named non-claim, in as many words:

> no live governor thread exists yet (residency = wired into the model's govern path, proved at
> boot, a pre-REQ-ML-003 posture)

That sentence describes a machine where the advisor is a *function the tests call*. Three things
were therefore still true, and none of them were small:

1. **Demand was declared, not measured.** Somebody called `set_demand` with a number. On a real
   machine nobody does that; the scheduler's accounting does, or nothing does.
2. **Nothing ran on the clock.** The advised path was driven by a fixture replay, not by time.
3. **The tick was not part of the threat model.** It could not be — there was no tick.

The third is the one that decides the shape of this ADR. A governor that runs off the timer
interrupt is reachable by anything that can influence when the timer fires. Making Lethe resident
without saying what a *tick* is worth would take a carefully bounded contract and hand it an
unbounded input.

## Decision

Add `kernel-core/src/lethed.rs`: **the watch**. A resident governor that runs on the clock,
services exactly one domain per tick, measures demand instead of being told it, and treats the
tick itself as an authority question.

### The cadence is authority

A tick is admitted only if it is strictly newer than the last admitted tick and at least
`Cadence::min_gap` beyond it. Otherwise it is refused by name and **no state moves at all** — not
the round-robin cursor, not the observation history, not the contract:

| Refusal | What it names |
|---|---|
| `NotMonotone { last, got }` | a replayed or rolled-back timestamp |
| `TooSoon { gap, min }` | a source firing faster than the cadence floor |
| `Reentered` | a nested tick, or a second CPU (ADR-039 guard) |
| `NoDomains` | nothing is attached to service |
| `BadCadence` | a cadence that cannot admit anything (`min_gap` zero, or above `max_gap`) |

This is what makes a berserk or captured timer a *counted refusal* rather than a lever. A host
proof fires 10,000 ticks into a governor whose floor is 1,000 and asserts exactly 10 admissions and
9,990 refusals: churn — which on real silicon is energy and heat — cannot be amplified through this
door.

### A stale window is not a window

If the gap exceeds `Cadence::max_gap`, the machine moved without us and every remembered sample now
describes a machine that no longer exists. The governor does not interpolate and does not shrug. It
**resyncs**: it forgets every window, withholds the advisor until a *full* `DEMAND_WIN`-sample
window has refilled with post-gap truth, and counts how many times it did so. The same rule covers
cold boot, so the advisor is never consulted on a partially-filled ring — a feature derived from
six samples out of sixteen is not a weak signal, it is a false one. Withholding is ADR-056 applied
to time.

### The work per tick is bounded, constant, and allocation-free

Exactly one domain is serviced per tick, round-robin. A tick is one demand read, one sensor read,
one depth-3 tree walk, and at most one contract act — **independent of how many domains exist**. The
attached domain list is a fixed `MAX_DOMAINS` array claimed once at attach time, deliberately *not*
`PmEngine::domain_ids()`, which allocates. A full sweep takes `attached()` ticks; that is reported,
not assumed, and a host proof over five domains and a thousand ticks asserts each was serviced
exactly two hundred times.

### Demand is measured

`DemandMeter` converts busy/idle tick accounting — from whatever already accounts CPU time — into a
percentage over a **disjoint** window: `take_pct` consumes the window, so a burst is counted once
and cannot keep inflating later windows. Two details are contract, not implementation:

* Counters **renormalize rather than saturate.** Saturation is a silent lie: once `busy` and
  `total` both pin to `u64::MAX`, the ratio between them is destroyed and a fully loaded domain
  reports 1%. Halving both preserves the ratio, so an unconsumed window degrades in *precision*,
  never in *truth*.
* The arithmetic widens before it multiplies, and the result is clamped, so no magnitude and no
  over-reported `busy` can manufacture demand above full.

### The ceiling outranks the advisor

While a domain's thermal cooldown is latched, the governor observes but does not act on it. The
contract would *permit* a raise — a cooldown gates the overclock band, not the governor range — and
the governor declines anyway, because raising silicon that the thermal contract just clamped is
precisely how a machine ends up oscillating at its trip point. It still parks a genuinely idle
domain, the one act that can only help while cooling. `PmEngine::cooldown_remaining` became public
for exactly this: the resident must be able to *see* the ceiling holding in order to stand down on
its own.

### No new authority

The governor holds no grant and offers no tokens. `request_index` is therefore refused above
nominal for it by the same code that refuses any unauthorized caller, which means the overclock band
is unreachable **by construction, not by policy** — there is no code path from here into it. A host
proof sweeps 4,000 ticks across four domains at randomized full demand and asserts no point ever
exceeds nominal.

### Every act still flows through ADR-078

`lethe::govern_advised`'s sweep body was lifted into `lethe::govern_one_advised` and the sweep now
calls it per domain. The extraction is behavior-preserving — all 32 pre-existing `lethe` and `pm`
proofs pass untouched — so the resident inherits ADR-078's proofs whole, including the one that
matters most: **with the advisor absent, the advised path drives the machine through the same clock
sequence as the untouched ADR-076 baseline.** `PmEngine::govern` remains byte-for-byte unchanged.

## Proofs

**Host-exhaustive** (`kernel-core/tests/lethed.rs`, 15 tests): adversarial clock streams that jump,
stall, and run backwards with the census asserted to balance at *every* step; the berserk-timer
rate-limit; stale resync and full-window rewarm; the advisor-free resident landing exactly on the
baseline demand map over 40 randomized multi-regime trials; no reachable point above nominal over
4,000 ticks; demanded silicon never parked over 3,000 ticks; heat outranking the advisor for the
whole 200-tick cooldown with `pm_refusals == 0` throughout; meter exactness across the entire
0..=100 range; meter renormalization under `u64::MAX` input; capacity bounding and idempotent
attach; round-robin fairness; and the boot suite itself running on the host.

**In-kernel** (`lethed_suite`): 14 invariants on every boot of all three targets
(`[lethed] ALL 14 RESIDENT GOVERNOR INVARIANTS HOLD`, boot fails `620 + i`). Seven are pinned
cross-CPU in the conformance contract (159 → 166 named behaviors): replay refusal, the cadence
floor, one-domain-per-tick round-robin, withholding until the window is full, stale resync, heat
outranking the advisor, and the governor range never being left.

## Consequences

* **Named non-claims.** This wave makes the governor *resident*, not *hardware*. The kernel still
  does not program MSR/CPPC/ACPI frequency control — QEMU TCG exposes none to a guest, so a
  hardware rung attempted today could only prove that code ran, not that anything was enforced
  (the ADR-071 posture). Temperature is still reported by a caller, not simulated
  thermodynamically. The benchmark numbers from ADR-078 still live in the trainer's documented cost
  model and still say nothing about Linux, Windows, or any real operating system.
* **Nothing calls `tick` from a real timer interrupt yet.** The watch is built, proved, and booted
  on three targets; wiring it to each target's timer IRQ and to the scheduler's busy/idle
  accounting is the next rung, and is deliberately separate so that the contract is proved before
  it is connected.
* **Marker map changed deliberately** (`lethed=14` on the aarch64, RISC-V, and x86-64 gates;
  ADR-061), and the conformance contract grew seven behaviors on all three targets.
* **`PmEngine::cooldown_remaining` is now public**, read-only. `PmEngine::govern` and every ADR-076
  and ADR-078 semantic is otherwise untouched.
