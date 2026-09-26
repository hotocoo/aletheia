# ADR-208 — The comparisons, re-measured

**Status:** Accepted (2026-09-27)
**Requirements:** REQ-PERF-006 (new)
**Supersedes:** ADR-179's x86-64 round-trip figure (22.1 us).

## Context

ADR-200 showed that x86-64's timings before it were taken with a TSC calibrated to within about
40 % and stopped while idle. `docs/BENCHMARKS.md` still compared Aletheia's IPC with Linux's using
the old figure, and `docs/BOOT-COST.md` predated ADR-204..207. Separately, ADR-202, ADR-204 and
ADR-207 had put three storms inside `usermode::selftest`, a contract suite the interactive image
runs too, against ADR-163's rule that an interactive boot pays for contracts and not for storms.

## Decision

* `docs/BENCHMARKS.md` gains a dated section 0 with today's numbers from
  `scripts/comparative-bench.sh` (Aletheia against Linux 6.12-lts on the same host, emulator and
  machine; Redox and FreeBSD not re-run) and the HPET-timed IPC round trip. Older sections stay as
  measured; section 0 names where it overrides them. The Linux pipe figure (41.1 us, the Linux
  guest's own clock) stands.
* The three run storms are gated `STORMS = !cfg!(feature = "interactive")` in each `usermode.rs`:
  the gate image's `usermode` counts (49/49/57) are unchanged; the interactive image runs 46/46/54.
* `docs/BOOT-COST.md` is regenerated from today's gate and interactive logs.

## Numbers (2026-09-27, TCG on the development host)

* Boot to prompt: Aletheia 2210 ms (1371 ms of it OVMF, 839 ms the kernel) against Linux 1774 ms
  (loaded directly with `-kernel`).
* Idle host CPU at the prompt: Aletheia 0.0 %, Linux 2.0 %.
* 12 typed `echo` round trips: Aletheia 3 ms/op, Linux 29 ms/op (Aletheia's dispatcher is in the
  kernel, busybox `sh` in user space: a design difference the column prices, not a control).
* Cross-address-space IPC round trip: 27.4-29.9 us (x86-64), 30.0 us (aarch64), 43.9 us (riscv64).
* Interactive `usermode` lap after gating: 154 ms (aarch64), 133 ms (riscv64), 179 ms (x86-64).

## Non-claims

* Every number is QEMU TCG on one workstation; none is a hardware claim.
* The x86-64 in-kernel operation costs in section 3 of BENCHMARKS.md were not all re-run; section
  0 lists today's.
