# ADR-084 — A second operating system: the skip was the harness, not Redox

**Status:** Accepted (2026-09-12)
**Requirements:** REQ-PERF-001 (advanced)
**Builds on:** ADR-082 (the boot clock is split), ADR-081 (a parked machine costs nothing),
ADR-056 (the honesty rule).

## Context

Every performance number in this repository had been measured against exactly one other operating
system. `scripts/comparative-bench.sh` had always carried a Redox OS leg, opt-in behind
`WITH_REDOX=1`, and on this host it had always printed:

```
  Redox did not reach a login prompt on this host — reported, not hidden.
```

"Reported, not hidden" is the right instinct and the wrong conclusion. That line describes the
harness's own inability to press a key, and it prints it as though it were a fact about somebody
else's operating system. Read at a glance it says "Redox does not boot here". What was actually
true is that Redox's bootloader draws a **video-mode picker** and waits on the **UEFI console**,
which under `-nographic` nobody can answer. The guest was blocked, not slow.

A benchmark that mislabels its own limitation as the competitor's failure is a worse defect than a
benchmark that is simply wrong, because it fails in the direction its author would prefer.

## Decision

Three fixes, all of them about the harness rather than about Redox.

1. **Give it firmware.** Redox ships a UEFI image, so it gets the same OVMF pflash the Aletheia leg
   does. This also makes its total directly comparable to Aletheia's total: both pay firmware,
   which the `-kernel`-loaded Linux leg does not.
2. **Answer the picker through the monitor.** `boot_and_measure` gained `MONITOR_SOCK` and
   `MONITOR_KEYS`; when set, a helper sends `sendkey ret` through the QEMU monitor until the boot
   marker appears. This drives **firmware**, never the measured system — every serial byte still
   comes from the same code path as the other legs, and the typed-workload leg still goes over the
   serial line like everyone else's.
3. **Exclude it from the typed-workload leg, and only that leg.** Redox boots to a login prompt.
   Its credentials are printed on its own console rather than guessed, but logging in would measure
   how well this script can drive somebody else's OS, not how well that OS answers. Boot and idle
   are measured exactly like everyone else's.

A latent harness bug surfaced on the way and is fixed: with `WORKLOAD_OPS=0`, `set -u` met an
unbound `WORKLOAD_MS` and killed the run mid-leg. Turning the workload leg off is now supported.

## Result (three runs, `docs/evidence/perf001`)

| Column | Aletheia | Linux 6.12-lts | Redox OS |
|---|---|---|---|
| boot to a prompt (total) | 2500-2517 ms | 1777-1817 ms | 11371-11438 ms |
| of which firmware (OVMF) | ~1433 ms | none (`-kernel`) | UEFI, not split |
| of which this kernel | ~1077 ms | 1777-1817 ms | — |
| idle host CPU at prompt | **0.0 %** | 0.3-0.7 % | 3.4 % |
| bootable payload | **1.44 MB** | 14.16 MB | 536.87 MB |
| typed echo round-trip | **65-66 ms** | 661-783 ms | n/a (login) |

**Redox is the fair boot comparison.** It boots through UEFI like Aletheia, so both pay firmware and
their totals are comparable in a way neither is with the `-kernel`-loaded Linux leg: ~2510 ms
against ~11412 ms, about **4.5x**.

Aletheia wins every measured column against Redox, including the boot total it loses to Linux.

## Consequences

* **Two operating systems is not "every other OS".** Windows, macOS, the BSDs and every RTOS remain
  entirely unmeasured, and no claim is made about them.
* **Beating Redox is not beating a production kernel.** Redox is a young research OS, as this one
  is. The Linux leg is still the one that matters, and Aletheia still loses its boot total.
* **Redox's numbers are its SERVER image with default services**, including a network stack that
  logs failures on this host (`no network adapter found`) and daemons Aletheia simply does not
  have. Its 3.4% idle and 536 MB image are measurements of that image, not of a minimal Redox.
  This is the same "not the same product" caveat the Linux leg carries, and it applies in Redox's
  favour here.
* **The split row is blank for Redox.** Its firmware share was not separated, because it prints no
  marker at the moment it takes the machine and inventing one would mean patching somebody else's
  OS to win a row.
* **Nothing here measures security.** ADR-083 measures attack *surface*, which is a different
  thing, and it measures it only against Linux.
