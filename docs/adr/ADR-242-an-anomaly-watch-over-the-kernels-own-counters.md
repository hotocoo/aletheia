# ADR-242 — An anomaly watch over the kernel's own counters

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-AI-017 (row 5 of ADR-237's wave order)
**Builds on:** ADR-042/202 (supervisor), ADR-081 (memory boundary), ADR-238 (reclaim), ADR-056 (advisory posture).

## Context

The kernel counts what goes wrong (faults contained, faults escalated, refused admissions,
pressure entries, programs reclaimed) and prints the totals when asked. Nothing watched how fast
they move, so ten faults in one second and ten faults over an hour read the same. ADR-237 listed
runtime anomaly detection as the fifth System-1 row.

## Decision

`kernel_core::anomaly`: a rate detector, integer-only, allocation-free, one fixed state per
signal.

* **Intervals of one second.** The console's loop hands the watch the five counters on every
  pass, typed or idle; when a new second has begun the open interval is closed and each counter's
  increase judged. Seconds nobody observed are replayed as quiet ones (at most 64).
* **Two baselines per signal, both bias-corrected:** a fast exponentially weighted mean (1/8) that
  follows the machine, and a slow one (1/64) that remembers its level for about a minute. An
  increase is an anomaly when, after five intervals of warm-up, it exceeds both four times the
  larger mean and a floor of two events.
* **Why a rate multiple and not a deviation band:** the first version flagged above
  `mean + 4 x mean absolute deviation + 2`. On the live console it never fired: these are sparse
  event counts, and a deviation estimated from isolated single faults widened the band until a
  burst of three fit inside it. A version with only the fast mean then fired on steady noise after
  a short lull. Both failures are now boot invariants.
* **Advisory.** It counts and remembers the latest finding (signal, increase, interval, baseline);
  it never kills, throttles or refuses. `mlstat` prints it:
  `anomaly: N interval(s) watched, M flagged; last: faults contained +3 at 71 s (baseline 0.25 per s)`.

## Evidence

* Boot family `anomaly`, 7 invariants on every CPU: nothing flagged before warm-up; 600 intervals
  of steady 0–3 noise raise nothing; a burst after quiet is flagged once, on its own signal, in its
  own interval; isolated single events stay quiet and three in one interval after them are
  flagged; one event on a quiet counter is under the floor; a persisting rate is learned and stops
  being flagged; a reset counter is no increase and the same stream twice gives the same verdicts.
  Host: the suite, and a gap replayed as quiet lets a burst after five idle minutes stand out
  while the same burst inside a busy period does not. kernel-core release suite 937 passed.
* Live, console-e2e on all three CPUs: a whole scripted session (single faults from `run trap`,
  `together`, background programs, reclaim and pressure readings) flags nothing until
  `together trap trap trap`; then exactly one interval is flagged, `faults contained +3`, against
  a baseline of 0.25 (aarch64, riscv64) and 0.37 (x86-64) faults per second.
* Boot gates on all three CPUs pass with the new family.

## What this is not

It watches five counters, read where the console runs; a machine without a console session
does not feed it. Signals are not yet the drivers' error counters (virtio, NVMe, AHCI, e1000) or
the network's; adding a counter is one array entry. Nothing acts on a finding.

## Consequences

* ADR-237 row "Anomaly detection" is live and advisory. Acting on a finding (for example,
  lowering a faulting program's weight) is a policy decision for its own ADR.
