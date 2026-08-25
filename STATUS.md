# Aletheia — Implementation Status

**As of:** 2026-08-25 (the IOMMU contract is programmed into real silicon — on x86-64 the kernel
discovers the VT-d unit through ACPI DMAR/DRHD, programs an identity domain over owned frames with
the kernel image punched out of it, adopts that domain via SRTP and turns enforcement ON, then
proves live enforcement from the unit's own fault bank: the granted function walks clean, a
revoked function is denied with an ACTIVE record naming its source-id and reason CONTEXT_ENTRY_P,
a restored grant returns to silence, and enforcement stays latched until halt — ADR-073); before that: the custody anchor crosses the platform boundary — the vault root is DELIVERED over the firmware configuration channel on all three targets (QEMU fw_cfg: ioports under q35+OVMF, MMIO on both virt machines), through one door that names every impostor — absent, firmware-absent, wrong-size, foreign-root, rolled-back — with a THIRD rootless boot in each gate proving absence seals the vault while the machine continues, and the combined-transaction question DECIDED: paired commits write the vault generation into the durable entity-store record so even a consistent older VAULT-pair rollback is caught BY NAME (ADR-072); before that: authority custody is a LIFECYCLE, not a caller-supplied key — the persisted registry gains `capvault`: a versioned data-key keystore sealed with in-tree RFC 8439 ChaCha20-Poly1305 under a root-derived subkey the vault alone retains, one-way rotation whose retirement DESTROYS the retired key, constructed prefix||counter nonces reserved before use because the kernel has no boot entropy, a three-commit rekey pivot crash-proved at EVERY recorded device-op position, and 17 custody invariants on every boot of all three targets — the custody half of ALET-P1-034 over authority, ADR-070; before that: encryption at rest is a LIFECYCLE, not a key file — the hosted semantic store gains versioned data keys under a root-derived keystore with rotation/rekey/retirement, constructed prefix||counter nonces whose ledger is the authenticated log itself, position-bound AEAD frames that refuse reordering/deletion/duplication with the position named, plaintext-SHA-256 identity semantics proved in both directions, and transparent wholesale migration of pre-ADR-069 logs detected by trial-authentication — closing the P1-028/029/030 trio over the store, ADR-069; before that: the supply chain is VERIFIED, LIVE, and RECORDED — chain verification crosses the installation boundary: root→signing-key→component provenance is enforced at install against public keys only, admitted entities record their full evidence, the launch gate re-judges that evidence against CURRENT trust so signer revocation goes live at the next launch, all faults are named per link, and the spawn path — found skipping provenance entirely, ALET-P2-050 — now passes the same gate, ADR-067; before that: the component DECLARES what it speaks — the ABI is explicitly versioned: a custom-section declaration enforced at BOTH gates, install refusing undeclared/malformed/foreign-version modules before their bytes are stored and run re-checking on every path, refusals naming both sides of a version disagreement, in... (line truncated to 2000 chars)
**Milestone delivered:** M1 — Hosted System-Core Reference (Rust); **P2 (start)** — WASM capability-secure component runtime; **P4 (start)** — bootable microkernel on THREE CPU targets, VM-tested: aarch64 (bootstrap) + AMD64/x86-64 (first-class) + **RISC-V/RV64GC (first-class)**; **P5 (start)** — real memory management: physical page-frame allocator + MMU virtual memory (identity map + dynamic map/unmap) + **EL0 user-mode with a capability-gated syscall boundary, hardware address-space isolation, per-process address spaces (separate TTBR0), and preemptive multitasking (full trap-frame context switch + round-robin scheduler + GICv2/generic-timer IRQ preemption)**, VM-tested on the aarch64 dev backend
**Maturity:** `docs/MATURITY.md` grades every subsystem Proved / Implemented / Architecture and states
plainly that **nothing here is production-ready** — read it before quoting any claim below.
**Sources of truth:** `docs/Aletheia_Product_Requirements_Document.md` (PRD-003),
`docs/Aletheia_Software_Architecture_Document.md` (SAD-002), `docs/adr/ADR-001..073`.

## Current wave — the IOMMU contract is programmed into real silicon (2026-08-25, ADR-073)

ALET-P1-018 advances to the first hardware rung. DELIVERY on x86-64:
`kernel-core/src/vtd.rs` (the wire: register map, root/context encodings, second-level domain
builder, auditor, controller with named refusals) + `kernel-x86_64/src/vtd.rs` (the platform:
ACPI DMAR/DRHD discovery, UEFI-map spans minus the kernel image, per-bus-0-function context
entries, and a 12-invariant live gate). The gate adopts the root (SRTP), turns enforcement ON
(TES observed), kicks the LIVE block functions this boot already drives, and takes its evidence
from the unit's own fault bank: the granted function walks CLEAN; revoking a function's context
and kicking produces an ACTIVE record naming its source-id with reason CONTEXT_ENTRY_P; restoring
the grant returns that function to silence; enforcement stays latched until halt
(`[dmar] ALL 12`, marker dmar=12). Drivers negotiate VIRTIO_F_IOMMU_PLATFORM whenever offered.
Two register-interface facts were forced by the live unit and are documented in ADR-073: the
fault bank is WRITE-ONE-TO-CLEAR, and QEMU serves FSTS at 0x34 where the spec puts 0x30 — so
enforcement EVIDENCE comes from the fault-record BANK (exact everywhere), not from FSTS.PPF.
Boot order changed deliberately: every DMA-dependent suite runs BEFORE the vt-d gate (devices are
brought up before enforcement — how real platforms meet an IOMMU) and the gate is last, because
what it turns on stays on until halt. Named non-claims: SMMUv3 delivery, per-device windows,
interrupt remapping, queued invalidation, pass-through types, and post-enable completion
assertions — QEMU 11.x TCG loses virtio completions across a mid-run enablement ('bogus descriptor
or out of resources'); the full evidence trail is in ADR-073.

## Previous wave — the custody anchor crosses the platform boundary (2026-08-24, ADR-072)

ALET-P1-034 closes completely. DELIVERY: \`kernel-core/src/bootroot.rs\` + per-target fw_cfg
transports hand the vault its 32-byte root over the platform channel; only Delivered(exactly 32)
opens a vault, and RootNotProvided / FirmwareAbsent / MalformedRoot are refused BY NAME. DECISION:
image and entity store stay two commits but are mutually detectable — each paired commit writes
the vault generation inside the durable entity record, and custody-open enforces
witnessed_generation <= keystore_counter, converting ADR-070's pinned undetectable residual into
a named refusal. Proofs: host sweeps in tests/bootroot.rs (lying directories, truncations,
wrong sizes, constructed pair-rollback, fault-at-every-pair-position) plus [vault] ALL 14
CUSTODY-DELIVERY INVARIANTS HOLD on real firmware + real persistent media on all three targets.
Every QEMU gate gained a THIRD rootless boot proving absence seals the vault while the machine
continues; marker maps gained vault=14 deliberately (ADR-061). Heap grew 8 -> 12 MiB on the DT
targets to hold the resident custody state (ADR-063 posture).

## Current wave — the IOMMU contract is modeled, not assumed (2026-08-23, ADR-071)

ALET-P1-018 advances: `kernel-core/src/iommu.rs` defines and proves the full enforcement semantics
of a hardware IOMMU as a software model (`SoftIommu`), so every proof runs on the host today and
a hardware implementation must satisfy the same contract. Nine invariants boot on all three
targets; seven are pinned cross-CPU. The gate-marker map changed deliberately (`iommu=9`).
Hardware realization (VT-d/SMMUv3 programming) stays scoped in the gap register.


## Current wave — authority custody is a lifecycle, not a caller-supplied key (2026-08-23, ADR-070)


The custody and rotation halves of ALET-P1-034 close, because they were one gap: `capstore` could
authenticate a persisted registry only under a key the CALLER handed in on every call, so custody
was nobody's, rotation was impossible, and every boot re-asked the question a keystore exists to
answer. The constraint that shaped everything: the kernel has NO entropy source at boot, so
randomness could not be the mechanism — the lifecycle had to be safe BY CONSTRUCTION.

* **The root is custody; working keys are derived.** `CapVault::open` takes the 32-byte root once,
## Gates executed in CI

Both pipelines (GitHub Actions and GitLab CI) execute exactly these scripts, each asserted by
scripts/check-ci-parity.sh against this file: scripts/build-all.sh (every crate on its own
toolchain, host crates tested), scripts/check-boundary-docs.sh, scripts/check-ci-parity.sh,
scripts/check-register.sh, scripts/check-traceability.sh, scripts/comparative-bench.sh,
scripts/conformance.sh (the cross-CPU core contract), scripts/console-agent-e2e.sh,
scripts/console-ai-e2e.sh, scripts/console-e2e.sh, scripts/keyboard-e2e.sh,
scripts/quality-gate.sh, and the four VM gates — scripts/vm-e2e.sh (aarch64),
scripts/vm-e2e-riscv.sh (RISC-V), scripts/vm-e2e-x86.sh (x86-64 under OVMF) and
scripts/vm-e2e-vbox.sh (VirtualBox, the second-hypervisor rung).
