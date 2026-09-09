# ADR-129 — Apply the Architectural HWP Performance Posture Per CPU

**Status:** Accepted / Implemented  
**Date:** 2026-09-09  
**Scope:** x86-64 hardware performance control

## Decision

Intel HWP performance requests are applied independently by every logical processor when that CPU
comes online. The BSP continues to establish the requested posture during normal boot; each AP now
calls the same `hwpm::request_performance_mode()` backend after entering long mode.

The SMP gate records successful AP requests and, when the BSP exposes architectural HWP, requires
every online AP to have accepted its own request. Unsupported HWP remains fail-closed and does not
cause unsafe MSR probing.

## Rationale

`IA32_HWP_REQUEST` is processor-local state. Programming only the BSP does not establish a uniform
performance posture for CPUs that the SMP scheduler can subsequently run on. The previous design
therefore left a real multi-core utilization gap: the scheduler could migrate work to an AP whose
performance policy had never been raised to the requested architectural ceiling.

This is a hardware boost/performance request inside `IA32_HWP_CAPABILITIES`, not an unlocked-ratio or
voltage overclock. Aletheia must never manufacture an operating point that the processor does not
advertise.

## Verification

* `kernel-x86_64/src/smp.rs` applies the request on every AP and gates the online-core contract on it.
* `kernel-x86_64/scripts/smoke-test.sh` was updated for the new SMP invariant count.
* `cargo test` passes for `kernel-x86_64`.
* `scripts/vm-e2e-x86.sh` passes with exit code 33, including three boots, persistent custody,
  ring-3, live desktop/input, VT-d, and the structured marker gate (`smp=23`).
* QEMU's `qemu64`-style CPU used by the validation environment reports HWP unsupported, so the new
  per-AP actuator path is compile- and gate-verified but not a physical HWP measurement on this host.

## Non-goals

This ADR does not claim arbitrary overclocking, voltage control, thermal sensor support, CPPC control,
GPU/NPU frequency control, or physical-hardware qualification. Those require hardware-specific,
architecture-qualified actuators and safety limits.
