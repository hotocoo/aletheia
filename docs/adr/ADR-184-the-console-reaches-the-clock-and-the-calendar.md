# ADR-184 — The console reaches the clock and the calendar

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-CON-009 (new)
**Builds on:** ADR-076 (the power/performance contract, overclock band grant-only), ADR-148 (the
wall clock), ADR-165/166 (Lethe, the resident power governor), ADR-089/180 (the console never grows).

## Context

The kernel runs a resident power governor on every CPU (`lethed::resident`, commissioned at boot on
aarch64, riscv64 and x86-64), reads a wall clock on every CPU (PL031, Goldfish, CMOS), and keeps a
grant-only overclock band in its power contract. None of it was reachable from the console: an
operator could not see the clock domains, could not ask for a point above nominal, and could not
read the date. A console that cannot reach what the machine is doing is not a console for that
machine, and it is the surface a future System-1 model is trained on, so gaps here become gaps there.

## Decision

Three commands, all in the shared dispatcher (`kernel-core/src/shell.rs`), so every CPU gets them:

* **`date`** — `ShellHost::wall_clock`, defaulted to `ClockRefusal::Absent`; each target returns its
  own RTC's `read_utc()`. Prints `YYYY-MM-DD hh:mm:ss UTC (unix N)` or `date: no wall clock (why)`.
* **`power`** — one `PowerFacts` copy taken under the watch's lock (`resident::facts`, fixed-size,
  no allocation): every domain's current/nominal/envelope clock, demand, idle state, cooldown, trip,
  ladder (overclock points marked `+`, current `*`), whether it is held, and the tick census.
* **`oc KHZ [DOMAIN]` / `oc off [DOMAIN]`** — an operator hold. `take_hold` asks the contract through
  the same `request_point` every caller uses, offering the console's grant, minted once per domain at
  the top of that domain's own ladder (the contract refuses a mint past the envelope); the governor
  then observes the held domain but never moves or advises it (`Census::operator_holds`). `oc off`
  returns the domain to nominal and to the governor. New authority class `ShellAction::Overclock` =
  capability `system.overclock`; the hosted planner classifies `oc` `Destructive` (a human answers).

**Heat still wins.** A thermal trip clamps the held domain like any other, and ENDS the hold rather
than pausing it (`Census::holds_dropped_by_heat`); during the cooldown the band refuses `oc` with the
contract's own `Cooldown { remaining_ticks }`.

Nothing here names a clock: the boot suite and the heap storm read the top point from the live
ladder via `resident::facts()`.

## Proof

* Host (`kernel-core/tests/lethed.rs`, 5 new): a hold reaches the band and 300 mixed busy/idle ticks
  leave it there with zero advisor consultations; a trip clamps it, ends the hold, and `oc` is then
  refused `Cooldown`; off-ladder and off-watch holds are refused by name and hold nothing; 1,000
  holds mint one grant (per-`oc` minting would hit `MAX_GRANTS` = 64); the facts copy matches the
  contract.
* Boot, all three CPUs: `console=54` (was 51) — `date`, `power`, and the full `oc` cycle
  (refused off-ladder, held at the top point, visible in `power`, released) against the real
  commissioned watch.
* Heap: `shellstorm` invariant 1 now includes `date`, `power`, a refused `oc`, and a hold/release
  every eighth line; it still requires the heap watermark to move by zero bytes over 256 commands.
* Live: `scripts/console-e2e.sh` and `scripts/console-fuzz-e2e.sh` (1,500 hostile lines per CPU,
  drawn from the command table, so the new verbs are fuzzed) PASS on aarch64, riscv64, x86-64.

## Non-claims

* The operating points are the power contract's model. QEMU exposes no DVFS/HWP actuator and this
  wave writes no MSR, CPPC or clock register: `oc` moves the contract's point, which is what a
  hardware rung will actuate. Physical ratio/voltage overclocking is still hardware-qualified work.
* The temperature the governor reads is still the targets' stand-in constant; no thermal driver.
* One domain is registered per target today; `DOMAIN` exists so a machine with more needs no
  new syntax.
