# Production-readiness audit — Aletheia v0.7.3 (2026-10-09)

**Verdict: a research operating system with production-grade discipline in its proofs, not a
production operating system.** What is built is gated on every push, on three CPU architectures,
and its claims are measured. What is not built is listed below by name, because the goal this
release answers to (a native System 1 everywhere, Laya shipped first-class, workstation, gaming,
graphics, virtualization and overclocking) is larger than what exists. `docs/MATURITY.md` grades
every subsystem; this document is the release-level summary and the reproduction recipe.

## What v0.7.x delivers (ADR-237..251)

v0.7.1 supersedes v0.7.0: a review after publication found a locked console still serving the
desktop's file panel and browser window, and an older cached checkpoint resolvable under the v4
manifest; both are fixed and tested (ADR-246).


| Area | State | Evidence |
|---|---|---|
| System 1, scheduler | Live: equal-priority advice on every admission (`mlrisk`), background programs share turns by weight with advice ordering ties | ADR-056/199/239; console-e2e measures weight 1 vs 4 at 3.98x / 4.00x / 3.99x on the three CPUs |
| System 1, memory | Live: memory boundary at admission; reclaim under live pressure ranks background programs by the eviction forest | ADR-081/082/238; reclaim family 11 invariants per CPU |
| System 1, power | Live: Lethe governor off the timer | ADR-165..173 |
| System 1, cache | Measured: CLOCK equals Belady's optimum on the namespace trace at 32 blocks; no learned policy can save a miss there | ADR-243 |
| System 1, anomaly | Live: rate detector over the kernel's failure counters, advisory | ADR-242; anomaly family 7 per CPU; console-e2e flags exactly the injected burst |
| System 1, decisions (Laya) | Native runtime (`aletheia-laya`, no Python), supervised by `aletheiad model serve`, console checkpoint v4 knows every shipped command; 112 ms vs the reference's 102 ms per CPU decision | ADR-240/241/245/249/251; parity 932/932, 700/700, 696/696 answers vs the reference |
| System 2 | Registry-selected local LLM behind the decision wire and the dual router | ADR-174/186 |
| Security | Capability engine, W^X, guard pages, IOMMU (VT-d/SMMU), TLS 1.3 from scratch, console accounts (PBKDF2, back-off, unreadable record), roles, per-object ownership, the lock covering the desktop | ADR-048/071/151/244/246/247/250; MATURITY rows |
| Reliability | Supervisor contains user faults; seeded fuzz of console, network, desktop; crash sweep at every journal prefix | ADR-180..183/202 |

## Validation, reproducible

Every command below ran for this release on an Apple M4 Max host (QEMU TCG guests); CI runs the
gate set on every push (`.github/workflows/ci.yml`, 38 jobs).

```bash
cd kernel-core && AI_PROVIDER=deterministic cargo test --release       # 950 passed
cd aletheia && cargo test                                                # 284 passed
cd aletheia-laya && cargo test --release                                 # 8 passed (no checkpoint needed)
./scripts/vm-e2e.sh            # aarch64: 66 families, 858 boot invariants, three boots
./scripts/vm-e2e-riscv.sh      # riscv64: 62 families, 795 invariants
./scripts/vm-e2e-x86.sh        # x86-64: 66 families, 869 invariants
./scripts/console-e2e.sh       # operator sessions on all three CPUs, across a reboot (accounts, weights, anomaly)
./scripts/console-fuzz-e2e.sh  # 1000 hostile lines per CPU
./scripts/console-ai-e2e.sh && ./scripts/console-agent-e2e.sh   # model-driven console (deterministic arm)
./scripts/quality-gate.sh      # fmt, clippy -D warnings (8 crates), cargo audit, licenses, SBOM
for g in traceability register boundary-docs threat-model ci-parity userland lethe-pin; do ./scripts/check-$g.sh; done
BOOT_SAMPLES=3 WORKLOAD_OPS=25 ./scripts/comparative-bench.sh            # docs/BENCHMARKS.md section 000
python3 scripts/system1/native_parity.py CORPUS REF_ENDPOINT NATIVE_ENDPOINT   # needs a checkpoint
```

Against Linux 6.12 on the same emulator (BENCHMARKS section 000): boot 2299 vs 1841 ms (Aletheia
boots through OVMF, Linux skips firmware), idle CPU 0.0 vs 0.4-0.9 %, typed round-trip 2-3 vs
27-28 ms/op, payload 3.5 vs 14.2 MB. No overall speed winner is claimed.

## What is NOT production-ready, by name

| Gap | Status | Why it matters |
|---|---|---|
| Real hardware | Boots on emulated boards (QEMU virt, q35/OVMF, VirtualBox); VMware package unverified on VMware itself | A production OS runs on machines people own |
| Interrupt-driven, multi-queue I/O | Every driver polls, one request in flight; since ADR-248 a virtio-blk request carries up to 6 blocks (3-5x on 48-block reads) | Throughput and latency under load; an I/O System 1 has no queue to order |
| GPU 3D, graphics stack | virtio-gpu 2D, 1-bit surfaces plus a colour plane for programs | No 3D, no Vulkan/GL, no media decode |
| Native 3A-class gaming | Not started | Needs the GPU stack, input latency work, and a userland that does not exist yet |
| Virtualization (Aletheia as host) | Not started | No VMX/SVM/EL2 host support |
| Silicon overclocking | Contract and grant-only band exist (ADR-076/184); no frequency or voltage ever changed on real silicon | QEMU has no actuator |
| Per-user authority | Fixed console roles (ADR-247) and per-object ownership for operators (ADR-250); no read permissions or groups, and programs a session runs carry no account | Least privilege between people |
| System-1 decision latency | Native runtime 1.1x (CPU, ADR-251) / 1.4x (Metal) slower per decision than torch (was 2.3x); loads 70x faster, uses less memory | A first-class runtime should not be the slower one |
| Training | Fine-tuning still needs Python + torch | Only serving is native |
| Userland | Seeded Rust programs over a small syscall ABI; no POSIX layer, no package manager | Workstation and development workloads need an ecosystem |

None of these is hidden behind a passing gate: the ADR that would close each one is the next work.
