# ADR-219 — The machine keeps its settings

**Status:** Accepted (2026-10-07)
**Requirements:** REQ-CON-011 (new)
**Builds on:** ADR-196 (runtime resolution), ADR-080/085 (the desktop's timer pump).

## Context

The desktop redrew and polled its devices at 1 kHz on every target because three constants said
so, and a resolution the operator chose with `resolution WxH` was forgotten at the next boot. The
operator's goal for this system is that what a person can sensibly choose is not compiled in.

## Decision

* **`kernel_core::settings`** holds the machine's settings: the desktop's pump rate (an atomic,
  30..=1000 Hz, 1000 by default) and the format of the namespace object `settings`, one
  `key=value` per line (`resolution=WxH`, `refresh=HZ`). A line that is not understood - an unknown
  key, a bad value, bytes that are not UTF-8 - is skipped and counted, never fatal: a settings file
  from a newer machine, or edited by hand, must not stop the console starting.
* **`refresh [HZ]`** at the console shows the rate, or sets it and keeps it. Each target arms its
  desktop tick from the rate: aarch64 divides the generic timer's frequency by it, riscv64 the
  10 MHz timebase, x86-64 reprograms the PIT (`pit::set_rate`, which `pit::init` then keeps). A
  machine with no live desktop refuses by name. The hosted planner classifies it as destructive.
* **`resolution WxH`** now also keeps what it set.
* **The console puts the settings back when it starts**, before its first prompt, and names each
  one it applied or could not.

## Proof

* Host: settings round-trip, bad lines skipped and counted, the rate clamped
  (`kernel-core/src/settings.rs`); `refresh 120` is set, an out-of-range rate refused, the object
  written, and the next console start applies it before the first prompt
  (`kernel-core/tests/shell.rs`).
* Live, every target (`scripts/console-e2e.sh`): aarch64 and riscv64 set `refresh 120`, and the
  second boot of the same disk prints `settings: refresh 120 Hz` and reports 120 Hz; the x86-64
  console, booted there without a GPU, refuses it by name.

## Non-claims

* The refresh is the desktop's compose/poll rate; the emulated displays have no scanout timing of
  their own to change.
* x86-64 still chooses its mode at boot only (ADR-196), so a kept resolution is refused there.
* Two settings so far; the format and the start-up path are where the next ones go.
