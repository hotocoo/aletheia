# Aletheia — Implementation Status

**As of:** 2026-08-23 (authority custody is a LIFECYCLE, not a caller-supplied key — the persisted registry gains `capvault`: a versioned data-key keystore sealed with in-tree RFC 8439 ChaCha20-Poly1305 under a root-derived subkey the vault alone retains, one-way rotation whose retirement DESTROYS the retired key, constructed prefix||counter nonces reserved before use because the kernel has no boot entropy, a three-commit rekey pivot crash-proved at EVERY recorded device-op position, and 17 custody invariants on every boot of all three targets — the custody half of ALET-P1-034 over authority, ADR-070; before that: encryption at rest is a LIFECYCLE, not a key file — the hosted semantic store gains versioned data keys under a root-derived keystore with rotation/rekey/retirement, constructed prefix||counter nonces whose ledger is the authenticated log itself, position-bound AEAD frames that refuse reordering/deletion/duplication with the position named, plaintext-SHA-256 identity semantics proved in both directions, and transparent wholesale migration of pre-ADR-069 logs detected by trial-authentication — closing the P1-028/029/030 trio over the store, ADR-069; before that: the supply chain is VERIFIED, LIVE, and RECORDED — chain verification crosses the installation boundary: root→signing-key→component provenance is enforced at install against public keys only, admitted entities record their full evidence, the launch gate re-judges that evidence against CURRENT trust so signer revocation goes live at the next launch, all faults are named per link, and the spawn path — found skipping provenance entirely, ALET-P2-050 — now passes the same gate, ADR-067; before that: the component DECLARES what it speaks — the ABI is explicitly versioned: a custom-section declaration enforced at BOTH gates, install refusing undeclared/malformed/foreign-version modules before their bytes are stored and run re-checking on every path, refusals naming both sides of a version disagreement, in... (line truncated to 2000 chars)
**Milestone delivered:** M1 — Hosted System-Core Reference (Rust); **P2 (start)** — WASM capability-secure component runtime; **P4 (start)** — bootable microkernel on THREE CPU targets, VM-tested: aarch64 (bootstrap) + AMD64/x86-64 (first-class) + **RISC-V/RV64GC (first-class)**; **P5 (start)** — real memory management: physical page-frame allocator + MMU virtual memory (identity map + dynamic map/unmap) + **EL0 user-mode with a capability-gated syscall boundary, hardware address-space isolation, per-process address spaces (separate TTBR0), and preemptive multitasking (full trap-frame context switch + round-robin scheduler + GICv2/generic-timer IRQ preemption)**, VM-tested on the aarch64 dev backend
**Maturity:** `docs/MATURITY.md` grades every subsystem Proved / Implemented / Architecture and states
plainly that **nothing here is production-ready** — read it before quoting any claim below.
**Sources of truth:** `docs/Aletheia_Product_Requirements_Document.md` (PRD-003),
`docs/Aletheia_Software_Architecture_Document.md` (SAD-002), `docs/adr/ADR-001..069`.

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