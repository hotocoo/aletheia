# ADR-260 — Block completions wake the CPU (x86-64)

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-STOR-007 (new)
**Builds on:** ADR-252 (requests in flight; its "what this is not" named this rung and its
constraint), ADR-131 (virtio-pci MSI-X on x86-64), ADR-150 (completion wait bounded on the clock),
ADR-168 (a waiting console costs nothing), ADR-180 (check-then-sleep).

## Context

Every driver spun on its completion. ADR-252 recorded the constraint on fixing that: the x86-64
console quiesces the PIT, so a CPU that halts waiting for a block completion has no wake source if
the device never answers, and ADR-150's clock bound could not fire.

## Decision

* **A platform completion wait in the shared driver.** `VirtioBlk::set_completion_wait(Some(f))`
  hands each look at the used ring to `f(ready)`; `None` (the default) spins as before, so every
  boot suite, both IOMMU suites and the fuzz sweep are unchanged. The clock bound is checked on
  every look either way. Batches completed are counted by how they were waited for.
  `bind_queue_interrupt(entry)` points the request queue at an MSI-X table entry and requires the
  device's read-back.
* **x86-64 console**: before the persistent device mounts, `arm_blk_interrupt` enables the BSP's
  xAPIC as an MSI target, programs the function's MSI-X entry 0 to vector 0x52, binds queue 0 to it,
  and installs `blk_wait`. Any refusal leaves the device polled and says so.
* **`blk_wait`**: with interrupts off (the boot path, the mount before `conirq::init`) it spins.
  Otherwise it clears IF, checks `ready` (a completion already in the ring is never slept on), arms
  a LAPIC one-shot (vector 0x53, about 17 ms under QEMU) and `sti; hlt`s; on wake it disarms the
  one-shot. The one-shot is the wake that keeps ADR-150's bound alive when the device is dead, and
  it exists only while a request waits, so an idle console still costs nothing.
* **Handlers** count and EOI only (ADR-080): the foreground reads the used ring itself.
* `lsblk` reports `completions: interrupt-driven, N interrupts, M sleeps, K bound wakes` (or
  `polled`).

## Evidence

* `console-e2e.sh`, x86-64 second boot (cold cache: `cat`, `grep`, the file reads after login):
  `completions: interrupt-driven, 7 interrupts, 4 sleeps, 0 bound wakes`; the gate now asserts
  sleeps > 0 with zero bound wakes, i.e. every sleep was ended by the device's completion.
* Idle (QEMU CPU over 20 s at the prompt, two runs): 0.00 % with the interrupt-driven wait armed,
  as before.
* Host: `completions_are_counted_by_how_they_were_waited_for`,
  `a_silent_device_still_times_out_through_the_platform_wait` (the wait is asked and the clock
  bound still fires); the virtio-blk suite and fuzz sweep unchanged.
* Boot gates on all three CPUs, `keyboard-e2e`, `vinput-e2e`, `usbkbd-e2e` and the quality gate
  pass (STATUS, this date).

## What this is not

* x86-64 only. virtio-mmio's interrupt line on aarch64 (GIC SPI) and riscv64 (PLIC) is the next
  step; the `CompletionWait` seam is already shared.
* NVMe and AHCI still poll; writes still go one block per request (the journal's design).
* One caller's wait becoming interrupt-driven, not an I/O scheduler: nothing yet queues independent
  requests from different callers.

## Addendum 2026-10-23: a bound wake is judged by the waiter, and the bound is longer

`console-e2e.sh` went red on a CI run with `90 interrupts, 94 sleeps, 6 bound wakes` on x86-64. Two
causes, both fixed in `kernel-x86_64/src/shellio.rs`:

* **The count.** The LAPIC one-shot's handler counted every expiry, including one that fires in the
  window after a completion interrupt has already woken the CPU but before `blk_wait` disarms it.
  `blk_wait` now disables interrupts, disarms, and counts a bound wake only when the one-shot fired
  during that sleep and the device did NOT interrupt (`BLK_TIMER_ENDED`), which is the
  dead-interrupt signature the gate exists to catch.
* **The bound.** `1 << 20` LAPIC ticks (about 17 ms under QEMU) was shorter than a loaded runner's
  slowest emulated read, so a live completion could lose the race to the timer. The bound only
  costs anything when the interrupt is dead; it is now `1 << 23` (about 134 ms).

A dead interrupt still fails the gate: every sleep then ends by the one-shot with no device
interrupt, and the readout shows those bound wakes.
