# ADR-019: Aletheia-owned HAL; AMD64 (x86-64) and RISC-V as first-class targets

**Status:** Accepted · **Date:** 2026-07-21

## Context

Aletheia is a completely new operating system designed from first principles — **not** macOS/Darwin,
**not** Linux, not a fork, and not a distribution built on another OS. It must be hardware-independent
through an **Aletheia-owned Hardware Abstraction Layer (HAL)**, with **AMD64/x86-64 and RISC-V as
first-class target architectures**. The aarch64 (QEMU `virt`) kernel built so far is a bootstrap/dev
target only.

Target architecture:

```text
Aletheia Applications → Native APIs & User Environment → AI Runtime / Intelligence Substrate
→ Aletheia Services → Aletheia Kernel → Aletheia HAL → AMD64/x86-64 | RISC-V hardware
```

## Decision

Introduce the Aletheia HAL (`kernel/src/hal.rs`) as the single arch-abstraction seam: a `Hal` trait
defining the arch-independent primitives the kernel needs (timer, privilege level, machine exit;
console, IPC-relevant CPU state, and MMU/address-space to follow), against which the kernel is
written — never against a specific CPU.

Target matrix:
- **AMD64 / x86-64** — first-class production target.
- **RISC-V (riscv64)** — first-class production target.
- **aarch64** — bootstrap/dev target implemented today; VM-tested; exercises the HAL contract live.

Per ADR-010 (no blind hardware code), the x86-64 and RISC-V backends are declared as the contract
they must satisfy but are `cfg`-gated to their own targets, so no untested bring-up code ships in the
VM-tested aarch64 build. Each is implemented behind the SAME `Hal` trait when brought up in a VM.

The HAL abstracts **genuine hardware differences only**. It must NOT import Linux, macOS/Darwin,
POSIX, or any other OS architecture. Every operating-system abstraction above the HAL — the syscall
ABI, scheduler, memory model, IPC, capability/security model, storage model, device model, native
APIs, AI runtime, and user environment — belongs to Aletheia.

## Consequences

- The kernel is arch-independent; changing target swaps only the HAL backend, nothing above it.
- Rust is the implementation language; the CPU architecture is a hardware target, not an OS
  dependency. AMD64 and RISC-V are hardware; the OS is Aletheia's own.
- aarch64 stays green in QEMU as the contract's live proof while AMD64 and RISC-V are brought up.
- The macOS host remains a temporary dev environment for the *hosted System Core only*; it is never
  imported into the Aletheia architecture (no Darwin/POSIX assumptions leak inward).
