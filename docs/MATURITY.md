# Aletheia — Implemented vs Production-Ready

**As of:** 2026-08-03. Closes GAPS4 **ALET-P2-013**.

Every gate in this repo answers "does it behave as specified, here, now". None answers "is it ready for
someone else's machine, data and adversaries". Conflating those is how a project ends up claiming more than
it has, so this file states the difference once and grades each subsystem against it.

## The levels

| Level | Means | Evidence required |
|-------|-------|-------------------|
| **P — Proved** | The behavior is specified, and a gate fails when it breaks. | A named invariant in a VM gate or a host property test, plus a `conformance.sh` behavior where it must not vary by CPU |
| **I — Implemented** | The code exists and works on the paths exercised. | It runs in a boot or a test, but its failure modes / scale / adversarial surface are not covered |
| **A — Architecture** | Written down, not built. | An ADR + a register row; **no** code claimed |
| **X — Production-ready** | Would survive a hostile environment, real hardware variety, and operational failure. | Nothing here is X yet — see "What production-ready would additionally require" |

**Nothing in Aletheia is X.** That is the honest headline of this file. Aletheia is a research OS with an
unusually strong proof discipline for its age — 64 cross-architecture behaviors, ~180 live invariants per
target, host property tests with crash sweeps at every prefix — and none of that is the same as production.

## Where each subsystem stands

| Subsystem | Level | What is proved | What is NOT |
|-----------|-------|----------------|-------------|
| Capability engine (mint/delegate/revoke, atomic authorize+execute) | **P** | Fail-closed authorization, attenuation, cascading + concurrent revocation (§INV-CAP-REVOKE) | No persistence of authority across reboot (ALET-P1-026); no formal scope composability (P1-027) |
| Intent→Action pipeline | **P** | Malformed model output cannot execute; every effect authorized | No adversarial campaign against intent confusion (P2-028) |
| Memory: admission, ownership, reclamation, teardown, erase-on-free, W^X | **P** | All three targets, live-tree audits, 0 violations | No DMA isolation (P1-018 — bus-master is now enabled); no higher-half split |
| Address-space layout + guard pages + VA 0 unmapped | **P** | Guard pages and a dead null page on all three targets | Not re-asserted inside derived per-process spaces; no heap/secondary-stack guards |
| Fault classification + trap re-entrancy | **P** (model) / **I** (wiring) | Exhaustive host proof; live on x86-64 | aarch64/RISC-V decoders not wired into their handlers; no adversarial ring-3 `#UD`/`#GP` entry trials (P1-011) |
| IPC (blocking, grants, priority inheritance, cancellation) | **P** | 25 written invariants with adversarial tests | Single-queue bounds only; no soak (P2-009) |
| SMP (bring-up, per-CPU queues, stealing, shootdown, affinity) | **P** | 22 invariants per target at `-smp 4` | Contention depth untested at scale (P1-014); task lifecycle transitions (P1-015) |
| Storage: journal, namespace, atomic replace, durable store | **P** | Crash sweep at EVERY prefix, host + real device; cross-reboot proof | One flat namespace, one bitmap block, contiguous extents, no encryption at rest (P1-028/029) |
| Drivers: virtio-blk over mmio + pci | **I** | Discovery, geometry, I/O, journal, filesystem over real devices on all three targets | Synchronous poll, one request in flight, no interrupts, no hotplug, no multi-queue, no restart/recovery (P2-019) |
| WASM components | **I** | No ambient authority, fuel bounds, attenuated spawn | Resource model beyond fuel (P1-021), ABI versioning (P1-022), supply-chain verification (P1-023), dependency resolution as a boundary (P1-024) |
| AI subsystem / Context Fabric | **I** | Untrusted-provider model; capability-gated search | Not an OS-native runtime (P2-024 deferred); no scheduler integration |
| Boot chain / secure boot | **A** | Component provenance (ed25519) is **P** | Measured chain, key custody, rollback: architecture only (P2-014/015/016/017) |
| Networking | **A** | — | Nothing built (P2-020) |
| Graphics / compositor, power management | **A** | — | Nothing built (P2-021/022); QEMU cannot prove either honestly |

## What production-ready would additionally require

1. **A supervisor.** Today a `KillTask` verdict ends the boot, because there is no task supervisor to kill
   a task *and continue*. Fault recovery (REQ-REL-001) is architecture only.
2. **DMA isolation.** Devices receive raw physical addresses; an IOMMU/SMMU model is the answer (P1-018).
3. **Interrupt-driven I/O.** Every driver polls. That is provable and slow, and it does not survive real
   device latencies.
4. **Real hardware variety.** Everything is proved under QEMU. Firmware quirks, cache maintenance for
   non-coherent DMA, and errata are untouched.
5. **Soak and fault injection at scale** (P2-008/009/010): the crash sweeps are exhaustive but short.
6. **Key lifecycle** (P1-028/029/030), and integrity that resists forgery rather than only rot — FNV-1a
   detects damage, not an attacker.
7. **An update and rollback story** (P2-016/017).

## How to read a claim in this repo

* `STATUS.md` says what a wave delivered and names what it did **not** claim. Trust the "not claimed" lines
  as much as the rest — they are written in the same commit as the code.
* `docs/gap/ARCHITECTURE-GAPS4-REGISTER.md` is the backlog and the anti-drift mechanism: `resolved` rows
  carry evidence that exists in the tree; `open`/`deferred` rows are not counted as delivered.
* `docs/TRACEABILITY.md` is machine-checked — a `delivered` row whose evidence path does not exist fails CI.
* A behavior in `scripts/conformance.sh` is stronger than an invariant in one gate: it must hold on **every**
  CPU target.
