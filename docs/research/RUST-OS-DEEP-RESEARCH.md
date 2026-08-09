# Deep research — building a whole new operating system in majority Rust

**As of:** 2026-08-09. **Status:** research input, not a claim of delivery. Nothing in this document
asserts that Aletheia has any property described here; where a finding lands on Aletheia it is stated
as a *consequence for this repository*, and the corresponding work is either already in
`STATUS.md` or open in `docs/gap/ARCHITECTURE-GAPS4-REGISTER.md`.

This is the survey that should have preceded implementation, written down so that later architectural
choices can be argued against evidence rather than taste. It answers five questions:

1. What does the 2026 field of Rust operating systems actually look like, and what has each project
   proved?
2. What does Rust buy a kernel, precisely — and where does the guarantee stop?
3. What are the load-bearing structural decisions when the majority of the OS is Rust?
4. What is specifically hard about an *AI-native* OS, and what has the research literature settled?
5. What does this mean for Aletheia — which choices are vindicated, which are exposed?

---

## 1. The 2026 landscape

Five projects matter, and they disagree with each other in instructive ways. The disagreement is not
about whether Rust belongs in a kernel — that argument is over — but about **where the isolation
boundary goes** once the language already provides memory safety.

### 1.1 Redox — the orthodox Rust microkernel

[Redox](https://en.wikipedia.org/wiki/RedoxOS) is a from-scratch microkernel written entirely in Rust,
with drivers, filesystems, and the display server in userspace, communicating over a URL-like scheme
namespace. In 2026 it ported Xfce to run on top of it and gained an EEVDF scheduler and further
capability-based security work.

What Redox proves is the *unglamorous* thing: a Rust microkernel plus a userspace can reach the point
where third-party desktop software runs unmodified. Its cost is the classical microkernel cost — every
driver interaction is IPC, and IPC performance becomes the whole-system performance story.

**Consequence for Aletheia:** Redox is the closest structural sibling and the honest yardstick. Its
relative maturity is a reminder that the distance between "the invariants hold in a VM" and "a
desktop runs" is measured in years of driver and userspace work, not in kernel cleverness. Aletheia's
`docs/MATURITY.md` already refuses to claim otherwise; that refusal is correct and should not soften.

### 1.2 Asterinas — the framekernel, the most interesting new idea

[Asterinas](https://github.com/asterinas/asterinas) (0.18, June 2026; backed by Ant Group, Intel,
Peking University, SUSTech) introduces the **framekernel**. The kernel runs in a single address space
like a monolith — so it keeps monolithic performance — but Rust's ownership model partitions the
kernel source into:

- a small privileged **Framework**, which is permitted `unsafe`, and
- a large body of **Services**, which must be pure safe Rust.

The isolation boundary is therefore *the language*, not the MMU. Asterinas's 2026 priority is
production readiness on x86-64 standard and confidential VMs, and 0.18 shipped verified-working
packages (QEMU, Firefox, Codex), initial NVMe support, and groundwork for Confidential/Kata
Containers.

This is the sharpest architectural claim in the field: **if the safe/unsafe split is enforced and the
unsafe surface is small and audited, you do not need to pay for address-space crossings to get the
isolation property you actually wanted.** It is also the claim with the most to lose — it stands or
falls on whether the Framework's `unsafe` is genuinely sound, because a single unsoundness there is
unmediated by hardware.

**Consequence for Aletheia:** Aletheia is not a framekernel — it uses ring 3 and per-process page
tables (`kernel-x86_64/src/usermode.rs`, ADR-030..034) — but the framekernel argument applies
*inside* the kernel regardless. The discipline worth importing is the **explicit, enumerated unsafe
surface**: a written list of every `unsafe` block, what invariant makes it sound, and a test that
attacks that invariant. Aletheia has this for behaviors (`docs/INVARIANT-CONTRACTS.md`) but not yet
as an `unsafe`-block census. That is a concrete, cheap, high-value addition.

### 1.3 Theseus — single address space, single privilege level

Theseus runs all components in a single address space at a single privilege level and derives its
isolation entirely from the language, going further than Asterinas by removing privilege levels too.
Its distinguishing research contribution is **intralingual design**: expressing OS-level resource
lifetimes in the type system so that the compiler, not the kernel, is the enforcer — and it has been
extending toward running legacy components in a WASM sandbox.

**Consequence for Aletheia:** the WASM-for-legacy direction is convergent evidence for
ADR-008/ADR-014. Aletheia already uses WASM components as the capability-secure application boundary;
Theseus arriving at WASM from the opposite direction (a language-isolated kernel needing a sandbox for
untrusted native code) suggests the WASM component boundary is a stable answer rather than a fashion.

### 1.4 seL4 — the verification ceiling, and it is not written in Rust

[seL4](https://sel4.systems/) remains the reference point for assurance: ~8,700 lines of C and ~600 of
assembler, with a machine-checked proof that the implementation refines an abstract specification, so
the kernel provably never crashes and never performs an unsafe operation. It is worth being precise
about what that costs and buys, because "formally verified" is used loosely:

- The proof is about **functional correctness against a spec** plus derived integrity and
  confidentiality results. It is not a claim that the spec is the one you wanted.
- The proof effort was on the order of 20 person-years for a kernel of under 10 kLOC. The ratio
  matters more than the total: **verification cost scales superlinearly with kernel size**, which is
  the real argument for microkernels, independent of Rust.
- seL4 has Rust *bindings* and Rust userspace support (with breaking changes across 16.0.0), but the
  verified kernel itself is C. Rust does not currently have a verified kernel of comparable standing.

**Consequence for Aletheia:** the seL4 result sets the price of the assurance Aletheia's documents
gesture at. A capability microkernel in Rust starts closer to the goal than C did — the memory-safety
lemmas that consumed much of the seL4 effort are discharged by the type system for safe code — but
"closer" is not "there", and the honest posture is the one `MATURITY.md` already takes. The
actionable part is **keeping the trusted computing base small enough that verification stays possible
later**, which is an argument against feature growth in `kernel-core`.

### 1.5 Rust for Linux — the incrementalist counter-argument

Rust is now a first-class kernel language in mainline Linux, with driver-core, DMA, IOMMU, GPIO, DRM
and serial-bus abstractions landing through 2026, and the design philosophy stated explicitly:
**minimize the unsafe surface the driver author touches**, so that if the abstraction layer's
`unsafe` is right, driver code can be entirely safe. The
[DMA-coherent-allocation debate](https://lwn.net/Articles/1006805/) is the instructive part — the
friction was never technical feasibility but maintainership and the cost of a second language in a
shared subsystem.

**Consequence for Aletheia:** the Linux experience is the best available evidence on *driver-layer*
Rust ergonomics, and its lesson is directly transferable — the value is concentrated in the
abstraction crate, not in the drivers. Aletheia's `kernel-core/src/virtq.rs` + `dma.rs` split already
follows this shape (the DMA-visibility registry is the abstraction; `virtioblk`/`virtionet` are
consumers). ALET-P1-018 correctly records that this is a *software* boundary and that a device which
invents its own addresses needs an IOMMU/SMMU — Linux's Rust IOMMU abstractions are the reference
implementation to study when that row is worked.

---

## 2. What Rust actually buys a kernel — and where it stops

### 2.1 What is genuinely eliminated

For the code the compiler checks, the classes of bug that dominate kernel CVE histories are gone by
construction: use-after-free, double-free, buffer overrun, data race on shared mutable state, null
dereference, and iterator/aliasing invalidation. This is not a probabilistic improvement; it is a
type-system guarantee, and it is the entire reason a five-person project can plausibly attempt a
kernel at all.

### 2.2 What is not eliminated — the four leaks

1. **`unsafe` is a hole, and a kernel is full of holes.** Every MMIO access, page-table write, port
   I/O, trap frame, DMA buffer, and `ExitBootServices` handoff is `unsafe` by necessity. Rust does not
   make these correct; it makes them *findable*. The engineering value is that they are enumerable —
   which is only realized if someone actually enumerates them.
2. **Safety ≠ correctness.** A deadlock, a lost wakeup, a scheduler that starves, a journal that
   claims durability it does not have — all are perfectly safe Rust. Aletheia's own history is the
   evidence: the register records real defects found by *writing invariants*, not by compiling
   (`sched.rs` inventing a task from a stray id; the null page mapped by every boot identity map).
3. **The hardware is outside the model.** Rust's memory model does not describe a device performing
   DMA into your frames, a TLB entry surviving a CR3 load because firmware marked it global (a real
   bug this repo hit, ALET-P1-031), or speculative execution. These are firmware/architecture
   contracts, and they are where kernel work actually lives.
4. **Nightly and `build-std` are load-bearing.** Bare-metal Rust today needs nightly for
   `abi_x86_interrupt`, `build-std`, and target-specific features, and `x86_64-unknown-uefi` /
   `*-unknown-none` targets need `rust-src`. A floating nightly means the compiler under the gates
   changes without a commit — which is precisely why a **dated pin** is not bureaucracy but a
   correctness property of the test evidence (see `docs/TOOLCHAIN.md`, ALET-P2-001).

### 2.3 The verification tooling that closes part of the gap

The 2026 Rust verification landscape is no longer research-only, and the tools are complementary
rather than competing:

| Tool | Technique | Best at | Cost |
|------|-----------|---------|------|
| **Miri** | runtime UB detection (interpreter) | finding UB in `unsafe` under real test inputs | near-zero adoption cost; **cannot prove absence**; no bare-metal targets |
| **Kani** | bounded model checking (CBMC backend) | verifying `unsafe` blocks and arithmetic/UB classes exhaustively over bounded inputs | moderate; harness per property |
| **Verus / Creusot / Prusti** | deductive verification | rich functional properties (a scheduler really is fair) | high — separation logic / ghost state, specialist skill |
| **MIRAI** | abstract interpretation | whole-crate taint/state properties | moderate, noisier |

The `verify-rust-std` campaign is the scale data point: a community effort produced **16,748 automatic
proof harnesses, of which 11,970 verified** against Kani's supported classes of undefined behavior.
The lesson is that *bounded* verification is now tractable at library scale, while *deductive*
verification remains specialist.

**Consequence for Aletheia:** there is a cheap, high-leverage move available — run **Kani** against
`kernel-core`'s pure-logic modules (`vmaddr`, `frameown`, `ptreclaim`, `faultclass`, `dma`,
`reentry`), which are deliberately `no_std`-but-host-testable and already have property tests. These
modules are exactly the shape Kani handles well: small, total, integer-and-bitfield logic where the
existing tests sample and Kani would exhaust. This maps onto the open rows ALET-P2-008
(fault-injection coverage) and ALET-P2-010 (larger property campaigns) without inventing new scope.

---

## 3. Structural decisions when the OS is majority Rust

### 3.1 Where the isolation boundary goes

Four defensible answers, and the field currently holds all four:

| Boundary | Exemplar | Buys | Costs |
|----------|----------|------|-------|
| MMU + privilege levels | Redox, Aletheia, seL4 | hardware-enforced; survives a language-level unsoundness | IPC on every crossing |
| Language only, single address space, ring 0 | Theseus | no crossing cost at all | one unsound `unsafe` is total |
| Language, with a privileged Framework / unprivileged Services split | Asterinas | monolith performance, audited unsafe core | soundness of Framework is load-bearing |
| WASM sandbox above the kernel | Theseus (legacy), Aletheia (components) | portable, capability-shaped, fine-grained | interpreter/JIT cost; resource model must be built (ALET-P1-021) |

The decision is not "which is best" but **which failure you are willing to own**. Hardware boundaries
fail closed and cost throughput; language boundaries cost nothing and fail totally. A system that
claims capability security for *untrusted* code — which is Aletheia's whole premise, since the AI is
untrusted — should keep the hardware boundary, because the threat model includes an adversary that is
trying to find the unsound block.

### 3.2 The `no_std` / `alloc` / `std` layering, and why the host build matters

The idiomatic layering is three crates deep: a `no_std` core of pure logic, a `no_std + alloc` layer
that needs an allocator, and target crates that own the `unsafe`. Aletheia's `kernel-core` is exactly
this, and the payoff is the one the repo already banks: **the same invariant source runs on the host
under `cargo test` and inside three kernels**, so a property proved by a fast host test is the
property the VM gate re-proves on real hardware paths.

The trap is the *reverse* direction — a supposedly portable host crate reaching for platform APIs.
`aletheia/src/service.rs` importing `std::os::unix::net::{UnixListener, UnixStream}` is that trap: it
makes the "no Linux/macOS/POSIX imports" doctrine in `README.md` false at the one place the doctrine
is most load-bearing (the Service API / IPC boundary, ADR-016), and it means the hosted Core does not
compile on a Windows host at all. The fix is not a `#[cfg]` ladder but an **Aletheia-owned transport
seam** with per-host backends behind it — which is what the architecture claims already exists.

### 3.3 Boot and firmware

UEFI is now the pragmatic default for x86-64: `x86_64-unknown-uefi` is a supported target, firmware
gives you a memory map, a framebuffer, and a loaded-image descriptor, and `ExitBootServices` is a
clean, auditable moment at which the OS takes the machine. The alternative — a hand-rolled BIOS
bootloader, as in `rust-osdev/bootloader` — buys control at the cost of owning early-boot assembly.

Two firmware facts are worth writing down because both have bitten this repository or will:

- **The firmware's page tables are not yours.** OVMF hands over a W+X identity map. Building your own
  tree and *activating* it (`kmap::activate`, ALET-P1-031) is the difference between claiming W^X and
  having it — and the CR4.PGE cycle across the CR3 write is required because firmware marks its
  mappings global and a global TLB entry survives a CR3 load.
- **The `.efi` is a PE image, not ELF.** There are no `linker.ld` symbols; section bounds come from
  the image's own PE section table via `LoadedImage`. Any tooling that assumes ELF symbols silently
  produces a wrong map on this target.

### 3.4 Testing a kernel you cannot debug

The validation pyramid that works for a from-scratch OS, in increasing cost and decreasing frequency:

1. **Host property tests** over `no_std` logic crates — milliseconds, run on every save.
2. **Bounded model checking** (Kani) over the same crates — minutes, run in CI.
3. **In-kernel invariant suites** — the same assertions re-proved in kernel space and reported over
   a serial line, with a machine-checkable process exit code.
4. **VM boot gates** — QEMU with a watchdog, asserting exit code *and* every marker.
5. **Cross-architecture conformance** — every target must prove the *same* semantic contract, which
   is what stops a target from quietly diverging.
6. **A second, independent hypervisor** — see §5.2; this is the layer Aletheia is missing.
7. **Real hardware.**

Levels 3 and 4 have a subtle failure mode this repo has already documented: a gate that *skips* when
a device is absent looks identical to a gate that *passes*. The doctrine — never a silent pass; an
absent capability must print SKIP and be named again in the summary — is worth stating as a general
rule of kernel CI, not a local convention.

### 3.5 Supply chain

`cargo` is a strength and a liability. `--locked` everywhere, a committed `Cargo.lock` per crate, an
SBOM derived from `cargo metadata` (timestamp-free, so an unchanged lockfile yields a byte-identical
file), `cargo audit` that *runs* rather than silently skips, and a permissive-license allow-list are
the minimum. Aletheia has all five (ALET-P2-002/004/005). Reproducible builds as a *release* property
(ALET-P2-006) remain open, and are harder than they look on a nightly toolchain.

---

## 4. The AI-native part, and what the literature settled in 2026

This is where Aletheia is making a research bet, not an engineering one, so it is worth checking the
bet against what has been published.

The 2026 literature converged, from several directions, on the same structural claim Aletheia's
pipeline encodes:

- **[AgenticOS](https://arxiv.org/abs/2606.21129)** reframes the OS from a *resource manager* into an
  **intent filter**: agents submit structured intent declarations, and the system synthesizes a
  least-privilege environment with mandatory mediation, auditing, and information-flow constraints.
- **[Agent libOS](https://arxiv.org/html/2606.03895)** places the boundary at the agent-runtime layer
  and enumerates agent-native resources — tool tables, object memory, checkpoints, **human approval
  queues**, provider endpoints — with capability-bearing calls, lineage-preserving evolution, and
  audit of *committed external effects*.
- **[Toward Securing AI Agents Like Operating Systems](https://arxiv.org/html/2605.14932v1)** states
  the threat plainly: once an agent is compromised by prompt injection or a malicious tool output, an
  attacker composes ordinary POSIX primitives into behavior far beyond the user's task authorization.
  The vulnerability is not the model; it is **ambient authority**.
- **[AIOS](https://arxiv.org/html/2312.03815v2)** takes the opposite tack — LLM as kernel, agents as
  applications — with kernel-level scheduling, context management, and tool orchestration.

Three findings are load-bearing for any AI-native OS:

1. **Ambient authority is the vulnerability.** Every paper reaches this independently. A capability
   system with no ambient authority is not a stylistic preference; it is the only structure in which
   prompt injection degrades to "the model proposed something and was denied".
2. **Authority and governance must be separate axes.** Agent libOS lists human approval queues as a
   first-class resource *distinct from* capability-bearing calls. Aletheia's ADR-015 split — a
   capability answers *may this subject do this at all*, policy answers *must a human approve it* —
   is the same finding, arrived at earlier. This is the most clearly vindicated decision in the
   repository.
3. **The audited unit is the committed external effect, not the model's output.** Logging prompts and
   completions is theater; what must be immutable and replayable is the effect that escaped the
   system. Aletheia's provenance event at the end of the pipeline is the right unit.

Where the literature is *ahead* of Aletheia:

- **Information-flow constraints.** AgenticOS synthesizes an environment with IFC, not merely a
  capability check. Aletheia authorizes each entity into context (`entity.read` before it enters
  `AiContext`) but does not track flow *out* — nothing prevents an authorized read from being
  laundered into an effect the subject could not have requested directly. This is a genuine gap and is
  adjacent to the open rows ALET-P1-027 (scope composability) and ALET-P1-030 (encrypted
  content-addressing identity semantics).
- **Resource models for sandboxed agent code.** ALET-P1-021 (WASM resource model beyond fuel: memory,
  table, stack, wall-clock) is exactly the row the agent-OS papers treat as table stakes.
- **Tool-supply-chain provenance.** ALET-P1-023/024 (component installation verification, dependency
  resolution as a security boundary) matches "malicious tool output" as the primary compromise vector.

Where Aletheia is *ahead* of the literature: almost all of this work runs as a userspace framework on
Linux, and therefore inherits ambient authority from the host it is trying to eliminate. Building the
capability engine **below** the application boundary — in an OS that owns its own kernel, address
spaces, and IPC — is the structurally stronger position, and is the actual justification for the
from-scratch decision in ADR-001.

---

## 5. Consequences for Aletheia

### 5.1 Vindicated

- **Capability-first, no ambient authority** (ADR-003) — independently reached by four 2026 papers.
- **Authority/governance separation** (ADR-015) — matches Agent libOS's approval-queue-as-resource.
- **WASM components as the application boundary** (ADR-008/014) — Theseus converged on the same
  answer from the opposite direction.
- **A shared `no_std` invariant core re-proved on every target** (`kernel-core` + conformance) — this
  is the mechanism that makes a three-target claim checkable rather than three separate claims.
- **Never-a-silent-pass gates** — the discipline that makes the evidence mean anything.

### 5.2 Exposed

1. **Single-hypervisor qualification.** Every boot gate runs on QEMU. QEMU is one implementation of
   the platform contract, and a kernel that boots only on QEMU has proved "correct against QEMU", not
   "correct against the architecture". ADR-013 already frames VM-then-hardware qualification; a
   **second, independent hypervisor** is the missing rung. Oracle VirtualBox is the right next one:
   different firmware (its own EFI, not OVMF), different chipset model, different storage stack
   (AHCI/NVMe/VirtIO-SCSI — notably **no virtio-blk**), and no `isa-debug-exit` device. Each of those
   differences is a place where a QEMU-shaped assumption would be caught. This is the substance of
   ADR-046 and `scripts/vm-e2e-vbox.sh`.
2. **No `unsafe` census.** The Asterinas discipline applied to Aletheia: an enumerated list of every
   `unsafe` block with its soundness argument and the test that attacks it.
3. **`std::os::unix` in the hosted Core.** The doctrine says no POSIX imports; `service.rs` imports
   POSIX. The hosted Core does not build on a Windows host, which means the "temporary development
   environment" is in fact a *Unix* development environment.
4. **Floating nightly.** ALET-P2-001. The gates do not know which compiler they ran under.
5. **No information-flow tracking** in the Context Engine (§4).
6. **Polling drivers.** `MATURITY.md` already lists this; ADR-045 fixed the console specifically.

### 5.3 What this research says to do next, in order

1. Pin the dated nightly (ALET-P2-001) — cheap, and it is a precondition for every other piece of
   evidence meaning anything.
2. Second hypervisor: VirtualBox boot gate (ADR-046) — the highest-value new *evidence*, because it
   tests the assumptions no existing gate can.
3. Portable Service transport seam — makes the doctrine true and the host crates cross-platform.
4. `unsafe` census + Kani on `kernel-core` — the highest-value new *assurance* per unit of effort.
5. Information-flow constraints in the Context Engine — the highest-value new *architecture*, and the
   one place the 2026 literature is genuinely ahead.

---

## Sources

- [Asterinas](https://github.com/asterinas/asterinas) · [Asterinas 0.18 release coverage](https://www.phoronix.com/news/Asterinas-0.18)
- [Redox OS](https://en.wikipedia.org/wiki/RedoxOS) · [Rust OS comparison](https://github.com/flosse/rust-os-comparison)
- [seL4](https://sel4.systems/) · [seL4: Formal Verification of an OS Kernel (CACM)](https://cacm.acm.org/research/sel4-formal-verification-of-an-operating-system-kernel/)
- [Rust for Linux (kernel docs)](https://rust.docs.kernel.org/kernel/) · [Resistance to Rust abstractions for DMA mapping (LWN)](https://lwn.net/Articles/1006805/)
- [`*-unknown-uefi` platform support](https://doc.rust-lang.org/rustc/platform-support/unknown-uefi.html) · [rust-osdev/bootloader](https://github.com/rust-osdev/bootloader) · [Writing an OS in Rust](https://os.phil-opp.com/minimal-rust-kernel/)
- [Kani: A Model Checker for Rust](https://arxiv.org/pdf/2607.01504) · [Verifying the Rust Standard Library](https://arxiv.org/html/2606.17374v1) · [Surveying the Rust Verification Landscape](https://arxiv.org/pdf/2410.01981)
- [AgenticOS](https://arxiv.org/abs/2606.21129) · [Agent libOS](https://arxiv.org/html/2606.03895) · [Toward Securing AI Agents Like Operating Systems](https://arxiv.org/html/2605.14932v1) · [AIOS](https://arxiv.org/html/2312.03815v2)
