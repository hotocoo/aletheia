# Aletheia — Implementation Status

**As of:** 2026-08-28 (LETHE — the resident performance advisor — advises the
power/performance contract: `kernel-core/src/lethe.rs` verifies a frozen integer model (two
decision trees in one `ALTH1` blob, a 12-feature contract hash making moved feature meanings a
named refusal) and its advised governor path consults it once per domain per step — FREQ advice
(Coast/Hold/Boost; Boost pins the top of the governor range) from demand history, churn, dwell
and thermal margin, IDLE advice (Stay/Shallow/Deep) for zero-demand domains; ADVISORY by
construction: with the advisor absent or abstaining the advised path is bit-identical to the
ADR-076 baseline governor, with it present the overclock band stays authority-only and demanded
silicon is never parked; parity with the trainer is a committed fixture replayed through the
live observer at every boot — 12 boot invariants on all three targets, seven pinned cross-CPU,
13 host proofs, and a vendored comparative benchmark where Lethe beats the ADR-076 baseline by
2.88% and a TUNED classic hysteresis by 11.29% under the documented cost model, losing the
bursty regime and saying so — REQ-ML-006, ADR-077); before that: 2026-08-28 (THE
POWER/PERFORMANCE CONTRACT is modeled, not assumed — frequency is
AUTHORITY and heat is a HARD CEILING: `kernel-core/src/pm.rs` gives every domain an honest
discrete ladder, keeps the governor range free to any caller, gates the overclock band behind
live per-domain elevation grants that attenuate on delegation and clamp the domain back to
nominal the moment their grant dies, makes the thermal envelope absolute BY CONSTRUCTION,
answers a thermal trip with a machine-wide clamp and a tick-exact cooldown that refuses even
valid grants, never lets the governor overclock or park demanded silicon, accounts idle
residency and wake latency exactly, moves device power along legal arcs only, and audits every
act in a bounded monotonic ledger — 14 boot invariants on all three targets, six behaviors
pinned cross-CPU, 19 host proofs, ADR-076); before that: 2026-08-26 (PER-DEVICE DMA WINDOWS are enforced by the real VT-d unit - each driven function
translates ONLY the frames its own driver registry granted; a revoked PAGE is denied by name with measured
reason 6 while sibling windows keep serving - ADR-075); before that: the IOMMU contract crosses the ARM fence — on x86-64 the kernel
discovers the VT-d unit through ACPI DMAR/DRHD, programs an identity domain over owned frames with
the kernel image punched out of it, adopts that domain via SRTP and turns enforcement ON, then
proves live enforcement from the unit's own fault bank: the granted function walks clean, a
revoked function is denied with an ACTIVE record naming its source-id and reason CONTEXT_ENTRY_P,
a restored grant returns to silence, and enforcement stays latched until halt — ADR-073); before that: the custody anchor crosses the platform boundary — the vault root is DELIVERED over the firmware configuration channel on all three targets (QEMU fw_cfg: ioports under q35+OVMF, MMIO on both virt machines), through one door that names every impostor — absent, firmware-absent, wrong-size, foreign-root, rolled-back — with a THIRD rootless boot in each gate proving absence seals the vault while the machine continues, and the combined-transaction question DECIDED: paired commits write the vault generation into the durable entity-store record so even a consistent older VAULT-pair rollback is caught BY NAME (ADR-072); before that: authority custody is a LIFECYCLE, not a caller-supplied key — the persisted registry gains `capvault`: a versioned data-key keystore sealed with in-tree RFC 8439 ChaCha20-Poly1305 under a root-derived subkey the vault alone retains, one-way rotation whose retirement DESTROYS the retired key, constructed prefix||counter nonces reserved before use because the kernel has no boot entropy, a three-commit rekey pivot crash-proved at EVERY recorded device-op position, and 17 custody invariants on every boot of all three targets — the custody half of ALET-P1-034 over authority, ADR-070; before that: encryption at rest is a LIFECYCLE, not a key file — the hosted semantic store gains versioned data keys under a root-derived keystore with rotation/rekey/retirement, constructed prefix||counter nonces whose ledger is the authenticated log itself, position-bound AEAD frames that refuse reordering/deletion/duplication with the position named, plaintext-SHA-256 identity semantics proved in both directions, and transparent wholesale migration of pre-ADR-069 logs detected by trial-authentication — closing the P1-028/029/030 trio over the store, ADR-069; before that: the supply chain is VERIFIED, LIVE, and RECORDED — chain verification crosses the installation boundary: root→signing-key→component provenance is enforced at install against public keys only, admitted entities record their full evidence, the launch gate re-judges that evidence against CURRENT trust so signer revocation goes live at the next launch, all faults are named per link, and the spawn path — found skipping provenance entirely, ALET-P2-050 — now passes the same gate, ADR-067; before that: the component DECLARES what it speaks — the ABI is explicitly versioned: a custom-section declaration enforced at BOTH gates, install refusing undeclared/malformed/foreign-version modules before their bytes are stored and run re-checking on every path, refusals naming both sides of a version disagreement, in... (line truncated to 2000 chars)
**Milestone delivered:** M1 — Hosted System-Core Reference (Rust); **P2 (start)** — WASM capability-secure component runtime; **P4 (start)** — bootable microkernel on THREE CPU targets, VM-tested: aarch64 (bootstrap) + AMD64/x86-64 (first-class) + **RISC-V/RV64GC (first-class)**; **P5 (start)** — real memory management: physical page-frame allocator + MMU virtual memory (identity map + dynamic map/unmap) + **EL0 user-mode with a capability-gated syscall boundary, hardware address-space isolation, per-process address spaces (separate TTBR0), and preemptive multitasking (full trap-frame context switch + round-robin scheduler + GICv2/generic-timer IRQ preemption)**, VM-tested on the aarch64 dev backend
**Maturity:** `docs/MATURITY.md` grades every subsystem Proved / Implemented / Architecture and states
plainly that **nothing here is production-ready** — read it before quoting any claim below.
**Sources of truth:** `docs/Aletheia_Product_Requirements_Document.md` (PRD-003),
`docs/Aletheia_Software_Architecture_Document.md` (SAD-002), `docs/adr/ADR-001..075`.

## Current wave — Lethe, the resident performance advisor (2026-08-28, ADR-077)

The power/performance contract (ADR-076) made frequency AUTHORITY and heat a HARD CEILING; this
wave gives its governor a MEMORY that obeys it. `kernel-core/src/lethe_contract.rs` + `kernel-core/src/lethe.rs`
define the 12-feature contract (demand history over 16 samples, dwell at the current point,
churn in the last 16 steps, reported temperature against the trip margin, the point's share of
the governor range) and the advisor: two decision trees packed into `models/lethe_pm.alth`,
verified at load against ten named refusals — including a CYCLE check (load walks each tree
with a visited set, because an evaluate-time loop would be a hang, not an error) and an
inverted training box (the range guard must be able to fire). `govern_advised` observes the
live state EXCLUSIVELY (history strictly before the advice acts), consults the advisor, and
acts only through the contract's own named APIs — `request_index`, `wake`, `enter_idle` — so
every act is audited and every refusal named.

The advisor proposes; the contract disposes. The suite proves the sharp edges: with a
full-ceiling grant MINTED and the advisor decisive, no reachable point exceeds nominal; parks
happen only at zero demand; residency is monotone and wake latency is a sum of real wake costs;
the census accounts for every consultation; and with the advisor ABSENT — or abstaining on a
collapsed training box — the advised path drives the machine through the SAME clock sequence as
the untouched `govern` baseline. `PmEngine::govern` is byte-for-byte unchanged.

Proofs: 13 host tests in kernel-core/tests/lethe.rs (the full mutation table for every named
refusal, contract/blob agreement, fixture parity with determinism, the absent- and
abstaining-advisor equivalence sweeps over randomized multi-regime traces, the safety sweep,
engine-level determinism including the ledger, ledger wraparound, observer bounds with
features in-domain for arbitrary streams, degenerate-input withholding, a REPORTED advice-cost
measurement of ~370 ns/advice in a debug build), plus the 12 invariants booting on all three
targets (`[lethe] ALL 12 LETHE ADVISOR INVARIANTS HOLD`, boot fails 580+i), seven pinned
cross-CPU in the conformance contract. The marker maps changed deliberately (lethe=12,
ADR-061).

The benchmark proof is vendored (docs/evidence/lethe006): a deterministic trainer/exporter
(six workload regimes with a thermal stand-in that heats on the clock each arm ran at;
cost-sensitive depth-3 CART on expected per-row cost; K=16 class-consistent counterfactual
rollout labels; two DAgger rounds on the policy's own trajectory) plus the six-arm comparison
on 300 held-out traces — ADR-076 baseline, eager C2 parker, TUNED classic hysteresis, Lethe,
always-nominal, always-low — under a documented cost model (ramp 2 steps, wake penalties 1/3
steps, CV² energy with the ladder's own mV, unmet work weighted 10× energy with the α sweep
published). Lethe 0.6100 vs baseline 0.6280 (+2.88%) and hysteresis 0.6876 (+11.29%),
dominating the baseline on BOTH components; the per-regime decomposition shows the lead is the
idle policy (0.017 vs 0.097 on idle regimes) and Boost's anti-churn pinning (staccato 1.017 vs
1.083), with the bursty regime honestly LOST (0.869 vs 0.857). Named non-claims, in the
register: the numbers live in the simulator's cost model (the kernel models transitions as
free); no live governor thread exists yet (residency = wired into the model's govern path and
proved at boot, the pre-REQ-ML-003 posture); the corpus is synthetic — this says nothing about
real silicon or real operating systems.

## Previous wave — the power/performance contract is modeled, not assumed (2026-08-28, ADR-076)

ALET-P2-022 leaves the deferred column. The wave answers the OS's overclocking promise the way
this kernel answers every privileged act: frequency is AUTHORITY, heat is a HARD CEILING.
`kernel-core/src/pm.rs` defines the contract as a complete software model: every core belongs to
a frequency DOMAIN with an honest discrete ladder (registration refuses dishonest ladders by
name); the governor range (at or below nominal) is free to any caller; the OVERCLOCK band above
nominal exists only through a LIVE, per-domain elevation grant — attenuated on delegation
(a child ceiling never widens its parent, `Amplification`/`CrossDomain` refused), revoked with
cascade, and clamping the domain back to nominal the moment its grant dies (a governor-range
grant clamps nothing); the thermal ENVELOPE is absolute BY CONSTRUCTION — no ladder point above
it can register and no grant past it can mint, so no reachable state exceeds it, whatever
authority says; a thermal TRIP clamps every domain to its lowest point and latches a tick-exact
cooldown that refuses elevation BY NAME even with a valid grant while the governor range keeps
serving; the demand governor never enters the OC band and never parks demanded silicon
(`DomainBusy`), parking zero-demand domains instead (the idle machine costs nothing, ADR-056);
idle residency and wake latency are accounted exactly, with a clock change CLOSING a parked
span so real time is never lost; device power moves only along legal arcs (D3→D1 refused — wake
through D0 or not at all); and every accepted act and every refusal lands in a bounded audit
ledger under a monotonic sequence, the holder named on grant acts.

Proofs: 19 host tests in kernel-core/tests/pm.rs — the full OC-band decision table over every
point × ceiling × authority state, a 5^3 attenuation-chain sweep, revocation clamps and
idempotence, envelope absoluteness from registration and mint, cooldown tick-exactness across
the whole window, idle accounting under transition interference, the complete device-arc table,
ledger completeness with wraparound, capacity bounds, and bit-identical determinism — plus 14
in-kernel invariants booting on all three targets (`[pm] ALL 14 POWER-PERFORMANCE INVARIANTS
HOLD`, boot fails 560+i), six of them pinned cross-CPU in the conformance contract. The marker
maps changed deliberately (`pm=14`, ADR-061). Named non-claims, in the register: no
MSR/CPPC/ACPI programming (QEMU TCG exposes no frequency control to the guest — a hardware rung
attempted today could only prove code ran, not that anything enforced; the ADR-071 posture),
no battery, no system sleep/wake, no voltage rail enforcement beyond recording mV, no
thermodynamic simulation — callers report temperatures, the contract decides.

## Previous wave — per-device DMA windows (2026-08-26, ADR-075)

ALET-P1-018 advances to its third hardware rung. The registry-driven narrowing lands on VT-d:
every DRIVEN function now gets its OWN second-level tree containing exactly the frames ITS
driver registry vouches for (leaf-set equality audited live against sorted spans), ungranted
functions get NO context entry and the gate reads their absence back from the live context
table, grant sets are pairwise disjoint or the boot refuses, and revocation granularity drops
to ONE PAGE: the block device data-frame leaf is revoked under enforcement and the unit answers
with an ACTIVE record naming source-id AND address with MEASURED reason 6 (PAGING_NOT_PRESENT,
pinned beside the ADR-073 codes 2/4/5) while sibling windows keep serving; restore returns
read-back equality and silence; enforcement stays latched layered over the software registry.
dmar 12 -> 14; host proofs tests/vtd.rs 12 -> 15. En route the wave EXPOSED and FIXED a
repo-wide boot breaker: the ADR-074 seam mapped [bar_base, len) instead of bar_base+offset, so
q35 device-cfg ran unmapped and every target gate died in an infinite mis-labelled ring-3 fault
loop (commit fix(pci), found by bisect plus CR3/translate instrumentation). Named at the
boundary: SMMUv3 per-stream windows, device-side walk probes on ARM (QEMU 11.1 artifact),
interrupt remapping, queued invalidation and pass-through types stay open in the gap register.
## Previous wave - the IOMMU contract crosses the ARM fence (2026-08-26, ADR-074)

ALET-P1-018 advances to its second hardware rung. DELIVERY on aarch64: kernel_core::smmu programs
the ARM SMMUv3 QEMU emulates on virt - discovered through the machine's own device tree, delivered over
the firmware configuration channel (the same door as the custody anchor; direct -kernel ELF boots get
NO DTB pointer at all - measured x0=0), stage-2-only identity domain over OWNED frames minus image, every
present PCI function granted an STE under its DECLARED iommu-map stream id, stream table + command/event
queues published with readback, enforcement enabled through CR0->CR0ACK and latched layered over the software
DMA registry: a 10-invariant boot gate (smmu=10) on top of 15 host proofs against a simulated unit and a
device-side walker built from the emulator's own decoder shapes. The virtio-pci transport moved ONCE into
kernel-core (PciEnv seam) when this wave became its second consumer; the aarch64 kernel became its own PCI
firmware (BAR sizing + assignment) because bare-metal boots run none. NAMED at the boundary: CLI-attached
virtio-pci DMA does not traverse the legacy iommu=smmuv3 unit on QEMU 11.1 (abort-canary measured), so
grant-serves/revocation-events stay open in the gap register beside ADR-073's completion-loss artifact.


## Previous wave — the IOMMU contract is programmed into real silicon (2026-08-25, ADR-073)

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

## Previous wave — the IOMMU contract is modeled, not assumed (2026-08-23, ADR-071)

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
