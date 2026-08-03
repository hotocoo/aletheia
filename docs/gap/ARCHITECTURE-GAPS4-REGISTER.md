# GAPS4 Disposition Register

Machine-readable disposition of every finding in `ARCHITECTURE-GAPS4.md` (the comprehensive
repository audit). This register is the backlog tracker and the mechanism that keeps status claims
from drifting (ALET-P2-011) — every finding is either **resolved** (with evidence that exists in the
tree), **open** (queued, not yet started), or **deferred** (explicitly ADR-scoped as future
milestone work, carrying no false "done" claim).

**Disposition legend:** `resolved` — fix landed + verified; `open` — accepted, queued;
`deferred` — real work, scoped to a later milestone via ADR/roadmap (not counted as delivered).

**As of:** 2026-08-03.

| ID | Sev | Disposition | Evidence / Note |
|----|-----|-------------|-----------------|
| ALET-P0-001 | P0 | resolved | x86-64 boot gate at CI parity: `kernel-x86_64/scripts/build-image-linux.sh` (portable mtools ESP), `scripts/vm-e2e-x86.sh`, `.github/workflows/ci.yml` job `vm-e2e-x86`; boots exit 33 + 22 ring-3 markers |
| ALET-P0-002 | P0 | resolved | `scripts/build-all.sh` (every crate, own toolchain/target, host crates tested) + CI job `build-all`; `E2E-ALL: PASS` 3 targets |
| ALET-P0-003 | P0 | resolved | `scripts/check-ci-parity.sh` (REQ-QUAL-001) + CI job `ci-parity` in BOTH pipelines: FS-discovered bootable crates must each have a CI-executed boot gate, GitHub↔GitLab script sets must match, every matrix `VM Gate` must actually run, STATUS↔CI cross-check. Wiring it exposed and fixed 3 real gaps (GitLab missing `build-all`/`vm-e2e-x86`; `conformance.sh` claimed but never CI-run and macOS-gated on x86) |
| ALET-P1-001 | P1 | resolved | `kernel-core/src/vmaddr.rs` (REQ-MM-001, ADR-029) enforced at every mapping API on all three targets; host property proof `kernel-core/tests/vmaddr.rs` (no accepted VA pair aliases; every accepted PA is an owned frame) + 8/8/7 live-table VM invariants (aarch64 21, riscv64 21, x86-64 13 vm invariants) + refusals added to the conformance core contract |
| ALET-P1-002 | P1 | resolved | `kernel-core/src/ptreclaim.rs` (REQ-MM-003, ADR-031): an unmap that empties a table frees it — empty-only, parent cleared before the free, root never freed, stop at the first table in use, refused free restores the reference. Wired into all three targets' unmap paths; host-proved against an in-memory table model + VM-gated (aarch64/RISC-V 33 vm invariants, x86-64 25) + five reclamation behaviors added to the conformance core contract |
| ALET-P1-003 | P1 | resolved | `kernel-core/src/frameown.rs` (REQ-MM-002, ADR-030): one owner per frame, claimed/released through the allocator on all three targets; host property proof `kernel-core/tests/frameown.rs` (no frame held twice; counters balance; every refusal is a no-op, asserted after each of 20 000 deterministic ops) + 17 memory invariants per target in the VM gates (was 7) + the five ownership refusals added to the conformance core contract |
| ALET-P1-004 | P1 | resolved | `kernel-core/src/teardown.rs` (REQ-MM-004, ADR-032): a dying space returns every page/table/root it owns and nothing else, behind two independent guards (per-target privacy predicate + the ownership model); destroying the ACTIVE space is refused everywhere. Host-proved against an in-memory model + VM-gated on live hierarchies (aarch64/RISC-V 42 vm invariants, x86-64 33) with the frame count returning EXACTLY to its pre-space value + five teardown behaviors in the conformance core contract |
| ALET-P1-005 | P1 | open | SMP TLB shootdown formal contract (REQ-SMP-004 delivered; needs written invariant contract + adversarial test) |
| ALET-P1-006 | P1 | open | kernel/user virtual address layout hardening (guard pages, layout constants, KASLR posture) |
| ALET-P1-007 | P1 | resolved | W^X is a GLOBAL invariant of the kernel-built address space on both QEMU targets (REQ-MM-006, ADR-034 + its 2026-08-03 addendum). `kernel-core/src/memattr.rs` validates permissions at every dynamic mapping API on all three targets; on top of that, `linker.ld` now exports `__text_start`/`__text_end`/`__rodata_end` and `build_identity` builds every RAM block overlapping the image as 4 KiB pages — text RO+X, `.rodata` RO+NX, data/bss/stack/heap RW+NX, RAM outside the image RW+NX blocks. Both QEMU gates require `dynamic_violations == 0` **and** `bootstrap_violations == 0` (was `== 64`), plus four invariants that assert the split is real: the leaf over `__text_start` is a 4 KiB page, text is executable and read-only, `.rodata` is neither, data+stack are RW+NX. Because the split replaced the block descriptors that had implicitly made image VAs unmappable-over, `map_page`/`unmap_page` now refuse the block-aligned split span explicitly (verified: the two new invariants fail at 54 with `exit 114` against the pre-guard code). Virtual-memory invariants 49 → 55 on aarch64 and RISC-V (1087 / 576 live leaves audited, 0/0 violations). The x86-64 firmware-inherited tree is tracked separately as ALET-P1-031 |
| ALET-P1-008 | P1 | resolved | permissions decoded and validated per-arch at every mapping API (REQ-MM-006, ADR-034): aarch64 AP/UXN/PXN/AttrIndx, RISC-V R/W/X/U, x86-64 WRITABLE/NO_EXECUTE/USER_ACCESSIBLE with EFER.NXE + CR4.SMEP enabled after a CPUID check and reported at boot. Cacheability is enforced where the ISA expresses it (aarch64 AttrIndx ⇒ device memory is never executable) and explicitly modelled as Normal where it does not (RISC-V PMAs, x86-64 PAT/MTRRs) rather than silently assumed. Four attribute behaviors in the conformance core contract |
| ALET-P1-009 | P1 | open | x86-64 trap-frame layout hardening (manual ABI — add static asserts + fuzz) |
| ALET-P1-010 | P1 | open | shared mutable trap state reentrancy guarantees |
| ALET-P1-011 | P1 | open | interrupt/fault entry adversarial testing |
| ALET-P1-012 | P1 | open | x86-64 kernel stack guard pages |
| ALET-P1-013 | P1 | open | page-fault classification model (present/write/user/reserved/exec, fail-closed) |
| ALET-P1-014 | P1 | open | scheduler multicore contention testing depth |
| ALET-P1-015 | P1 | open | task lifecycle state-transition invariants |
| ALET-P1-016 | P1 | open | priority inheritance end-to-end IPC validation (REQ-IPC-009 delivered; needs deeper proof) |
| ALET-P1-017 | P1 | open | blocking IPC cancellation semantics explicit proof |
| ALET-P1-018 | P1 | open | DMA isolation model (device-visible memory boundary) |
| ALET-P1-019 | P1 | open | virtio-blk production-grade test depth (error paths, partial IO, malformed descriptors) |
| ALET-P1-020 | P1 | open | storage error semantics formal contract |
| ALET-P1-021 | P1 | open | WASM sandbox resource model beyond fuel (memory, table, stack, wall-clock) |
| ALET-P1-022 | P1 | open | component ABI explicit versioning |
| ALET-P1-023 | P1 | open | component installation supply-chain verification (builds on REQ-BOOT-002 ed25519) |
| ALET-P1-024 | P1 | open | component dependency resolution as a security boundary |
| ALET-P1-025 | P1 | open | capability revocation under concurrency (REQ-CAP-006 atomic authorize+execute delivered; extend to revocation races) |
| ALET-P1-026 | P1 | open | capability lifetime/persistence model |
| ALET-P1-027 | P1 | open | capability scope formal composability |
| ALET-P1-028 | P1 | open | key management (encryption-at-rest ≠ key lifecycle) |
| ALET-P1-029 | P1 | open | nonce/IV lifecycle proven per encrypted object |
| ALET-P1-030 | P1 | open | encrypted content-addressing identity semantics |
| ALET-P1-031 | P1 | resolved | x86-64 builds its OWN kernel map and RUNS ON IT (REQ-MM-006, ADR-034 + its 2026-08-03 addendum). `kernel-x86_64/src/kmap.rs` takes the image base/size from `LoadedImage` before `ExitBootServices` and the per-section permissions from the image's own PE section table — the x86-64 equivalent of the `linker.ld` symbols the other two targets read — then builds an identity map: 2 MiB RW+NX huge pages for RAM and sub-4 GiB MMIO, 4 KiB pages over every 2 MiB region the image touches (text RO+X, `.rodata` RO+NX, data/bss RW+NX, headers/padding RO+NX), every table frame claimed through the ADR-030 ownership model. Nine invariants prove each address class is mapped as the right thing BEFORE `kmap::activate()` writes CR3 (build-then-switch, so a wrong map fails a check instead of triple-faulting), and CR4.PGE is cycled across the write because OVMF marks its mappings global and a global TLB entry survives a CR3 load. Measured: 4 GiB covered, 2043 huge + 2560 page leaves, 5 split blocks, 11 table frames; **live audit 4603 leaves, 0 violations** where the inherited tree had 524 795 of 524 799 — and the spine, 4-core SMP and the entire ring-3 suite now execute on the kernel's tree. Boot fails closed (exit 28) on any live violation; the gate requires the ACTIVE marker + zero live violations instead of pinning a bootstrap count. Virtual-memory invariants 40 → 52. Not claimed: devices above 4 GiB (64-bit PCI BARs) are unmapped by this tree |
| ALET-P2-001 | P2 | open | pin Rust toolchains (dated nightly) across all crates |
| ALET-P2-002 | P2 | resolved | `--locked` enforced in `build-all.sh` + existing CI `--locked`; each crate carries `Cargo.lock` |
| ALET-P2-003 | P2 | open | complete CI quality gate set (fmt, clippy -Dwarnings, audit, deny) |
| ALET-P2-004 | P2 | open | `cargo audit` / advisory scanning in CI |
| ALET-P2-005 | P2 | open | license + SBOM generation |
| ALET-P2-006 | P2 | open | reproducible builds as a release property |
| ALET-P2-007 | P2 | open | strengthen marker-based VM testing (structured machine-readable markers) |
| ALET-P2-008 | P2 | open | fault-injection coverage |
| ALET-P2-009 | P2 | open | long-running soak testing |
| ALET-P2-010 | P2 | open | larger/more diverse property-test campaigns |
| ALET-P2-011 | P2 | resolved | this register + `check-traceability.sh` prevent manual-metric drift |
| ALET-P2-012 | P2 | open | machine-checked requirement traceability — evidence-exists (`check-traceability.sh`) and evidence-runs (`check-ci-parity.sh`) are gated; remaining: mechanically bind THIS register's rows to their evidence |
| ALET-P2-013 | P2 | open | separate "implemented" vs "production-ready" in all status docs |
| ALET-P2-014 | P2 | deferred | complete production boot chain — milestone work |
| ALET-P2-015 | P2 | deferred | secure boot — milestone work (ADR needed) |
| ALET-P2-016 | P2 | deferred | update/rollback system — milestone work |
| ALET-P2-017 | P2 | deferred | recovery architecture — milestone work |
| ALET-P2-018 | P2 | resolved | the storage stack has a top: a named-object filesystem namespace over the journal (REQ-FS-001, ADR-035). `kernel-core/src/fs.rs` keeps a directory block + an allocation bitmap block as ordinary home blocks, and commits every mutation — the data blocks, the bitmap and the directory — as ONE journal transaction, so a name and its allocation can never disagree and there is no repair pass because there is no inconsistent state to repair. A removed object's blocks are zeroed inside the same transaction (erase on delete, the storage twin of ADR-033), and the transaction bound is refused (`TooLarge`), never truncated. Proved on the host by `kernel-core/tests/fs.rs` — a crash sweep over EVERY prefix of a create and of a remove (each outcome must be the whole object or none of it, with an unrelated object untouched), a 4 000-op campaign re-checking the structural invariants after every op, and every refusal asserted to be a no-op — and in-kernel by 12 named behaviors in all three VM gates, which `conformance.sh` now requires of every target (37 → 49 core behaviors). On aarch64 the SAME twelve also run over the real virtio-blk device through the virtqueue (virtio invariants 5 → 17), so crash atomicity is a claim about hardware. Not claimed: one flat namespace, one bitmap block, contiguous extents only, no rename/in-place update, no per-object integrity beyond the commit checksum, no encryption at rest (ALET-P1-028/029 remain open) |
| ALET-P2-019 | P2 | deferred | complete driver model — milestone work. **Progress, not closure (REQ-DRV-004, ADR-036):** the virtio-blk driver moved out of the aarch64 *dev* crate into `kernel-core/src/virtioblk.rs` behind a two-function per-target seam (`VirtioHal`: an identity-mapped DMA frame + a barrier) plus an `MmioLayout`, so RISC-V — a **first-class** target that had no real storage at all — now drives a real disk and proves the journal AND the whole 12-behavior filesystem namespace over the virtqueue (`ALL 17 VIRTIO-BLK INVARIANTS HOLD` on both targets, boot fails 120+i / 180+i; host-proved in `kernel-core/tests/virtioblk.rs`, which pins the count, the numbering and the group order and asserts a mis-sized device is refused at invariant 2 before any I/O). Still missing for the *model*: hotplug, interrupt-driven completion (the poll is synchronous, one request in flight), multi-queue, device restart/recovery, DMA isolation (ALET-P1-018 — the device gets raw physical addresses), and x86-64, whose transport is virtio-**pci** and needs PCI enumeration rather than an MMIO window |
| ALET-P2-020 | P2 | deferred | networking stack — milestone work |
| ALET-P2-021 | P2 | deferred | graphics/compositor — milestone work |
| ALET-P2-022 | P2 | deferred | power management — milestone work |
| ALET-P2-023 | P2 | deferred | hardware discovery (ACPI/DT) completion — milestone work |
| ALET-P2-024 | P2 | deferred | native AI runtime as a complete OS subsystem — milestone work |
| ALET-P2-025 | P2 | open | context lifecycle resource boundaries |
| ALET-P2-026 | P2 | resolved | erase-on-free (REQ-MM-005, ADR-033): every target zeroes a frame at release — the choke point explicit frees, reclamation and teardown all share — so no frame carries a previous owner's bytes. Proved by the reuse case (pattern → free → same frame back → zeros) in each VM gate (memory invariants 17 → 21, x86-64 22) + the erase behavior added to the conformance core contract. Not claimed: frames never owned still hold firmware bytes (pre-boot memory, not task data) |
| ALET-P2-027 | P2 | open | relationship-graph access capability enforcement |
| ALET-P2-028 | P2 | open | intent-confusion attack testing |
| ALET-P2-029 | P2 | open | continuous threat-model maintenance |
| ALET-P2-030 | P2 | open | explicit security-boundary enumeration |
| ALET-P2-031 | P2 | open | DoS ≠ unauthorized-access distinction in the threat model |
| ALET-P2-032 | P2 | resolved | the kernel-image mapping refusal lives ONCE, in `kernel-core/src/vmaddr.rs`: `AddrPlan::with_protected` + `MapFault::ProtectedVirt` (REQ-MM-006, ADR-034 addendum). Closed at exactly the moment the row predicted — ALET-P1-031 gave x86-64 a split and would have made `in_image_split` a third copy — so instead the two existing copies were deleted and all three targets now declare only WHERE their image is (`image_split_span()` on aarch64/RISC-V, `kmap::protected_span()` on x86-64), while the refusal itself is one implementation applied by both `validate_map` and `validate_unmap`. Host-proved page by page across the span in `kernel-core/tests/vmaddr.rs`: both APIs refuse every page, the boundaries are half-open, a malformed protected address still reports the malformation first, and an undeclared span protects nothing. The two refusal behaviors joined the `conformance.sh` core contract (37 named behaviors) now that all three targets emit them in identical words |
| ALET-P3-001 | P3 | open | centralized assembly/Rust boundary documentation |
| ALET-P3-002 | P3 | open | unsafe/assembly audit ownership |
| ALET-P3-003 | P3 | open | centralized architectural invariants doc |

**Rollup (2026-08-03, third update):** 69 findings (67 audited + ALET-P1-031 and ALET-P2-032, both
split out while closing ALET-P1-007) — **15 resolved**, 44 open, 10 deferred (milestone subsystems).
The newest close is **ALET-P2-018**, the first of the deferred milestone subsystems to become code
rather than architecture text: the filesystem namespace, gated on all three targets and — on aarch64 —
over the real device.
Every P0 is closed, and the memory cluster is now complete end to end: address admission
ALET-P1-001, page-table reclamation ALET-P1-002, frame ownership ALET-P1-003, address-space
destruction ALET-P1-004, erase-on-free ALET-P2-026, per-arch attribute validation ALET-P1-008, W^X as
a global invariant ALET-P1-007, **x86-64 running on its own map ALET-P1-031**, and **the image
refusal unified in `kernel-core` ALET-P2-032**.

A frame can no longer be aliased, double-freed, leaked by unmapping, leaked by dying, or read by its
next owner. And on **all three targets** — not two — NO descriptor in the live address space is
writable+executable: kernel text is mapped read-only at 4 KiB granularity, and on x86-64 the
firmware's 524 795-violation tree is no longer translating at all rather than being reported beside
the one the kernel built. The last W^X hole is closed; what remains open in this cluster is layout
hardening (ALET-P1-006: guard pages, layout constants, KASLR posture), not permissions.
