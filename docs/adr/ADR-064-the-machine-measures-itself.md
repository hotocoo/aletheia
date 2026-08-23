# ADR-064: The machine measures itself

**Status:** Accepted · **Date:** 2026-08-23 · **Closes:** ALET-P2-010 · **Builds on:** ADR-056 (reported-not-gated timing; the same-substrate rule), ADR-061 (families join gate maps deliberately), ADR-063 (the Hal clock seam and the steady-state doctrine)

## Context

The repo could say how fast its subsystems run on ONE target, once, ad hoc: `kernel/src/bench.rs`
measured svc traps and capability-checked delivery on aarch64 only, with CNTVCT inline asm, no
invariants, and no presence in any other target's boot. Meanwhile the operator's first question
about a new machine — "how fast are the load-bearing paths ON THIS SUBSTRATE?" — had no
arch-independent answer, and `comparative-bench.sh` compared only how the two systems WAIT
(boot-to-prompt, idle host CPU), never how they ANSWER under a typed workload.

Two honesty constraints shaped everything:

* QEMU-TCG timing is an EMULATOR's number. Any throughput figure that can fail a gate is a figure
  that will fail on a busy host for reasons that have nothing to do with the kernel.
* An integer clock can be COARSER than one operation (x86-64 TSC vs one console line). Printing
  the integer division of that is printing a confident 0 — lying in the other direction.

## Decision

`kernel-core/src/bench.rs` measures five load-bearing paths through the shared `Hal` seam, once,
on every target, inside every VM gate:

authority checks (`CapEngine::evaluate`) · capability-checked delivery round-trips · journal
commits · scheduler dispatches · console line formatting.

**What is REPORTED:** per-path throughput and unit cost, in ns on calibrated clocks (aarch64
CNTFRQ, riscv64 SBI timebase) and in raw ticks labelled "uncalibrated" on x86-64 (TSC has no
known frequency here) — where a cost rounds below one tick it prints `<1` rather than 0.

**What is GATED** (twelve checks, marker family `bench=12`): the clock advanced in every window;
every authority check allowed (authority cannot change mid-measurement); every delivery delivered,
summed to a closed form; every commit read back byte-for-byte after the window; an equal-size
rerun window materialized ZERO device blocks (steady state, not setup — the trap ADR-063's
invariant 1 caught); dispatch was EXACTLY fair across the pool; console bytes equal the arithmetic
and the last line re-encodes identically outside the meter; a second campaign performs IDENTICAL
work (counter census); and four pixel-level GUI checks — glyph-exact blitting against the embedded
font table, wrap, scroll, and THIS boot's own summary legible ON THE DISPLAY over real framebuffer
frames.

**Both consoles, by construction:** the metric lines go to serial (the TUI the gates judge) and
through `fbcon` onto page-backed surfaces (the GUI a human sees), with the render itself proved
at pixel level. The hosted suite (`tests/bench.rs`) adds what hosts prove better: a frozen clock
is refused BY NAME, an uncalibrated clock reports ticks and never nanoseconds, every line fits an
80-cell display unwrapped, and a counting global allocator shows the hot loops allocate nothing
per operation at 400 000 measured operations.

**The cross-OS half:** `comparative-bench.sh` gains a typed-workload leg — after idle sampling,
still alive, each guest receives byte-identical paced `echo` round-trips over serial, each held
until that op's unique token returns as OUTPUT (anchored so input echo cannot satisfy it),
wall-clocked end-to-end. Same emulator, same pacing, same judge. Redox stays opt-in and skips this
leg only because its image boots to a login whose credentials this script will not guess.

## Consequences

* Every future kernel path worth arguing about joins ONE shared harness instead of sprouting
  per-target microbenchmarks; the gate maps make disappearance or drift a named failure.
* The x86-64 numbers stay tick-denominated until someone calibrates the TSC (CPUID 0x15/0x16 or
  PIT cross-check); the label says so on every line and in every doc that quotes it.
* The workload leg prices in a DESIGN asymmetry (Aletheia's dispatcher is in-kernel; busybox sh
  is user-space over syscalls) and says so beside the number — comparative data, not advertising.