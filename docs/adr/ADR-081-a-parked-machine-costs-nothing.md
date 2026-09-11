# ADR-081 — A parked machine costs nothing: stop the counter, don't just mask it

**Status:** Accepted (2026-09-12)
**Requirements:** REQ-PERF-001 (advanced), REQ-CON-006 (advanced)
**Builds on:** ADR-056 (the honesty rule), ADR-049 (the console's second input source),
ADR-064 (the machine measures itself).

## Context

`scripts/comparative-bench.sh` exists to make "faster than Linux" a measurable statement rather
than an adjective: both systems boot under the same `qemu-system-x86_64`, on the same host, in the
same TCG mode, with the same `-machine`/`-m`/`-smp`/`-cpu`, to the same end state.

Run honestly, it said Aletheia **lost** the idle column: 0.5% host CPU at the prompt against
Linux's 0.1-0.4%. That is one of the two genuinely fair columns in the whole table — both guests
doing nothing, on the same emulator — so there was nowhere to put the loss except on this kernel.

Two things were wrong. One was the instrument. One was the kernel.

## The instrument was wrong first

`boot_and_measure` polled for the prompt marker with `sleep 1`. A one-second poll on a
two-to-three-second measurement means every boot time is rounded up toward the next poll, and a gap
between two legs can be mostly the sleep. The same binaries measure 2487 ms against 1773 ms at a
5 ms poll where the coarse poll reported 3065 against 2044: both legs were overstated, and the
reported gap shrank from ~1.02 s to ~0.71 s.

A benchmark whose ruler has one-second graduations should not be reporting three-digit millisecond
differences, and this one had been. Numbers taken before the fix are not comparable to numbers
taken after it, and `docs/evidence/perf001` says so rather than quietly restating the new ones.

The Linux leg was also not running at all on this machine: it required Docker purely to obtain a
static busybox, so a host with no container daemon SKIPped the comparison and left the claim
unmeasured. It now builds the same initramfs from Alpine's minirootfs with the host's
`curl`/`tar`/`cpio`/`gzip`, carrying the musl loader and libc alongside the dynamically linked
busybox. Same guest, same end state, same measurement.

## Then the kernel was wrong

When the console comes up, `conirq::init` masks IRQ0 at the 8259A. That stops the interrupt being
**delivered**. It does not stop the 8254 from **counting**.

The emulator goes on modelling a device that ticks a hundred times a second. A host emulating a
counter for a guest that is asleep is a host burning CPU on behalf of nothing, and it is precisely
what the idle column measures. Masking answered "does the kernel get woken up"; the column was
asking "does the machine cost anything".

## Decision

`pit::quiesce()` reprograms channel 0 to **mode 0** — interrupt on terminal count — with a full
count. Mode 0 does not reload: the counter runs down once and then sits there. This is not a slower
tick. It is the last tick.

`conirq::init` calls it immediately after masking, because the console is the state this machine
waits in and waiting should cost nothing. Anything that needs a periodic timer again calls
`pit::init`, which reprograms mode 3 from scratch; the function is `#[cfg(feature = "interactive")]`
because the non-interactive image never parks at a prompt.

## Result

Idle host CPU at the prompt, measured across three independent runs:

| | before | after |
|---|---|---|
| Aletheia | 0.5 % | **0.0 %** |
| Linux 6.12-lts | 0.2-0.6 % | 0.2-0.3 % |

The column flipped, and it flipped because a real periodic cost was found and removed, not because
a measurement was chosen differently.

The full table after both fixes (`docs/evidence/perf001`, three runs):

| Column | Aletheia | Linux 6.12-lts | Winner |
|---|---|---|---|
| boot to a prompt (total) | 2507-2516 ms | 1780-1790 ms | Linux, by ~0.72 s |
| idle host CPU at prompt | 0.0 % | 0.2-0.3 % | **Aletheia** |
| bootable payload | 1,439,744 B | 14,163,373 B | **Aletheia**, 9.8x |
| typed echo round-trip | 64-69 ms | 751-783 ms | **Aletheia**, ~11.3x |
| privileged lines of code | 42,078 Rust | ~40M C (cited) | **Aletheia**, ~950x |

## Consequences

* **Named non-claims.** Four columns of five is not "Aletheia beats Linux". Boot time is still
  lost by ~0.71 s, and Aletheia boots through OVMF while the Linux leg is `-kernel`-loaded and
  skips firmware entirely — splitting Aletheia's total into a firmware share and a kernel share is
  the obvious next piece of work and **has not been done**, so no part of that gap is currently
  excused. The round-trip win carries a kernel-space/user-space asymmetry stated beside it. The
  payload win is mostly a size difference.
  *(Superseded in part by ADR-082, which measured the split rather than leaving it as prose: OVMF
  costs ~1429 ms and this kernel's own share is ~1082 ms against Linux's ~1786 ms. The TOTAL boot
  column is still lost, and is still reported as lost.)*
* **Nothing here measures security.** The idle column is a performance result. Capability and
  isolation claims live in the invariant suites and their ADRs.
* **No other operating system is measured.** Windows, macOS, the BSDs and every RTOS are absent
  from this table, and the Redox leg is opt-in and was skipped.
* **The unsafe surface grew by one** (`kernel-x86_64`: 258 → 259), a control-word and count write
  to the 8254, recorded in `docs/UNSAFE-AUDIT.md`.
* **Pre-fix benchmark numbers are retired**, not reconciled. `docs/evidence/perf001` carries only
  post-fix runs and states what changed in the ruler.
