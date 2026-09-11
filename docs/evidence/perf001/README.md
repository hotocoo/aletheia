# REQ-PERF-001 — Aletheia against a real Linux kernel, measured

**Date:** 2026-09-12
**Harness:** `scripts/comparative-bench.sh`
**Raw runs:** `run-1.txt`, `run-2.txt`, `run-3.txt` — three independent executions, complete output,
nothing trimmed.

Both systems boot under the same `qemu-system-x86_64` on the same host, in the same TCG emulation
mode, with the same `-machine q35 -m 256 -smp 4 -cpu qemu64`, to the same end state: an interactive
shell on ttyS0 waiting for input. Every number below is measured across that line.

## What the three runs said

| Column | Aletheia (x86-64) | Linux 6.12-lts | Verdict |
|---|---|---|---|
| boot to a prompt (total) | 2507 / 2516 / 2509 ms | 1790 / 1780 / 1786 ms | **Linux, by ~0.72 s** |
| — of which firmware (OVMF) | 1431 / 1429 / 1427 ms | none (`-kernel`) | — |
| — of which this kernel | 1076 / 1087 / 1082 ms | 1790 / 1780 / 1786 ms | **Aletheia, ~1.65x** |
| idle host CPU at prompt | 0.0 / 0.0 / 0.0 % | 0.3 / 0.2 / 0.3 % | **Aletheia** |
| bootable payload | 1,439,744 B | 14,163,372 B | **Aletheia, 9.8x smaller** |
| typed echo round-trip | 69 / 64 / 69 ms | 751 / 774 / 783 ms | **Aletheia, ~11.3x faster** |
| privileged lines of code | 42,078 (Rust, counted) | ~40M (C, cited) | **Aletheia, ~950x less** |

Four columns of five to Aletheia, one to Linux — and the one Aletheia loses splits: it loses the
TOTAL because it boots through UEFI firmware, and wins the share each project actually wrote. Tight spreads, same direction every run.

## Two things changed in the instrument, and both mattered

**The boot clock had one-second resolution.** `boot_and_measure` polled for the prompt marker with
`sleep 1`, which put a full second of quantization on a two-to-three second measurement — every
boot time was rounded up toward the next poll, and a gap between two legs could be mostly the
sleep. At 5 ms the same binaries measure 2487 ms against 1773 ms, where the 1-second poll had
reported 3065 against 2044. Both legs were overstated; the reported gap shrank from ~1.02 s to
~0.71 s. This was a defect in the ruler, and the numbers before it are not comparable to the
numbers after it.

**The Linux leg required Docker and was therefore not running at all** on machines without a
container daemon. It now builds the same busybox initramfs from Alpine's minirootfs with the host's
`curl`/`tar`/`cpio`/`gzip`, carrying the musl loader and libc alongside the dynamically linked
busybox. Same guest, same end state, same measurement; only the assembly differs.

## The idle column was lost, and then it was fixed

The first honest run of this harness put Aletheia at 0.5% idle host CPU against Linux's 0.1-0.4%.
That is one of the two genuinely fair columns — both guests doing nothing, same emulator — so it
was a real loss, and the cause turned out to be real too.

Aletheia masked IRQ0 at the PIC when the console came up, which stops the interrupt being
*delivered*. It does not stop the 8254 from counting. The emulator went on modelling a device
ticking 100 times a second, and a host emulating a counter is a host burning CPU for a guest that
is asleep.

`pit::quiesce()` now reprograms channel 0 to mode 0 — interrupt on terminal count, which does not
reload — so the counter runs down once and stops. Not a slower tick: the last tick. Idle host CPU
went from 0.5% to **0.0%**, measured across three runs against Linux's 0.2-0.3%, and the
column flipped.

## What each result does and does not mean

**Boot time — Linux wins the total; Aletheia wins the part it wrote.** Aletheia boots through
OVMF, a full UEFI firmware implementation; the Linux leg is loaded directly by QEMU's `-kernel` and
skips firmware entirely. That caveat used to sit in prose, excusing a loss without measuring it.
It is now measured (ADR-082): the harness timestamps `calling ExitBootServices` as well as the
prompt, so one boot yields both shares.

OVMF costs ~1429 ms. Aletheia's own kernel reaches an interactive prompt in ~1082 ms against
Linux's ~1786 ms, about **1.65x faster** — while still losing the total by ~0.72 s, because a
machine that boots through firmware takes longer to reach a prompt than one handed the CPU, and
that is the honest end-to-end experience.

Both statements are true and neither replaces the other. **Named non-claim on the split:** the two
kernel shares are far closer to like-for-like than the totals were, but they are not identical
work — Linux's 1786 ms includes QEMU loading and decompressing a 14.16 MB kernel-plus-initramfs
payload, while Aletheia's 1082 ms starts from a 1.44 MB image firmware has already placed. Part of
the difference is the payload difference, priced separately in its own row. Anyone quoting the
1.65x owes the reader that sentence. And nothing here optimized a boot path: the same binary was
measured more carefully.

**Idle CPU — Aletheia wins, and this one is a design result.** Both guests are doing nothing on the
same emulator. The win came from finding and removing a real periodic cost, not from a
measurement choice.

**Typed echo round-trip — Aletheia wins ~11.3x, with a stated asymmetry.** N `echo` round-trips
typed into each guest's shell, wall-clocked end to end, exercising the whole interactive path:
input ring or tty discipline, line editor, dispatcher, output formatting, serial transmission. Also
a small stress test — a dropped keystroke hangs the leg and fails it rather than passing quietly.
**The asymmetry, priced in rather than hidden:** Aletheia's dispatcher runs in kernel space while
busybox `sh` runs in user space over syscalls. That is a design difference between the two systems,
not a controlled variable, and part of the 11.3x is it.

**Payload size — Aletheia wins, and it is mostly not a design victory.** Linux ships drivers for
hardware Aletheia has never heard of. Reported because it is true and measured, not because it is a
fair fight.

**Privileged lines of code — the only column that does not depend on an emulator, a host, or a
workload.** 42,078 counted Rust lines against a cited ~40M C lines.

## What this is NOT evidence for

* **Not "Aletheia beats Linux."** It wins four columns of five and loses total boot time. One of its wins
  is mostly a size difference, and another carries a kernel-space/user-space asymmetry that anyone
  quoting the 11.3x without it is quoting dishonestly.
* **Not throughput, under any load that matters.** No scheduler tuning, no page cache, no SMP work
  stealing, one flat filesystem namespace on one block device.
* **Not hardware.** Linux boots on the machine you own. Aletheia boots on three emulated boards.
* **Not a claim about any other operating system.** Windows, macOS, the BSDs and every RTOS are
  unmeasured here. The Redox leg is opt-in (`WITH_REDOX=1`) and was skipped in these runs.
* **Not security.** Nothing in this harness measures security. The capability and isolation claims
  live in the invariant suites and ADRs, not in this table.
* **Not production readiness.** `docs/MATURITY.md` grades every subsystem and says plainly that
  nothing here is production-ready.

## Reproducing it

```sh
./scripts/comparative-bench.sh
```

The Linux leg needs network access; it no longer needs Docker. Either path SKIPs loudly and never
passes silently.
