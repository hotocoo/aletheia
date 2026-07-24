> **Status (Aletheia team, 2026-07-22)** — tracking the 10 issues below against the roadmap. This is a
> second external audit; entries update as work lands (single source of truth = `STATUS.md` +
> `docs/TRACEABILITY.md`).
>
> - **#1 Target-specific traceability — ✅ ADDRESSED.** `REQ-USER-*` and `REQ-MEM-*` split into
>   per-target rows, each naming its own VM gate (`vm-e2e.sh` / `smoke-test.sh` / `vm-e2e-riscv.sh`);
>   a target-specific regression can no longer escape the gate. See TRACEABILITY "How it is checked".
> - **#2 Cross-target conformance suite — ✅ ADDRESSED.** `scripts/conformance.sh` (REQ-CONF-001) boots
>   all three targets and asserts each proves the SAME 10 core semantic behaviors (cross-AS IPC,
>   grant-table cap-gate/zero-copy/revoke, blocking IPC, priority inheritance). Spec'd on named
>   behaviors with per-arch invariants declared as EXTENSIONS (x86-64 46 vs aarch64/RISC-V 53 total —
>   honest MMU-off→on difference), NOT count equality. All three PASS. Follow-on: CI job; grow contract.
> - **#3 IPC transfer through the real per-target user-mode path — ⏳ open** (kernel-core policy done;
>   cross-AS `send_transfer` wiring per target is the remaining integration).
> - **#4 SMP / concurrency cliff — ⏳ open** (REQ-SMP-001; blocked on #9 concurrency spec first).
> - **#5 Priority inheritance end-to-end VM-tested — ✅ ADDRESSED (all three targets).** REQ-IPC-009
>   proved through the REAL blocking-IPC path: a HIGH receiver blocks on the endpoint a LOW task
>   services, donates its priority, and the boosted LOW is dispatched ahead of a Ready MEDIUM
>   (`…/usermode.rs::run_priority_ipc`, invariants 20-22). Built on REQ-IPC-010 real blocking IPC.
>   Proved on aarch64 (`scripts/vm-e2e.sh`), RISC-V (`scripts/vm-e2e-riscv.sh`), and x86-64
>   (`kernel-x86_64/scripts/smoke-test.sh`) — directly reducing the #2 divergence risk.
> - **#6 Real secure-boot chain — ⏳ partial, advanced.** Component signatures now **asymmetric**
>   (REQ-BOOT-002 delivered): ed25519, public-key-only verifier (cannot forge) + root→signing-key
>   hierarchy (`aletheia/src/provenance.rs`). STILL hardware-bound (REQ-BOOT-001): firmware→kernel
>   measured chain, TPM/secure-enclave root of trust, anti-rollback (needs persistent secure storage).
> - **#7 Persistent storage substrate — ⏳ partial, advanced.** Crash-consistent journaled block store
>   delivered (REQ-STOR-002, `kernel-core/src/storage.rs`): WAL over a `BlockDevice` seam, proved by a
>   crash-at-every-prefix sweep + torn-record/torn-journal rejection. The **real storage driver now
>   exists**: a virtio-blk driver over modern virtio-mmio (REQ-DRV-003, `kernel/src/virtio.rs`)
>   implements that `BlockDevice` trait and is VM-gated (`scripts/vm-e2e.sh`) — the journal's
>   commit+recover is proved over real emulated storage, not just the in-memory device. STILL open
>   (REQ-STOR-001): the encryption-at-rest layer over the device and the semantic store on persistence.
> - **#8 Fault supervision as a kernel primitive — ⏳ deferred** (REQ-REL-001 / ADR-026).
> - **#9 Capability concurrency spec before SMP — ✅ ADDRESSED (spec + hosted-proved primitive).**
>   REQ-CAP-006 / ADR-027: the authorize→execute atomicity guarantee is specified (Option A — one
>   critical section; Option B epochs documented, not built) and implemented as
>   `CapEngine::with_authorization` (+ `authorize`). Proved under real `std::thread` contention
>   (`kernel-core/tests/cap_concurrency.rs`): the naive check-then-act is stale by construction, the
>   disciplined primitive never commits under a revoked cap, and revocation is permanent. Proves the
>   MECHANISM on host threads; wiring it into each target's real trap path (+ TLB-shootdown /
>   atomic-ordering audit) is the SMP integration still deferred under #4.
> - **#10 Hardware-diversity ladder — ⏳ deferred** (QEMU is the current top rung; real boards later).
>
> Recent progress feeding these: all three targets now DRIVE the shared `kernel_core::sched::RoundRobin`
> (one scheduler, three backends — directly reduces the #2 divergence risk).

Highest-priority issues I found
1. 🔴 Traceability can prove that evidence files exist, not that the correct target executes them
docs/TRACEABILITY.md marks the generic user-mode requirements as delivered, but the VM-gate mapping is not fully target-specific. Meanwhile, the architecture gap register says x86-64 user mode is gated through kernel-x86_64/scripts/smoke-test.sh, while the traceability rows primarily reference the generic aarch64/RISC-V gates.
Risk: a target-specific regression could compile successfully and still escape the requirement gate.
Recommendation: make the matrix explicitly target-specific:
REQ-USER-AARCH64-001
  implementation: kernel/src/usermode.rs
  test: ...
  VM gate: scripts/vm-e2e.sh

REQ-USER-X86-64-001
  implementation: kernel-x86_64/src/usermode.rs
  test: ...
  VM gate: kernel-x86_64/scripts/smoke-test.sh

REQ-USER-RISCV64-001
  implementation: kernel-riscv64/src/usermode.rs
  test: ...
  VM gate: scripts/vm-e2e-riscv.sh
I attempted to raise this as a GitHub issue automatically, but the connected GitHub integration returned 403 / resource not accessible, so no issue was created.
2. 🔴 The biggest remaining architectural risk is cross-architecture divergence
The repository has successfully extracted the shared capability/security spine and scheduler policy into kernel-core, but the architecture gap register still identifies architecture-specific implementations for:
address spaces
memory management
process lifecycle
interrupt handling
context switching
IPC fast paths
This is exactly where independent kernels can silently emerge.
The danger is:
AArch64 implementation
        ≠
x86-64 implementation
        ≠
RISC-V implementation
even though all three claim to implement the same Aletheia semantics.
Recommendation: continue extracting shared contracts and add a cross-target conformance suite, not just shared unit tests.
For every target, test the same semantic contract:
create process
→ map memory
→ execute user code
→ syscall
→ capability check
→ IPC
→ block
→ preempt
→ resume
→ terminate
The important thing is not merely that each architecture passes its own tests. It is that the same behavioral specification passes identically on every architecture.
3. 🔴 IPC capability transfer is not yet fully wired through the real user-mode path
The shared kernel-core IPC layer now has:
capability transfer
attenuation
bounded queues
notifications
timeout/deadline semantics
cancellation
trace/replay
That is excellent.
However, the gap register explicitly still identifies the need to wire capability transfer into each target's cross-address-space user-mode path.
This is a critical distinction:
Hosted kernel-core IPC
        ↓
works

Real process A
        ↓
kernel trap
        ↓
kernel IPC
        ↓
process B
        ↓
capability transfer
        ↓
works on every target
The second path is what matters for the actual OS.
Risk: the abstract IPC semantics may be stronger than the real hardware syscall boundary.
4. 🔴 SMP is the next major security and correctness cliff
The current design is still fundamentally single-core in important areas.
When SMP arrives, you will need to audit:
capability revocation races
concurrent delegation
IPC queue races
scheduler state
task lifecycle
address-space destruction
page-table updates
TLB shootdowns
interrupt delivery
cross-core wakeups
shared event logging
storage transactions
The current model:
check capability
        ↓
perform action
        ↓
record event
becomes dangerous if another CPU can concurrently:
revoke capability
destroy object
unmap memory
terminate process
between those operations.
You need explicit concurrency semantics for every security-sensitive operation.
I would prioritize:
SMP
 ↓
concurrency model
 ↓
lock hierarchy
 ↓
atomic capability semantics
 ↓
revocation race tests
 ↓
cross-core IPC tests
 ↓
TLB shootdown correctness
before adding large amounts of higher-level functionality.
5. 🔴 Priority inheritance is designed but not yet integrated into actual blocking IPC
The repository has a priority-aware scheduler and priority donation policy, but the traceability matrix still marks it partial.
The important question is not:
Can the scheduler calculate a donated priority?
It is:
Does a real high-priority process blocked on a real IPC endpoint cause the lower-priority holder to inherit priority across the actual kernel path?
The required end-to-end scenario is:
High-priority task
        ↓ waits for
IPC endpoint
        ↓ owned/held by
Low-priority task
        ↓
priority donation
        ↓
Low-priority task executes
        ↓
releases resource
        ↓
High-priority task resumes
This should eventually be VM-tested, not only hosted-tested.
6. 🟠 The current secure boot work is not yet secure boot
The component-signature work is a good first slice, but the current design is fundamentally application/component provenance verification, not yet a complete hardware boot chain.
The actual chain still needs to become:
Firmware / ROM root
        ↓
Bootloader
        ↓
Kernel
        ↓
Kernel configuration
        ↓
System services
        ↓
Drivers
        ↓
Components
        ↓
Applications
The current HMAC trust model also needs careful treatment for a real OS:
Where does the trust key originate?
How is it provisioned?
How is it protected from replacement?
How does key rotation work?
How does rollback protection work?
What happens if the trust store is corrupted?
How are keys bound to a specific machine?
What prevents an attacker with disk write access from replacing the trust anchor?
The hosted signature layer is useful, but the real security boundary must eventually move to hardware-backed roots of trust where available.
7. 🟠 Storage is currently a major missing OS substrate
The semantic store is encrypted and durable in the hosted System Core, but the actual OS still needs a complete persistence stack:
NVMe / storage device
        ↓
driver
        ↓
block layer
        ↓
partitioning
        ↓
filesystem / object store
        ↓
encryption
        ↓
content-addressed store
        ↓
semantic store
        ↓
world model
The most important missing properties are:
crash consistency
atomic commit
recovery after power loss
corruption detection
rollback
snapshots
key lifecycle
secure key storage
boot-state persistence
Aletheia's semantic architecture makes this more important than in a conventional OS, because the world model, memory, audit history, and AI context will all become persistent security-sensitive state.
8. 🟠 Fault containment and supervision needs to become a first-class kernel primitive
The architecture gap register correctly identifies fault recovery/supervision as deferred.
A real Aletheia system should not have:
driver crashes
    ↓
system crashes
or:
AI service crashes
    ↓
entire experience layer dies
The intended model should be something like:
Supervisor
 ├── AI Runtime
 ├── Storage Service
 ├── Network Service
 ├── GPU Service
 └── Driver Service
with:
service failure
    ↓
detect
    ↓
contain
    ↓
revoke capabilities
    ↓
restart / replace
    ↓
restore state
    ↓
audit
The particularly important security question is:
When a compromised service is restarted, how do you guarantee it does not retain authority from the previous instance?
That needs explicit capability-lifecycle semantics.
9. 🟠 The capability model needs a formal concurrency specification before SMP
The capability system is one of the strongest parts of the repo. The adversarial tests already cover:
confused deputy
capability laundering
stale capabilities
revocation
scope confinement
wildcard boundary problems
The next class of bugs will be time-of-check/time-of-use under concurrency.
Example:
CPU 0: evaluate(capability) → ALLOW
CPU 1: revoke(capability)
CPU 0: execute()
What is the exact guarantee?
Possible designs include:
Option A — Atomic authorization + execution
The kernel performs both under one synchronization boundary.
Option B — Generation/version tokens
capability generation = 42
authorize → generation 42
revoke → generation 43
execute rejects stale 42
Option C — Object references with revocation epochs
The important thing is that this must be formally defined before SMP implementation spreads assumptions throughout the kernel.
10. 🟠 The VM tests are good, but hardware diversity is still narrow
QEMU VM testing is essential, but it does not prove:
real cache behavior
real interrupt controller quirks
real DMA
IOMMU behavior
real firmware behavior
real multicore startup
device reset behavior
hardware memory ordering edge cases
The current architecture should eventually have a hardware-validation ladder:
Hosted unit/property tests
        ↓
QEMU single-core
        ↓
QEMU SMP
        ↓
QEMU device model
        ↓
real development board
        ↓
real x86-64 PC
        ↓
real ARM machine
        ↓
real RISC-V board
The most important issue I would raise next
If I had to choose only one new issue, it would be:
Build a cross-architecture conformance and traceability system that proves the same Aletheia kernel semantics on AArch64, x86-64, and RISC-V.
Because right now the biggest systemic risk is no longer “does Aletheia have security ideas?”
It clearly does.
The risk is:
Aletheia semantic contract
        ↓
three different hardware implementations
        ↓
subtle behavioral divergence
        ↓
security boundary differs by CPU architecture
The ideal long-term test structure is:
kernel-core/conformance/
    capability.rs
    ipc.rs
    process.rs
    memory.rs
    scheduler.rs
    faults.rs

        ↓ same contract

AArch64 VM
x86-64 VM
RISC-V VM
Every target should be required to pass the same semantic contract.
Overall audit verdict
Architecture: Strong and unusually coherent for an early OS project.
Security model: Strong conceptually, with meaningful adversarial regression testing.
Kernel maturity: Promising but still early; the biggest missing pieces are SMP, devices, storage, recovery, and real secure boot.
Main systemic risk: architecture-specific divergence and concurrency bugs as the system moves from single-core proof-of-concept toward a real multicore OS.