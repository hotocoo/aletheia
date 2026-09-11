# ADR-085 — The remaining boot gap is self-verification, and it is not going to be deleted

**Status:** Accepted (2026-09-12)
**Requirements:** REQ-PERF-001 (advanced)
**Builds on:** ADR-082 (the boot clock is split), ADR-061 (the gate counts itself),
ADR-056 (the honesty rule).

## Context

ADR-082 split the boot clock and left a specific, unexplained number: ~1077 ms of **kernel share**,
against Linux's ~1786 ms. Aletheia wins that comparison, and still loses the boot *total* because it
pays for UEFI.

An unattributed number is a number waiting to be misused. "Our kernel boots in a second" invites
exactly one follow-up — *doing what?* — and until this wave nothing here could answer it.

## Decision

`scripts/boot-profile.sh` boots the same interactive image the comparative benchmark measures, on
the same machine configuration flag for flag, and timestamps every serial line as it arrives on the
host. It then reports the largest gaps between consecutive lines.

**Host-side timestamps, deliberately.** The kernel could timestamp itself, but then the profile
would depend on a clock calibrated during the very window being measured, and the instrumentation
would change the thing it measures. Stamping arrival on the host needs no kernel change, so the
binary profiled is the binary shipped.

**A gap is evidence, not attribution.** A gap is wall-clock between one line appearing and the next.
It shows that work happened *between* those prints. It does not show the work belongs to either
line, and it includes serial transmission of the line that closes it. The script says so in its own
output.

Two bugs in the profiler had to be fixed before it told the truth, and both were the same species
as the Redox skip in ADR-084 — a harness failing and blaming the kernel:

* A line-oriented reader blocks forever on the shell prompt, because a prompt has **no trailing
  newline**. It reported "the image never reached a prompt" for a boot that reached one in 2414 ms.
* A chunk-oriented reader sees the prompt but shreds every line into fragments, making "which print
  did this gap follow" meaningless.

It now stamps whole lines when they complete, and watches the residual tail separately for the
prompt.

## Result (`docs/evidence/perf002`)

```
total to prompt: 2407 ms   firmware share: 1360 ms   kernel share: 1047 ms
```

The largest gaps after `ExitBootServices`:

| gap | after |
|---|---|
| 146.8 ms | `[mlsched] ALL 12 LIVE-ADVISORY INVARIANTS HOLD` |
| 144.7 ms | `[mlrisk] ALL 22 RISK-ADVISOR INVARIANTS HOLD` |
| 92.1 ms | mlrisk-stress rate-arrival abstention check |
| 89.7 ms | SMP selftests (MADT + INIT-SIPI-SIPI + cross-core) |
| 65.1 ms | benchmark selftests |
| 64.1 ms | benchmark gating |
| 59.1 ms | ring-3 live-advisory check |
| 53.7 ms | `calling ExitBootServices` (the first kernel work) |
| 50.4 ms | soak selftests |
| 40.0 ms | waiting for timer IRQs |
| 39.4 ms | the resident governor under load |

Reported gaps total 879.7 ms of the kernel share. **735.6 ms of that — 84% of the attributed time,
about 70% of the whole kernel share — is Aletheia running its own invariant suites.**

## Consequences

* **The boot comparison was never like-for-like, in a second way.** ADR-082 found the first
  asymmetry (Aletheia pays for UEFI, the `-kernel`-loaded Linux leg does not). This is the second:
  Aletheia proves ~470 invariants on every boot and Linux proves none. Linux does not run its test
  suite at boot; this kernel does, by design (ADR-061).
* **The suites are not going to be deleted to win a column.** A build that skips its own
  verification is not this operating system — ADR-061's whole position is that the gate counts
  itself on every boot, on every target. Stripping it to produce a faster number would be precisely
  the move this repository's honesty rule exists to forbid, and the resulting number would describe
  software nobody ships.
* **So the honest statement has three parts, and all three are required.** Aletheia's kernel share
  is ~1047-1077 ms against Linux's ~1786 ms; roughly 70% of Aletheia's is self-verification Linux
  does not perform; and Aletheia still loses the boot *total* because of firmware. Anyone quoting
  the first without the second and third is quoting dishonestly.
* **This wave optimized nothing.** No boot path changed. The same binary was attributed, not
  improved. If a real optimization target exists it is now visible — the two ML advisor suites cost
  ~291 ms between them — but shaving them means making verification faster, not making it optional.
* **Named non-claims.** The profile is one boot, on one host, under TCG. Gaps include serial
  transmission. Nothing here says anything about any operating system other than the two already on
  the bench.
