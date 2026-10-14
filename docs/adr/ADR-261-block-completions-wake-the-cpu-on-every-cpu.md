# ADR-261 — Block completions wake the CPU on aarch64 and riscv64

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-STOR-008 (new)
**Builds on:** ADR-260 (the `CompletionWait` seam, x86-64 over MSI-X), ADR-045 (the console's GIC
and PLIC paths), ADR-085 (the timer under the console), ADR-180 (check-then-sleep).

## Context

ADR-260 put the x86-64 console's block reads to sleep until the device's MSI-X completion. The
seam was shared; aarch64 and riscv64 still spun. Their persistent device is virtio-mmio, whose one
interrupt line is level-triggered and must be acknowledged at the device (INTERRUPT_STATUS /
INTERRUPT_ACK) as well as at the interrupt controller.

## Decision

* **aarch64**: `conirq::arm_blk(base)` maps the transport to its GIC INTID (QEMU virt wires slot
  `n` to SPI 16 + n, INTID 48 + n) and `init` routes it to CPU 0 at the console's priority. The
  handler reads INTERRUPT_STATUS, writes it to INTERRUPT_ACK, counts, and EOIs. `blk_wait` masks
  IRQs, checks `ready`, arms the EL1 physical timer as a one-shot unless a desktop or USB keyboard
  already keeps it running, and `wfi`s. The timer PPI is enabled whenever a block wait might need
  it, but ARMED only while something ticks or a request waits; a one-shot that fires with nothing
  ticking is disarmed and counted as a bound wake.
* **riscv64**: the same over the PLIC: slot `n` (0x1000_1000 + 0x1000 n) is source 1 + n; the
  handler claims, acknowledges at the device, counts, completes. The bound is an SBI timer set
  1/60 s ahead and cancelled (`set_timer(u64::MAX)`, which also clears the pending bit) when it
  fires with no desktop live. `sie.STIE` is enabled with the timer cancelled, so an idle console
  takes no timer interrupt.
* Both keep spinning while interrupts are off (boot, and the mount before `conirq::init`).

## Evidence

* `console-e2e.sh`, second boot, now asserted on all three CPUs:
  aarch64 `7 interrupts, 6 sleeps, 0 bound wakes`; riscv64 `6 interrupts, 6 sleeps, 0 bound
  wakes`; x86-64 `7 interrupts, 4 sleeps, 0 bound wakes`. Every sleep was ended by the device.
* Idle at the prompt (QEMU CPU over 20 s, two runs): aarch64 0.00 % with the wait armed.
* Boot gates on all three CPUs, `console-fuzz-e2e`, `desktop-e2e-dt`, `usbkbd-e2e`,
  `crash-e2e`, the quality gate and the kernel-core suite pass (STATUS, this date).

## What this is not

* Only the console's persistent virtio-blk; NVMe, AHCI and the scratch device still poll.
* The INTID and source are derived from QEMU virt's wiring, not read from the device tree's
  `interrupts` property; a board that wires virtio-mmio differently gets a polled device (the
  window check refuses a base outside QEMU's layout).
