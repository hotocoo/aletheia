# ADR-080 — The watch is wired to the clock: the governor runs on real timer interrupts

**Status:** Accepted (2026-09-12)
**Requirements:** REQ-PM-002 (advanced)
**Builds on:** ADR-079 (the advisor takes the watch), ADR-078 (Lethe), ADR-076 (the
power/performance contract), ADR-039 (re-entrancy is detectable and fatal), ADR-056 (the honesty
rule), ADR-061 (the gate counts itself).

## Context

ADR-079 built the watch and proved it on three targets, and then said plainly what it had not done:

> Nothing calls `tick` from a real timer interrupt yet. The watch is built, proved, and booted on
> three targets; wiring it to each target's timer IRQ and to the scheduler's busy/idle accounting
> is the next rung, and is deliberately separate so that the contract is proved before it is
> connected.

That is this ADR. It is a short one, because ADR-079 did the hard thinking; what remains is to
connect it without introducing the two failure modes that connecting a governor to an interrupt
handler classically introduces.

## Decision

### The watch stands behind one lock, and no caller ever waits for it

`lethed::resident` holds one `PmEngine`, one `ResidentGovernor` and the verified advisor in a
single `SpinLock<Option<Watch>>`, for the machine's whole uptime — the same shape `mlsched`'s
resident forest has had since ADR-056.

Every entry point uses a new `SpinLock::try_lock`, never `lock`. This is not a preference. A timer
handler runs *on top of* whatever it interrupted, so if the interrupted code holds the watch lock,
spinning waits for code that cannot run until the handler returns: a deadlock on one core, with no
second core to blame for it. `try_lock` turns that into `contended()`, a counted stand-down the
machine reports and the gate does not fail on — a nonzero count is a fact about the machine, not a
fault in it.

### An uncommissioned watch is a no-op

Before `commission`, `on_timer_tick` and `account` do nothing and return. The interrupt may
therefore be wired before the governor is stood, in either order, and an early interrupt is a
no-op rather than a crash or a stale act. `commission` itself refuses a second call, so a live
governor can never be silently replaced.

### Demand is measured from the machine's own busy/idle split

Each target's timer handler accounts the interval it just closed:

* **x86-64** — the PIT's IRQ0 handler reads an `IDLE` flag that the boot path sets around its
  `hlt`, so a tick that woke a halted core is an *idle* tick and a tick that interrupted working
  code is a *busy* one.
* **aarch64** — `el0_irq` fires while a ring-3 task is running, so the slice it closes is busy.
* **RISC-V** — the S-mode timer trap, same reasoning, same accounting.

Nothing declares demand on the governor's behalf. What the machine did is what the governor sees.

### Temperature is a named stand-in

No target exposes a thermal sensor to a guest, so each handler reports a fixed benign temperature
and says so in the source. The power contract still owns what a trip would mean; nothing here
pretends to measure heat. This is the ADR-056 rule and the same posture ADR-076 took.

## What the machine now prints, and what is gated

On x86-64, the same governor, on the same boot, in two opposite regimes:

```
[lethed] watch commissioned: true (1 domain, 4-point ladder, nominal 1.8 GHz)
[lethed] THE WATCH IS LIVE: 5 of 5 real IRQ0 ticks admitted, demand 0% measured,
         point index 0 (census balances, 0 lock stand-downs)
[lethed] the watch under load: 14 of 14 ticks admitted, 0 consulted, 14 warm-up,
         demand 100% measured, point index 2 of nominal 2 (0 lock stand-downs, 0 contract refusals)
[lethed] THE ADVISOR IS CONSULTED LIVE: 1 consultations over 16 admitted real timer ticks
         (15 warm-up, 0 resyncs, 0 cooldown holds, demand 100%, point 2)
```

The core halted: demand measured 0%, the governor dropped to the lowest point. The core working:
demand measured 100%, the governor climbed to nominal and stopped there. Nobody told it either
number.

aarch64 and RISC-V print the same `THE WATCH IS LIVE` line from their own ring-3 runs — 6 of 6 real
timer IRQs admitted, demand 100% measured, point index 2 of nominal 2.

**Gated** are the contract's properties, never the numbers: the census balances, `pm_refusals == 0`,
the governor range is never left, measured demand is actually answered, and — on x86-64 — the
advisor is genuinely consulted on live measurements. Boot fails `619` (not standing / not live),
`618` (census does not balance), `617` (contract refusals), `616` (left the governor range), `615`
(demand not answered), `614` (advisor never consulted live).

A fifteenth boot invariant joins `lethed_suite` on all three targets: the machine-wide watch is
stood exactly once and is a no-op until it is.

## Consequences

* **Named non-claims.** The advisor reaches live consultation on **x86-64 only**. aarch64 and
  RISC-V arm their timer for the ring-3 preemption run and disarm it afterwards, so their watch
  sees six slices — under one 16-sample observation window — and correctly reports itself as still
  warming rather than pretending to a consultation it did not make. Giving those targets a
  free-running periodic tick is a separate rung and is not claimed here.
* **Still no hardware frequency control.** The governor now decides on live measurements and the
  contract records every act, but no MSR/CPPC/ACPI programming exists: QEMU TCG exposes no
  frequency control to a guest, so that rung could only prove code ran, not that anything was
  enforced (the ADR-071 posture).
* **Still nothing about other operating systems.** This wave says the governor is live, measured
  and bounded on this kernel. It does not say Aletheia's power management beats Linux, Windows or
  anything else; no such comparison has been run, and ADR-078's benchmark numbers remain inside the
  trainer's documented cost model.
* **`SpinLock::try_lock` is new**, and is the only form an interrupt handler may use. Existing
  `lock()` callers are untouched.
* **Marker map changed deliberately** (`lethed=15` on all four gates including VirtualBox;
  ADR-061). The conformance contract is unchanged at 166 behaviors.
