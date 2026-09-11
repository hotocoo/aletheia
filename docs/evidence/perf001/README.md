# REQ-PERF-001 — Aletheia against a real Linux kernel, measured

**Date:** 2026-09-12
**Harness:** `scripts/comparative-bench.sh`
**Raw runs:** `comparative-bench-2026-09-12.txt`, `run-2.txt`, `run-3.txt` — three independent
executions, complete output, nothing trimmed.

Both systems boot under the same `qemu-system-x86_64` on the same host, in the same TCG emulation
mode, with the same `-machine q35 -m 256 -smp 4 -cpu qemu64`, to the same end state: an interactive
shell on ttyS0 waiting for input. Every number below is measured across that line.

## What the three runs said

| Column | Aletheia (x86-64) | Linux 6.12-lts | Verdict |
|---|---|---|---|
| boot to a prompt | 3070 / 3065 / 3066 ms | 2046 / 2051 / 2044 ms | **Linux, by ~1.0 s, consistently** |
| idle host CPU at prompt | 0.5 / 0.5 / 0.5 % | 0.3 / 0.4 / 0.3 % | **Linux, by ~0.15 pp** |
| bootable payload | 1,439,744 B | 14,163,372 B | **Aletheia, 9.8x smaller** |
| typed echo round-trip | 69 / 66 / 65 ms | 719 / 749 / 746 ms | **Aletheia, ~11x faster** |
| privileged lines of code | 42,053 (Rust, counted) | ~40M (C, cited) | **Aletheia, ~950x less** |

Three runs, tight spreads, same direction every time. These are not one-sample readings.

## What each result does and does not mean

**Boot time — Linux wins, and the margin is partly structural.** Aletheia boots through OVMF, a
full UEFI firmware implementation; the Linux leg is loaded directly by QEMU's `-kernel` and skips
firmware entirely. That is a boot-*path* difference, not evidence about either kernel's speed. The
honest statement is: Linux reaches a prompt first here, and roughly a second of Aletheia's time is
firmware the other leg never runs. Closing it means measuring Aletheia without OVMF, which is a
separate piece of work, not a footnote.

**Idle CPU — Linux wins narrowly.** Both guests are doing nothing on the same emulator, so this is
one of the two genuinely fair columns. 0.5% against 0.3-0.4% is a real difference and it is small.
Aletheia's console parks on the interrupt rather than spinning (REQ-CON-006); Linux still waits
slightly better. No excuse is offered.

**Typed echo round-trip — Aletheia wins by roughly 11x, with a stated asymmetry.** N `echo`
round-trips typed into each guest's shell, wall-clocked end to end, exercising the whole
interactive path: input ring or tty discipline, line editor, dispatcher, output formatting, serial
transmission. It is also a small stress test — a dropped keystroke hangs the leg and fails it
rather than passing quietly. **The asymmetry, priced in rather than hidden:** Aletheia's dispatcher
runs in kernel space while busybox `sh` runs in user space over syscalls. That is a design
difference between the two systems, not a controlled variable, and part of the 11x is it.

**Payload size — Aletheia wins, and it is mostly not a design victory.** Linux ships drivers for
hardware Aletheia has never heard of. Reported because it is true and measured, not because it is a
fair fight.

**Privileged lines of code — the only column that does not depend on an emulator, a host, or a
workload.** 42,053 counted Rust lines against a cited ~40M C lines. This is where the design, rather
than the youth, is doing the work: no ambient authority, a capability check on the syscall path, a
`no_std` core with no allocator in the boot path.

## What this is NOT evidence for

* **Not "Aletheia beats Linux."** It wins three columns of five, loses two, and one of its wins is
  mostly a size difference. Anyone quoting the 11x without the kernel-space/user-space asymmetry
  beside it is quoting it dishonestly.
* **Not throughput, under any load that matters.** No scheduler tuning, no page cache, no SMP work
  stealing, one flat filesystem namespace on one block device.
* **Not hardware.** Linux boots on the machine you own. Aletheia boots on three emulated boards.
* **Not a claim about any other operating system.** Windows, macOS, the BSDs and every RTOS are
  unmeasured here. The Redox leg is opt-in (`WITH_REDOX=1`) and was skipped in these runs.
* **Not production readiness.** `docs/MATURITY.md` grades every subsystem and says plainly that
  nothing here is production-ready.

## Reproducing it

```sh
./scripts/comparative-bench.sh
```

The Linux leg needs network access. It no longer needs Docker: when no container daemon is present
the harness builds the same busybox initramfs from Alpine's minirootfs tarball using the host's
`curl`/`tar`/`cpio`/`gzip`, carrying the musl loader and libc alongside the (dynamically linked)
busybox. The guest ends in the same state either way, so the measurement is unchanged; only the way
the bytes were assembled differs. Either path SKIPs loudly and never passes silently.
