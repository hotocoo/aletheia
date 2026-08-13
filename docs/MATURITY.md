# Aletheia — Implemented vs Production-Ready

**As of:** 2026-08-07. Closes GAPS4 **ALET-P2-013**.

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
| Keyboard input (i8042 + scancode decoding) | **I** | ACPI-declared, controller and port self-tested, config read back, every wait spin-bounded; 10 decode invariants on all three targets + 14 host proofs (§INV-KEYMAP), 5 bring-up invariants on every x86-64 boot (§INV-PS2), and an end-to-end gate that types on the emulated keyboard (ADR-049) | x86-64 only — the QEMU `virt` machines have no PS/2 controller. No USB HID (no USB stack; legacy emulation covers most machines). One US QWERTY layout, no key repeat, no LEDs |
| Capability engine (mint/delegate/revoke, atomic authorize+execute) | **P** | Fail-closed authorization, attenuation, cascading + concurrent revocation (§INV-CAP-REVOKE), and the attenuation ORDER itself — sound, reflexive, transitive, proved by exhaustion over the whole finite lattice (§INV-CAP-SCOPE, ADR-048) | The lattice covers the three dimensions the model has; a new constraint dimension must be added and swept there |
| Capability lifetime across a reboot (`capstore`) | **I** | Save/load with the delegation order re-checked on every edge, revocation cascade re-derived, clock monotonicity and id uniqueness enforced, whole-image refusal; legacy `save_to_fs`/`load_from_fs` and key-backed HMAC-SHA256 `save_authenticated_to_fs`/`load_authenticated_from_fs` store the image as one atomically replaced `cap.store` object; wrong-key and tamper refusal host proofs; 14 in-kernel invariants on all three targets + host proofs (§INV-CAP-LIFE, ADR-048) | Authenticated API accepts a caller-supplied key but does not establish key custody, rotation, secure-boot delivery, or recovery; capability image and entity records are not yet committed in one combined transaction; the logical clock is still a caller-supplied constant |
| Intent→Action pipeline | **P** | Malformed model output cannot execute; every effect authorized | No adversarial campaign against intent confusion (P2-028) |
| Memory: admission, ownership, reclamation, teardown, erase-on-free, W^X | **P** | All three targets, live-tree audits, 0 violations | No DMA isolation (P1-018 — bus-master is now enabled); no higher-half split |
| Address-space layout + guard pages + VA 0 unmapped | **P** | Guard pages and a dead null page on all three targets | Not re-asserted inside derived per-process spaces; no heap/secondary-stack guards |
| Fault classification + trap re-entrancy + task supervisor | **P** (model) / **I** (wiring) | Exhaustive host proof; live user-fault containment, private-space reclaim and continuation on all three targets | aarch64/RISC-V handlers still construct normalized fault metadata at their trap seam; no adversarial ring-3 `#UD`/`#GP` entry trials on those targets (P1-011) |
| IPC (blocking, grants, priority inheritance, cancellation) | **P** | 25 written invariants with adversarial tests | Single-queue bounds only; no soak (P2-009) |
| In-kernel risk advisor (frozen integer forest, `mlrisk`) | **P** (the model and its guarantees) / **A** (its use) | The blob is embedded in every image and VERIFIED AT BOOT on all three targets: 20 in-kernel invariants (load, a measured worst-case compare bound, exact margin/verdict parity with the trainer over the whole committed fixture, determinism, out-of-box abstention, nine named refusals) plus 9 host proofs — including that an abstaining model schedules bit-identically to the model-free kernel and that priority is never traded for risk (REQ-ML-001, ADR-056), plus 8 stress invariants measured UNDER LOAD on every target (REQ-ML-002): the cost of an advice timed with each target's own clock, a verdict census that separates the conformal band from the training-box range guard, and an A/B of the same admission stream scheduled model-free and advised. Timings are reported, never gated; the properties are gated. Running that gate is what exposed the O(n^2) per-comparison allocation in `effective_priority` that was consuming 7.7 MB of an 8 MiB bump heap | ADVISORY by construction and by choice: it reorders tasks of EQUAL priority and nothing else. The live path landed 2026-08-13 (REQ-ML-003): `taskfeat.rs` derives the 20-feature vector from live kernel state under the trainer's own accounting rules, and `mlsched.rs` holds the forest resident for the machine's whole uptime with 12 further boot-gated invariants. STILL a wiring gap: each target's `usermode.rs` spawns ring-3 tasks through its own bespoke rotation rather than through `PriorityScheduler`, so a real user-mode task spawn does not yet reach the advisor. |
| SMP (bring-up, per-CPU queues, stealing, shootdown, affinity) | **P** | 22 invariants per target at `-smp 4` | Contention depth untested at scale (P1-014); task lifecycle transitions (P1-015) |
| Storage: journal, namespace, atomic replace, durable store | **P** | Crash sweep at EVERY prefix, host + real device; cross-reboot proof | One flat namespace, one bitmap block, contiguous extents, no encryption at rest (P1-028/029) |
| Drivers: virtio-blk over mmio + pci | **I** | Discovery, geometry, I/O, journal, filesystem over real devices on all three targets | Synchronous poll, one request in flight, no interrupts, no hotplug, no multi-queue, no restart/recovery (P2-019) |
| WASM components | **I** | No ambient authority, fuel bounds, attenuated spawn | Resource model beyond fuel (P1-021), ABI versioning (P1-022), supply-chain verification (P1-023), dependency resolution as a boundary (P1-024) |
| AI subsystem / Context Fabric | **I** | Untrusted-provider model; capability-gated search | Not an OS-native runtime (P2-024 deferred); no scheduler integration |
| Boot chain / secure boot | **A** | Component provenance (ed25519) is **P** | Measured chain, key custody, rollback: architecture only (P2-014/015/016/017) |
| Interactive console: editor + dispatcher | **P** | Fail-closed admission (only printable ASCII enters a line; bounded; refused bytes never echoed), escape sequences PARSED rather than filtered (no byte inside a sequence reaches the line; the parser is never left armed; parameters bounded), a real cursor with mid-line editing, bounded history and Tab completion over the live namespace, refusals named, supervisor counters visible through `faults`, console writes committed, every inspect/write/flush/reboot/halt command passes a fail-closed `ShellHost::authorize` hook backed by explicit `CapEngine` evaluation, and the 28-command working set runs through a capability-bound `DeviceGuard`/`BlockDevice` view that re-checks read/write/flush authority — host denial/revocation tests, cross-target builds, and a three-target scripted-operator gate with a reboot | Kernel-space dispatcher over the kernel's own objects — NOT a user-mode shell over a syscall ABI; current boot console is an explicitly privileged root policy that mints console/system capabilities at boot, not yet a user-authenticated shell; the editor writes backspaces and spaces, never cursor-address sequences, and does not know the terminal's width, so a line that wraps redraws wrongly on the wrapped portion; no reverse search, no multi-line editing, ASCII only |
| Interactive console: the INPUT path | **P** (ring policy) / **I** (the wire) | The input ring's overflow policy is proved everywhere — 9 live invariants per target, including that a decoded key enters the ring whole or not at all, 9 host tests on the SURVIVING contents, 3 conformance behaviors — so a burst can never rewrite a typed command. Real bytes arrive on an INTERRUPT on all three targets (GICv2 / 8259A IRQ4 / PLIC): the three-target scripted-operator gate types at a running machine, writes, halts, reboots and reads back | Each handler's wire path — acknowledge, drain, complete — is hardware, so no host test covers it; framing/parity errors are read past rather than reported; the TRANSMIT side is still polled everywhere; and `run_loop` still spins when the ring is empty |
| Networking | **A** | — | Nothing built (P2-020) |
| Graphics / compositor, power management | **A** | — | Nothing built (P2-021/022); QEMU cannot prove either honestly |

## What production-ready would additionally require

1. **A supervisor — partially built (REQ-REL-002, ADR-042).** A user fault now terminates that task,
   reclaims its private address space, and the boot continues, proved live on all three targets by faulting
   a user task on purpose and then running another one. What remains: restart policy / supervision trees
   (REQ-REL-001, still architecture only), quotas and rate limits, and a production TCB/task lifecycle.
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
