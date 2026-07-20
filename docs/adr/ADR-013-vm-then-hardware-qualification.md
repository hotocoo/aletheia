# ADR-013 — Progressive qualification: automated VM testing before, and gating, real hardware

**Status:** Accepted
**Context:** ADR-010 defers bare-metal/GPU/NPU work honestly because it needs hardware. But
"needs hardware eventually" must not become "untested until hardware." A microkernel can be
built into a bootable image and exercised in an emulator on every change long before it runs
on a device, and virtual-machine testing — while necessary — is **not sufficient** on its own
(emulators model timing, devices, and accelerators imperfectly).
**Decision:** Qualification is staged and each stage gates the next. (1) **VM/emulator (P4):**
every change builds a bootable image and boots it in QEMU under CI; automated tests assert
boot success, kernel init, memory management, scheduling, IPC, capability enforcement, service
startup, and crash recovery, with a machine-checkable pass/fail (semihosting exit code). This
is delivered today: `kernel/` boots on QEMU `virt` at EL1 and re-proves the M1 invariants in
kernel space via 11 in-kernel selftests, gated by `scripts/vm-e2e.sh`. (2) **Real hardware
(P5):** an automated hardware lab flashes images, boots devices, captures serial/diagnostic
output, runs the suites, resets devices, and reports failures, per supported architecture and
accelerator configuration. Performance claims follow the same discipline: numbers are labeled
by substrate (emulated vs native), and only same-substrate comparisons are treated as fair
(e.g., the microkernel IPC fast-path vs the raw syscall-crossing floor on the same emulated
CPU); whole-OS "faster than Linux" is a benchmark program with named milestones, not a
headline asserted from one microbenchmark.
**Consequences:** Regressions are caught in CI in seconds, not on a bench weeks later; hardware
lab time is spent on what only hardware can prove. Emulation caveats are recorded so no VM
result is mistaken for a hardware guarantee. See PRD-003 §V&V, §Hardware Qualification, and
`kernel/README.md`.
