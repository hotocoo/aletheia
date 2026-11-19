# ADR-200 — The clock keeps time while the machine idles

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-DRV-008 (new)
**Supersedes:** the x86-64 HAL's "the TSC is the clock" assumption.

## Context

x86-64's `Hal::timer_ticks` was `rdtsc`, its frequency calibrated against two PIT interrupts. Two
defects, both measured on QEMU q35 / OVMF / `-cpu qemu64` under TCG:

* **The clock stopped while the machine idled.** Across fifteen seconds of real time with the
  console idle, the RTC (`date`) advanced 15 s and `uptime` advanced 0.47 s; `mlstat` read a machine
  clock of 3 s after a whole console session. Every x86-64 uptime, `boot` lap, `mlstat` timestamp
  and timeout that spans an idle wait inherits the error.
* **The calibration was noise.** Two PIT ticks carry up to one tick of error in two; three boots of
  one VM calibrated at 903, 1167 and 1292 MHz.

The ISA already says when the TSC is a clock across idle: CPUID.80000007H:EDX[8], invariant TSC.
`qemu64` does not set it. This ADR does not explain QEMU's mechanism; it applies the ISA's rule,
which is right for real pre-invariant hardware for the same reason.

## Decision

* `hal::select_clock()`, before the boot stopwatch starts (no measured interval spans a switch):
  * invariant TSC: the clock is the TSC, frequency from CPUID.15H, else measured over 50 ms of the
    HPET, else the old two-PIT-tick fallback;
  * otherwise, the HPET the firmware declares in ACPI (`HPET` table, base in system memory below
    4 GiB, 64-bit-capable counter, period in (0, 100 ns]), enabled if the firmware left it off;
  * neither: the TSC, and the boot line says uptime may stop while idle.
* One boot line names the choice: `[hal] clock: HPET (the TSC is not invariant)` on QEMU.
* `scripts/console-e2e.sh` gains an idle wait (`+N`: the driver waits, types nothing) and asserts on
  every CPU that across ten idle seconds `uptime` advances by at least 80% of what the RTC says.

## Proof

* Host: `kernel-core/tests/hpet.rs` - which ACPI declarations (`kernel_core::hpet::base_from_table`)
  and capability registers (`counter_hz`) make a clock, including the refusals.
* x86-64: fifteen idle seconds of RTC advance `uptime` by 15.03 s (was 0.47 s). HPET at 100 MHz.
* console-e2e, all three CPUs: RTC 12 s / uptime 12.04 s (aarch64), 12 s / 12.04 s (riscv64),
  10 s / 10.02 s (x86-64). aarch64 and riscv64 had never had their clock asserted either.
* All boot gates, conformance, keyboard, vinput and quality gates pass. `docs/BOOT-COST.md` is
  regenerated from today's logs, x86-64 now timed by the HPET.

## Non-claims

* The HPET path is proved on QEMU only. VMware and real hardware usually advertise invariant TSC and
  take the TSC path, whose HPET calibration is not yet exercised by any gate.
* A 32-bit-only HPET is refused rather than extended in software.
* The governor's tick count stays still at an x86-64 prompt without a desktop (its ticks come from
  ring-3 preemption); observed, not changed here.
* x86-64 numbers published before this ADR (ADR-162's harvest, ADR-179's IPC round trip) carry the
  old calibration's error; only the boot-cost page is re-measured here.

## Addendum 2026-11-19: the clock froze at 184 seconds

x86-64's `ticks_to_ns` was `ticks.saturating_mul(10^9) / hz`. With the 100 MHz HPET this ADR chose
under QEMU, `ticks * 10^9` passes `u64::MAX` at 184.47 s of uptime, and the saturating product then
returned the same value forever: `uptime` read `0h 03m 04s (184467440737 ns since boot)` twice twenty
seconds apart. Every wait bounded by `now_ns` (ADR-150) became unbounded after that moment.

That is the VirtualBox job's hang after e1000 invariant 5. VirtualBox has no HPET, and its TSC starts
near zero, so the product froze `2^64 / 10^9` ticks in: 14.6 s at the 1.26 GHz the failing run
(44460064031) calibrated. That run's clock sample (4.56 s) plus its suite laps before the e1000
family (8.44 s) put the echo-peer ARP at about 13 s, and that ARP is never answered on VirtualBox (no
peer at 10.0.2.100), so its 2 s budget, ending only on the clock, ran into the freeze. The passing run
on 095eee3 reached it at about 6.7 s against a 12.4 s freeze (1.48 GHz) and spent its 2.0 s. It does
NOT explain the x86-64 second boot that hits the 240 s watchdog on a loaded host: `smoke-test.sh` now
prints the tail of a failed later boot's log, and the next occurrence (after this fix) showed only
OVMF's screen-clear sequences, so the firmware never handed off to the kernel at all. That flake is
the firmware's under load, not this clock; it passes rerun alone. aarch64 and riscv64 already
multiplied in 128 bits.

The conversion now lives once in `kernel_core::ticks_to_ns_at(ticks, hz)`, 128-bit, used by all
three CPUs. `kernel-core/tests/ticks.rs` crosses the 64-bit edge at 100 MHz and a day of a 3 GHz
TSC. Live, on the interactive x86-64 image: `uptime` after 200 s read 203.7 s, then 223.7 s twenty
seconds later (the old image, same script: 184.47 s both times).
