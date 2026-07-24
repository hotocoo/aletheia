//! SMP — secondary-CPU bring-up and the first REAL multi-core execution regime (REQ-SMP-002).
//!
//! WHY THIS MATTERS: everything before this wave ran on one core, and the capability engine's
//! safety argument leaned on the borrow checker serializing `&self` readers against `&mut self`
//! revokers — a guarantee that evaporates the moment a second CPU exists (GAPS2 #9, ADR-027).
//! This module boots the other CPUs via the PSCI `CPU_ON` firmware call, gives each its own
//! stack + per-CPU identity (`TPIDR_EL1`), turns its MMU on over the SAME kernel tables, and then
//! proves — on real concurrently-executing cores, not host threads — that the cross-core
//! substrate holds: exact atomic accounting, release/acquire message passing, the ADR-027
//! atomic authorize+execute primitive under live cross-core revocation, and a GICv2 SGI
//! inter-processor interrupt delivered end-to-end.
//!
//! SCOPE (contract-honest, ADR-010/ADR-019): aarch64 dev backend on QEMU `virt` with PSCI (HVC
//! conduit). This is CPU bring-up + the concurrency substrate + the ADR-021 Phase 2 scheduling
//! policy on real cores (per-CPU run queues + work stealing via `kernel_core::smpsched`,
//! REQ-SMP-003) + cross-core TLB shootdown (Phase 6: the `kernel_core::shootdown` all-acknowledged
//! barrier + the aarch64 broadcast `tlbi …is`, REQ-SMP-004 / ADR-021 Phase 3). Preemptive
//! cross-core task *migration*, CPU affinity, and the lock-hierarchy / atomic-ordering audit remain
//! the named next slices under REQ-SMP-001, which stays partial. With `-smp 1` (or a PSCI without
//! more CPUs) the suite skips green, exactly like the virtio driver with no disk; the VM gate boots
//! `-smp 4` and asserts the full marker.
//!
//! CONCURRENCY RULES (load-bearing):
//! * Secondaries NEVER print — the PL011 writer is not serialized; core 0 narrates everything.
//! * Every capability-engine access happens under one [`SpinLock`] — that lock is what makes
//!   `with_authorization`'s single-critical-section contract real across cores (ADR-027 Option A).
//! * All allocation on secondaries happens inside that same lock (the bump allocator is CAS-safe
//!   as of this wave, but engine mutation must be serialized anyway).
//! * Liveness waits are PROGRESS-GATED with a wall-clock deadline — never a fixed spin count
//!   (a fixed spin races thread wake-up and flakes; proven in kernel-core's cap_concurrency).
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use alloc::boxed::Box;

use crate::frames;
use crate::hal::{ActiveHal, Hal};
use crate::vm;
use kernel_core::sched::TaskContext;
use kernel_core::shootdown::{Invalidation, TlbShootdown};
use kernel_core::smpsched::{AffinityMask, Dispatch, SmpSched};
use kernel_core::spine::{CapEngine, CapToken, Constraints, Scope, Target};
use kernel_core::sync::SpinLock;

// --- PSCI (Power State Coordination Interface), HVC conduit on QEMU 'virt' -----------------
/// PSCI 0.2 `CPU_ON`, SMC64 calling convention (function id per ARM DEN0022).
const PSCI_CPU_ON_64: u64 = 0xC400_0003;
const PSCI_SUCCESS: i64 = 0;

/// Ask firmware to power on `target` (MPIDR affinity value), entering at `entry` (physical
/// address, MMU off, IRQs masked) with `ctx` in `x0`. Returns the PSCI status (0 = success).
fn psci_cpu_on(target: u64, entry: u64, ctx: u64) -> i64 {
    let ret: i64;
    // SAFETY: HVC #0 with a valid PSCI function id is the platform firmware call on QEMU virt
    // (PSCI conduit = hvc when the kernel runs at EL1). Clobbers only x0..x3 per SMCCC.
    unsafe {
        asm!(
            "hvc #0",
            inout("x0") PSCI_CPU_ON_64 => ret,
            in("x1") target,
            in("x2") entry,
            in("x3") ctx,
            options(nomem, nostack),
        );
    }
    ret
}

// --- Per-CPU plumbing -----------------------------------------------------------------------
/// Max CPUs this suite drives (QEMU virt GICv2 tops out at 8; the gate boots 4).
const MAX_CPUS: usize = 8;
const STACK_BYTES: usize = 16 * 1024;

#[repr(C, align(16))]
struct SecondaryStack([u8; STACK_BYTES]);
/// One 16 KiB kernel stack per possible secondary (lives in BSS; core 0 zeroes BSS pre-CPU_ON).
static mut SECONDARY_STACKS: [SecondaryStack; MAX_CPUS - 1] =
    [const { SecondaryStack([0; STACK_BYTES]) }; MAX_CPUS - 1];

/// Page-table root every secondary installs (captured from core 0's live TTBR0 by `selftest`).
static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);
/// Bit i set ⇒ CPU i is online (MMU on, per-CPU state installed).
static ONLINE_MASK: AtomicUsize = AtomicUsize::new(0);
/// MPIDR affinity-0 and TPIDR_EL1 observed by each secondary (per-CPU identity proof).
static SEEN_AFF0: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
static SEEN_TPIDR: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
/// Secondaries that reached the final parking loop.
static PARKED: AtomicUsize = AtomicUsize::new(0);

/// Phase gate — core 0 advances it; secondaries wait on it.
/// 1=counter 2=mailbox 3=caps 4=ipi 5=sched 6=shootdown 7=park.
static PHASE: AtomicU32 = AtomicU32::new(0);
static DONE_COUNTER: AtomicUsize = AtomicUsize::new(0);
static DONE_MAILBOX: AtomicUsize = AtomicUsize::new(0);
static DONE_CAPS: AtomicUsize = AtomicUsize::new(0);
static DONE_IPI: AtomicUsize = AtomicUsize::new(0);

// Phase 1 — exact cross-core atomic accounting.
const COUNTER_ROUNDS: usize = 10_000;
static SHARED_COUNTER: AtomicUsize = AtomicUsize::new(0);

// Phase 2 — release/acquire mailbox (core 0 publishes, every secondary must observe exactly).
static MAILBOX_DATA: AtomicU64 = AtomicU64::new(0);
static MAILBOX_FLAG: AtomicBool = AtomicBool::new(false);
static MAILBOX_RESP: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
const MAILBOX_PAYLOAD: u64 = 0x5EED_2026_C0DE_CAFE;

// Phase 3 — capability concurrency on real cores (ADR-027 mechanism, now hardware-parallel).
static COMMITS: AtomicUsize = AtomicUsize::new(0);
/// Set by core 0 INSIDE the engine lock, immediately after `revoke` — the linearization marker.
static REVOKED: AtomicBool = AtomicBool::new(false);
static POST_DENIED: AtomicUsize = AtomicUsize::new(0);
static POST_ALLOWED: AtomicUsize = AtomicUsize::new(0);
const POST_ATTEMPTS: usize = 64;

// Phase 4 — GICv2 SGI inter-processor interrupt.
static IPI_SEEN: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static IPI_BAD: AtomicUsize = AtomicUsize::new(0);

// Phase 5 — per-CPU run queues + work stealing on REAL cores (ADR-021 Phase 2, REQ-SMP-003).
// Core 0 seeds every task on ONE secondary's queue, so every other core can progress only by
// stealing through `kernel_core::smpsched` — the policy host-proved in kernel-core/tests/smpsched.
const SCHED_TASKS: usize = 64;
/// The `&'static SmpSched` (leaked by core 0), published as an address for the secondaries.
static SCHED_PTR: AtomicUsize = AtomicUsize::new(0);
/// Per-task dispatch count — every slot must end at EXACTLY 1 (no loss, no duplication).
static EXEC_SEEN: [AtomicU32; SCHED_TASKS] = [const { AtomicU32::new(0) }; SCHED_TASKS];
static EXEC_TOTAL: AtomicUsize = AtomicUsize::new(0);
static STEALS: AtomicUsize = AtomicUsize::new(0);
static DONE_SCHED: AtomicUsize = AtomicUsize::new(0);

/// The scheduler core 0 published. Callers must be past the PHASE=5 gate (publication ordering).
fn sched_ref() -> &'static SmpSched {
    // SAFETY: SCHED_PTR is a Box::leak'd SmpSched published with Release before PHASE reaches 5;
    // readers gate on PHASE (SeqCst) first, so the pointer is non-null and the pointee immortal.
    unsafe { &*(SCHED_PTR.load(Ordering::Acquire) as *const SmpSched) }
}

/// One scheduling turn shared by every core: dispatch, tally, attribute steals.
fn sched_worker_turn(cpu: usize) {
    if let Some(d) = sched_ref().next_for(cpu) {
        let t = d.task as usize;
        if t < SCHED_TASKS {
            EXEC_SEEN[t].fetch_add(1, Ordering::SeqCst);
        }
        if d.stolen_from.is_some() {
            STEALS.fetch_add(1, Ordering::SeqCst);
        }
        EXEC_TOTAL.fetch_add(1, Ordering::SeqCst);
    } else {
        core::hint::spin_loop();
    }
}

// Phase 6 — cross-core TLB shootdown on REAL cores (REQ-SMP-004, ADR-021 Phase 3). Core 0 maps a
// fresh VA to frame A in the SHARED kernel root; every secondary reads it (priming its TLB with the
// A→translation); core 0 remaps the VA to frame B and shoots down — the aarch64 broadcast `tlbi
// …is` is the real cross-core mechanism, and the `kernel_core::shootdown` barrier proves every
// secondary acknowledged its own invalidation BEFORE core 0 treats the remap as globally effective.
// Each secondary then re-reads and must observe B. HONESTY (ADR-021 Phase 3): this proves the
// mechanism + barrier ran on real cores, NOT that QEMU TCG exhibits a stale entry (its softmmu TLB
// is a performance cache, not a faithful retention model) — the discriminating stale-vs-fresh proof
// is the deterministic host-thread test in kernel-core/tests/shootdown.rs.
/// A spare VA in an unused L1 slot (2 GiB; RAM identity map is L1 idx 1 = 1..1.125 GiB, peripherals
/// idx 0) — safe to map/remap in the shared root without disturbing the running kernel.
const SHOOTDOWN_VA: usize = 0x8000_0000;
/// The address space (ASID) the shootdown targets — a synthetic id for this VA-space test.
const SHOOTDOWN_ASID: u64 = 1;
/// Frame-A / frame-B tenant magics (distinct), written through the RAM identity map.
const MAGIC_A: u64 = 0xA1E7_0000_1111_0001;
const MAGIC_B: u64 = 0xB0B0_0000_2222_0002;
/// The `&'static TlbShootdown` (leaked by core 0), published as an address for the secondaries.
static SHOOTDOWN_PTR: AtomicUsize = AtomicUsize::new(0);
/// Value each secondary read through SHOOTDOWN_VA at priming (must be MAGIC_A) and after the
/// shootdown (must be MAGIC_B). Init u64::MAX = "not read yet".
static SEEN_MAP_A: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
static SEEN_MAP_B: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
static PRIMED: AtomicUsize = AtomicUsize::new(0);
static DONE_SHOOTDOWN: AtomicUsize = AtomicUsize::new(0);

/// The shootdown coordinator core 0 published. Callers must be past the PHASE=6 gate.
fn shootdown_ref() -> &'static TlbShootdown {
    // SAFETY: SHOOTDOWN_PTR is a Box::leak'd TlbShootdown published with Release before PHASE
    // reaches 6; readers gate on PHASE (SeqCst) first, so the pointer is non-null and immortal.
    unsafe { &*(SHOOTDOWN_PTR.load(Ordering::Acquire) as *const TlbShootdown) }
}

// --- Phase 7 — CPU affinity + cross-core migration (REQ-SMP-005, ADR-021 Phase 4) ---------------
// Task ids for the migration demo (kept clear of the 0..SCHED_TASKS work-item range).
const AFFINE_TASK: u64 = 1_001;
const MIG_TASK: u64 = 1_002;
/// The "saved register" value the migrated task's context carries; the thief must restore it exactly.
const MIG_MAGIC: u64 = 0xA110_C0DE_F00D_5EED;

/// Minimal saved-execution-context stand-in for the migration proof. A real backend restores the
/// task's full register file + address space in `resume` and `eret`s into it; this restores ONE saved
/// GPR through a register move — enough to prove the `kernel_core::sched::TaskContext` seam ran on the
/// thief CPU (the CPU currently executing `resume`), which is where the stolen task now lives.
struct GpCtx {
    saved: u64,
    restored: u64,
}
impl GpCtx {
    fn new(saved: u64) -> Self {
        GpCtx { saved, restored: 0 }
    }
}
impl TaskContext for GpCtx {
    fn resume(&mut self) {
        let out: u64;
        // SAFETY: a pure register-to-register move; touches no memory or stack. Models the "restore a
        // saved GPR" step of a context switch, executed on the thief CPU.
        unsafe {
            asm!("mov {o}, {i}", i = in(reg) self.saved, o = out(reg) out, options(nomem, nostack, pure));
        }
        self.restored = out;
    }
}

// --- Capability engine behind the SHARED kernel-core SpinLock (Issue 1: defined once) --------
// The engine lock is what makes ADR-027's `with_authorization` contract real across cores ("under
// the engine's lock no revoke can linearize between the authorization and the effect").
struct CapState {
    engine: CapEngine,
    cap: CapToken,
}
static ENGINE: SpinLock<Option<CapState>> = SpinLock::new(None);

// --- GICv2 SGI (software-generated interrupt) — the IPI path --------------------------------
const GICD_BASE: usize = 0x0800_0000;
const GICC_BASE: usize = 0x0801_0000;
const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER0: usize = 0x100; // SGI/PPI enables — BANKED per CPU
const GICD_IPRIORITYR: usize = 0x400; // SGI priority bytes — BANKED per CPU
const GICD_SGIR: usize = 0xF00;
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_IAR: usize = 0x00C;
const GICC_EOIR: usize = 0x010;
const SGI_ID: u32 = 0;
const SPURIOUS: u32 = 1023;

#[inline]
fn mmio_w32(addr: usize, v: u32) {
    // SAFETY: fixed Device-mapped QEMU-virt GIC register.
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) };
}
#[inline]
fn mmio_r32(addr: usize) -> u32 {
    // SAFETY: fixed Device-mapped QEMU-virt GIC register.
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// Per-CPU GIC setup for POLLED SGI reception: enable SGI 0 (banked), give it top priority
/// (banked byte), open the priority mask, enable this CPU's interface. PSTATE.I stays MASKED —
/// the pending SGI is claimed by reading IAR (classic polled-GIC operation), so this never
/// re-enters the core-0-owned vector table.
fn gic_cpu_init() {
    mmio_w32(GICD_BASE + GICD_ISENABLER0, 1 << SGI_ID);
    // SAFETY: banked per-CPU SGI priority byte for INTID 0.
    unsafe { core::ptr::write_volatile((GICD_BASE + GICD_IPRIORITYR) as *mut u8, 0x00) };
    mmio_w32(GICC_BASE + GICC_PMR, 0xF0);
    mmio_w32(GICC_BASE + GICC_CTLR, 1);
}

// --- Time (deadlines) ------------------------------------------------------------------------
fn now_ticks() -> u64 {
    ActiveHal::timer_ticks()
}
fn deadline_after(secs: u64) -> u64 {
    now_ticks() + secs * ActiveHal::timer_freq_hz()
}

/// Spin until `cond` or `deadline` (ticks). True ⇒ condition met (progress-gated, never a fixed
/// spin count).
fn wait_until(deadline: u64, mut cond: impl FnMut() -> bool) -> bool {
    while !cond() {
        if now_ticks() > deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

// --- Secondary-CPU code path ------------------------------------------------------------------
fn mpidr_aff0() -> u64 {
    let v: u64;
    // SAFETY: MPIDR_EL1 is always readable at EL1.
    unsafe { asm!("mrs {v}, mpidr_el1", v = out(reg) v, options(nomem, nostack)) };
    v & 0xff
}

/// Rust entry for a powered-on secondary (from `_secondary_start` in boot.s: MMU off, IRQs
/// masked, SP = its private stack). Never returns; parks in WFE when the suite completes.
#[no_mangle]
pub extern "C" fn ksecondary(_ctx: u64) -> ! {
    let cpu = mpidr_aff0() as usize;

    // Adopt the shared kernel address space. The root identity-maps kernel text/data/stacks and
    // the device GiB (vm::build_identity contract), so this core sees the same world as core 0.
    // SAFETY: root captured from core 0's live TTBR0; identity-map precondition holds.
    unsafe { vm::enable(KERNEL_ROOT.load(Ordering::Acquire)) };

    // Per-CPU identity: TPIDR_EL1 is the architectural per-CPU pointer slot.
    // SAFETY: writing TPIDR_EL1 at EL1 is always sound.
    unsafe { asm!("msr tpidr_el1, {v}", v = in(reg) cpu as u64, options(nomem, nostack)) };
    let tpidr: u64;
    // SAFETY: reading TPIDR_EL1 at EL1 is always sound.
    unsafe { asm!("mrs {v}, tpidr_el1", v = out(reg) tpidr, options(nomem, nostack)) };
    SEEN_AFF0[cpu].store(mpidr_aff0(), Ordering::SeqCst);
    SEEN_TPIDR[cpu].store(tpidr, Ordering::SeqCst);
    ONLINE_MASK.fetch_or(1 << cpu, Ordering::SeqCst);

    // Phase 1 — hammer the shared counter; exactness proves real cross-core atomicity.
    while PHASE.load(Ordering::SeqCst) < 1 {
        core::hint::spin_loop();
    }
    for _ in 0..COUNTER_ROUNDS {
        SHARED_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    DONE_COUNTER.fetch_add(1, Ordering::SeqCst);

    // Phase 2 — release/acquire mailbox: observe the payload published by core 0, answer with a
    // per-CPU transform so core 0 can prove THIS core read THIS value.
    while PHASE.load(Ordering::SeqCst) < 2 {
        core::hint::spin_loop();
    }
    while !MAILBOX_FLAG.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let data = MAILBOX_DATA.load(Ordering::Relaxed);
    MAILBOX_RESP[cpu].store(data ^ (0xA1E7 + cpu as u64), Ordering::SeqCst);
    DONE_MAILBOX.fetch_add(1, Ordering::SeqCst);

    // Phase 3 — capability concurrency on real cores. Commit through the ADR-027 primitive until
    // core 0 revokes; then make POST_ATTEMPTS more tries, every one of which must fail closed.
    while PHASE.load(Ordering::SeqCst) < 3 {
        core::hint::spin_loop();
    }
    while !REVOKED.load(Ordering::Acquire) {
        let guard = ENGINE.lock();
        if let Some(state) = guard.as_ref() {
            let _ = state.engine.with_authorization(
                "data.write",
                &Target::default(),
                &[state.cap],
                |_, _| {
                    COMMITS.fetch_add(1, Ordering::Relaxed);
                },
            );
        }
        drop(guard);
        core::hint::spin_loop();
    }
    for _ in 0..POST_ATTEMPTS {
        let guard = ENGINE.lock();
        if let Some(state) = guard.as_ref() {
            match state.engine.with_authorization(
                "data.write",
                &Target::default(),
                &[state.cap],
                |_, _| {
                    COMMITS.fetch_add(1, Ordering::Relaxed);
                },
            ) {
                Ok(_) => POST_ALLOWED.fetch_add(1, Ordering::SeqCst),
                Err(_) => POST_DENIED.fetch_add(1, Ordering::SeqCst),
            };
        }
    }
    DONE_CAPS.fetch_add(1, Ordering::SeqCst);

    // Phase 4 — receive core 0's SGI by polling this CPU's own banked GIC interface.
    while PHASE.load(Ordering::SeqCst) < 4 {
        core::hint::spin_loop();
    }
    gic_cpu_init();
    let deadline = deadline_after(5);
    loop {
        let iar = mmio_r32(GICC_BASE + GICC_IAR);
        let intid = iar & 0x3FF;
        if intid == SGI_ID {
            let src = (iar >> 10) & 0x7;
            if src != 0 {
                IPI_BAD.fetch_add(1, Ordering::SeqCst);
            }
            mmio_w32(GICC_BASE + GICC_EOIR, iar);
            IPI_SEEN[cpu].store(true, Ordering::SeqCst);
            break;
        }
        if intid != SPURIOUS {
            // Unexpected source — ack it away and count the anomaly.
            mmio_w32(GICC_BASE + GICC_EOIR, iar);
            IPI_BAD.fetch_add(1, Ordering::SeqCst);
        }
        if now_ticks() > deadline {
            break; // core 0's invariant 9 will report the miss
        }
        core::hint::spin_loop();
    }
    mmio_w32(GICC_BASE + GICC_CTLR, 0); // quiesce this CPU's interface before parking

    DONE_IPI.fetch_add(1, Ordering::SeqCst);

    // Phase 5 — per-CPU scheduling: pull work from this core's own run queue, steal when it is
    // empty, until the whole task set is dispatched (the executions themselves are the tallies).
    while PHASE.load(Ordering::SeqCst) < 5 {
        core::hint::spin_loop();
    }
    while EXEC_TOTAL.load(Ordering::SeqCst) < SCHED_TASKS {
        sched_worker_turn(cpu);
    }
    DONE_SCHED.fetch_add(1, Ordering::SeqCst);

    // Phase 6 — TLB shootdown. Prime this core's TLB by reading the mapped VA, service the
    // initiator's shootdown (drop our cached translation, then acknowledge), and re-read to
    // observe the remapped frame.
    while PHASE.load(Ordering::SeqCst) < 6 {
        core::hint::spin_loop();
    }
    // Prime: populate this core's TLB with the A→ translation and record what we saw.
    // SAFETY: SHOOTDOWN_VA is mapped to frame A in the shared root before PHASE reaches 6.
    let primed_val = unsafe { core::ptr::read_volatile(SHOOTDOWN_VA as *const u64) };
    SEEN_MAP_A[cpu].store(primed_val, Ordering::SeqCst);
    PRIMED.fetch_add(1, Ordering::SeqCst);
    // Service shootdowns until the initiator has invalidated our mapping (>=1 acknowledged). The
    // service callback runs the core-LOCAL invalidation, so the ack means real work happened.
    let sd = shootdown_ref();
    let sd_deadline = deadline_after(10);
    while sd.acked(cpu) == 0 {
        sd.service(cpu, |_inv| {
            // SAFETY: local TLB invalidation of the shootdown VA at EL1 is always sound.
            unsafe { vm::tlbi_va_local(SHOOTDOWN_VA) };
        });
        if now_ticks() > sd_deadline {
            break;
        }
        core::hint::spin_loop();
    }
    // Re-read: post-shootdown, this core must now observe the remapped frame (B).
    // SAFETY: the VA is remapped (not unmapped) by the initiator before it posts the shootdown.
    let reread_val = unsafe { core::ptr::read_volatile(SHOOTDOWN_VA as *const u64) };
    SEEN_MAP_B[cpu].store(reread_val, Ordering::SeqCst);
    DONE_SHOOTDOWN.fetch_add(1, Ordering::SeqCst);

    PARKED.fetch_add(1, Ordering::SeqCst);
    loop {
        // SAFETY: WFE in a loop is the architectural idle park.
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}

// --- Core-0 selftest ---------------------------------------------------------------------------
/// Prove the SMP invariants live. `Ok(n)` = all passed (`Ok(0)` = no secondary CPUs, skip green —
/// the VM gate boots `-smp 4` so a silent skip cannot pass CI); `Err((idx, name))` = failure.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            if !($cond) {
                kprintln!("  [FAIL {:>2}] {}", n, $name);
                return Err((n, $name));
            }
            kprintln!("  [pass {:>2}] {}", n, $name);
        }};
    }

    KERNEL_ROOT.store(vm::active_root(), Ordering::Release);

    extern "C" {
        fn _secondary_start();
    }
    let entry = _secondary_start as *const () as usize as u64;

    // 1 — power on every present secondary via PSCI. First failure past CPU 1 = end of topology.
    let mut secondaries: usize = 0;
    for cpu in 1..MAX_CPUS {
        let stack_top = {
            // SAFETY: address-of a static array element; no data is read or written here.
            let base = unsafe { SECONDARY_STACKS[cpu - 1].0.as_ptr() as usize };
            (base + STACK_BYTES) as u64
        };
        if psci_cpu_on(cpu as u64, entry, stack_top) == PSCI_SUCCESS {
            secondaries += 1;
        } else {
            break;
        }
    }
    if secondaries == 0 {
        kprintln!(
            "  [info  ] single-CPU machine (PSCI reports no other cores) — SMP suite skipped"
        );
        return Ok(0);
    }
    let all_mask: usize = ((1 << secondaries) - 1) << 1;
    check!(
        secondaries >= 1,
        "smp: PSCI CPU_ON accepted for secondary CPUs"
    );

    // 2 — every powered-on core comes online (stack up, MMU on over the shared tables).
    check!(
        wait_until(deadline_after(5), || ONLINE_MASK.load(Ordering::SeqCst)
            == all_mask),
        "smp: all secondaries online with the MMU on (shared kernel tables)"
    );
    kprintln!(
        "  [info  ] {} secondary CPU(s) online, mask {:#x}",
        secondaries,
        ONLINE_MASK.load(Ordering::SeqCst)
    );

    // 3 — per-CPU identity: each core saw its own MPIDR and its own TPIDR_EL1 value.
    let mut identity_ok = true;
    for cpu in 1..=secondaries {
        identity_ok &= SEEN_AFF0[cpu].load(Ordering::SeqCst) == cpu as u64;
        identity_ok &= SEEN_TPIDR[cpu].load(Ordering::SeqCst) == cpu as u64;
    }
    check!(
        identity_ok,
        "smp: per-CPU identity distinct (MPIDR + TPIDR_EL1 per core)"
    );

    // 4 — exact atomic accounting across all cores (core 0 participates too).
    PHASE.store(1, Ordering::SeqCst);
    for _ in 0..COUNTER_ROUNDS {
        SHARED_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    check!(
        wait_until(deadline_after(10), || DONE_COUNTER.load(Ordering::SeqCst)
            == secondaries),
        "smp: counter phase completes on every core"
    );
    check!(
        SHARED_COUNTER.load(Ordering::SeqCst) == (secondaries + 1) * COUNTER_ROUNDS,
        "smp: cross-core atomic counter is exact (no lost increments)"
    );

    // 5 — release/acquire publication: every core observed the exact payload.
    PHASE.store(2, Ordering::SeqCst);
    MAILBOX_DATA.store(MAILBOX_PAYLOAD, Ordering::Relaxed);
    MAILBOX_FLAG.store(true, Ordering::Release);
    check!(
        wait_until(deadline_after(10), || DONE_MAILBOX.load(Ordering::SeqCst)
            == secondaries),
        "smp: mailbox phase completes on every core"
    );
    let mut mailbox_ok = true;
    for (cpu, resp) in MAILBOX_RESP
        .iter()
        .enumerate()
        .take(secondaries + 1)
        .skip(1)
    {
        mailbox_ok &= resp.load(Ordering::SeqCst) == MAILBOX_PAYLOAD ^ (0xA1E7 + cpu as u64);
    }
    check!(
        mailbox_ok,
        "smp: release/acquire mailbox observed exactly by every core"
    );

    // 6..8 — ADR-027 on real silicon-parallel cores: commits flow before revoke; the revoke
    // linearizes inside the engine lock; nothing commits after it, and every retry fails closed.
    {
        let mut guard = ENGINE.lock();
        let mut engine = CapEngine::new(0x5EED_2026, 1_000);
        let cap = engine.mint("smp-worker", "data.write", Scope::All, Constraints::none());
        *guard = Some(CapState { engine, cap });
    }
    PHASE.store(3, Ordering::SeqCst);
    check!(
        wait_until(deadline_after(10), || {
            COMMITS.load(Ordering::SeqCst) >= secondaries * 4
        }),
        "smp: cross-core commits flow through with_authorization (progress gate)"
    );
    let commits_at_revoke;
    {
        let mut guard = ENGINE.lock();
        let state = guard.as_mut().expect("engine installed above");
        let cap = state.cap;
        state.engine.revoke(cap);
        REVOKED.store(true, Ordering::Release); // linearization marker, inside the lock hold
        commits_at_revoke = COMMITS.load(Ordering::SeqCst);
    }
    check!(
        wait_until(deadline_after(10), || DONE_CAPS.load(Ordering::SeqCst)
            == secondaries),
        "smp: capability phase completes on every core"
    );
    check!(
        COMMITS.load(Ordering::SeqCst) == commits_at_revoke,
        "smp: ZERO commits after revoke linearizes (atomic authorize+execute, ADR-027)"
    );
    check!(
        POST_ALLOWED.load(Ordering::SeqCst) == 0
            && POST_DENIED.load(Ordering::SeqCst) == secondaries * POST_ATTEMPTS,
        "smp: every post-revoke attempt on every core fails closed"
    );

    // 9 — inter-processor interrupt: SGI 0 from core 0, claimed on each core's banked interface.
    PHASE.store(4, Ordering::SeqCst);
    mmio_w32(GICD_BASE + GICD_CTLR, 1); // usermode's teardown disabled the distributor
                                        // Each secondary programs its banked SGI enable on its own schedule, and an SGI sent before that
                                        // enable is latched pending in the distributor — but re-sending until the target acknowledges is
                                        // progress-gated and covers both orders.
    for (cpu, seen) in IPI_SEEN.iter().enumerate().take(secondaries + 1).skip(1) {
        let deadline = deadline_after(5);
        loop {
            mmio_w32(GICD_BASE + GICD_SGIR, (1 << (16 + cpu)) | SGI_ID);
            if seen.load(Ordering::SeqCst) || now_ticks() > deadline {
                break;
            }
            core::hint::spin_loop();
        }
    }
    let ipi_all = wait_until(deadline_after(5), || {
        (1..=secondaries).all(|cpu| IPI_SEEN[cpu].load(Ordering::SeqCst))
    });
    check!(
        ipi_all && IPI_BAD.load(Ordering::SeqCst) == 0,
        "smp: SGI IPI from core 0 delivered + acknowledged on every secondary"
    );
    mmio_w32(GICD_BASE + GICD_CTLR, 0); // restore the pre-suite (post-teardown) GIC state

    // 10..12 — ADR-021 Phase 2 on real cores: per-CPU run queues + work stealing. Every task is
    // seeded on CPU 1's queue alone, so core 0 and CPUs 2.. can only progress by STEALING —
    // the policy (`kernel_core::smpsched`, host-proved) now demonstrably schedules real silicon.
    let ncpus = secondaries + 1;
    let sched: &'static SmpSched = Box::leak(Box::new(SmpSched::new(ncpus)));
    for t in 0..SCHED_TASKS {
        sched.enqueue_on(1, t as u64);
    }
    SCHED_PTR.store(sched as *const SmpSched as usize, Ordering::Release);
    // Deterministic first steal: no other core is scheduling yet and core 0's own queue is empty,
    // so this dispatch MUST come from CPU 1's queue — invariant 12 can never race to a flake.
    sched_worker_turn(0);
    PHASE.store(5, Ordering::SeqCst);
    let sched_deadline = deadline_after(10);
    while EXEC_TOTAL.load(Ordering::SeqCst) < SCHED_TASKS && now_ticks() <= sched_deadline {
        sched_worker_turn(0); // core 0 works the queues too (it steals — its queue stays empty)
    }
    check!(
        wait_until(deadline_after(10), || DONE_SCHED.load(Ordering::SeqCst)
            == secondaries),
        "smp: scheduling phase completes on every core"
    );
    let mut exactly_once = EXEC_TOTAL.load(Ordering::SeqCst) == SCHED_TASKS;
    for seen in EXEC_SEEN.iter() {
        exactly_once &= seen.load(Ordering::SeqCst) == 1;
    }
    check!(
        exactly_once,
        "smp: per-CPU queues dispatch every task EXACTLY once (none lost, none duplicated)"
    );
    check!(
        STEALS.load(Ordering::SeqCst) > 0 && (0..ncpus).all(|c| sched.load(c) == 0),
        "smp: work stealing drains an unbalanced queue across cores (smpsched live on real CPUs)"
    );

    // 16..18 — cross-core TLB shootdown (REQ-SMP-004, ADR-021 Phase 3). Map SHOOTDOWN_VA -> frame A
    // in the SHARED kernel root; each secondary primes its TLB by reading it; remap -> frame B and
    // shoot down (aarch64 broadcast tlbi + the kernel-core all-acknowledged barrier); each secondary
    // re-reads and must observe B. Honesty scope: proves the mechanism + barrier on real cores, not
    // that QEMU exhibits a stale entry (that discriminating proof is the host-thread test).
    let shootdown: &'static TlbShootdown = Box::leak(Box::new(TlbShootdown::new(ncpus)));
    SHOOTDOWN_PTR.store(shootdown as *const TlbShootdown as usize, Ordering::Release);
    let frame_a = frames::alloc().expect("shootdown frame A");
    let frame_b = frames::alloc().expect("shootdown frame B");
    // SAFETY: both are fresh RAM frames (identity-mapped); write the tenant magics directly.
    unsafe {
        core::ptr::write_volatile(frame_a.addr() as *mut u64, MAGIC_A);
        core::ptr::write_volatile(frame_b.addr() as *mut u64, MAGIC_B);
    }
    let root = vm::active_root();
    let mapped_a = vm::map_page(root, SHOOTDOWN_VA, frame_a.addr(), vm::NORMAL_PAGE);
    // SAFETY: publish the fresh A-mapping to every core in the inner-shareable domain before priming.
    unsafe { vm::tlbi_va_broadcast(SHOOTDOWN_VA) };
    PHASE.store(6, Ordering::SeqCst);
    check!(
        mapped_a
            && wait_until(deadline_after(10), || PRIMED.load(Ordering::SeqCst)
                == secondaries)
            && (1..=secondaries).all(|c| SEEN_MAP_A[c].load(Ordering::SeqCst) == MAGIC_A),
        "smp: every secondary primed a shared-address-space mapping (observed frame A)"
    );

    // Remap the SAME VA to frame B, then shoot it down across every secondary. The broadcast tlbi
    // is aarch64's real cross-core invalidation; the barrier proves each core acknowledged BEFORE
    // core 0 treats the remap as globally effective (the load-bearing ordering, ADR-027-style).
    let mapped_b = vm::map_page(root, SHOOTDOWN_VA, frame_b.addr(), vm::NORMAL_PAGE);
    // SAFETY: broadcast the B-remap invalidation to the inner-shareable domain.
    unsafe { vm::tlbi_va_broadcast(SHOOTDOWN_VA) };
    let mut tgt = [0usize; MAX_CPUS];
    for (i, slot) in tgt.iter_mut().enumerate().take(secondaries) {
        *slot = i + 1;
    }
    let barrier_deadline = deadline_after(10);
    let barrier_ok = shootdown.request(
        &tgt[..secondaries],
        Invalidation::page(SHOOTDOWN_ASID, SHOOTDOWN_VA as u64),
        || now_ticks() <= barrier_deadline,
    );
    check!(
        mapped_b
            && barrier_ok
            && wait_until(deadline_after(10), || DONE_SHOOTDOWN.load(Ordering::SeqCst)
                == secondaries),
        "smp: TLB shootdown barrier acknowledged by every secondary (all-acknowledged completion)"
    );
    check!(
        (1..=secondaries).all(|c| SEEN_MAP_B[c].load(Ordering::SeqCst) == MAGIC_B),
        "smp: every secondary observes the remapped frame B after the shootdown (coherent)"
    );

    // 19..21 — CPU affinity + cross-core migration (REQ-SMP-005, ADR-021 Phase 4). Driven entirely by
    // core 0 over a PRIVATE SmpSched (the invariant-12 "deterministic first steal" doctrine: no other
    // core touches `mig`, so nothing here can race). This is the policy on real cores; the arch resume
    // is the kernel_core::sched::TaskContext seam. Preemptive *timing* is the PIT's job (already gated
    // by the EL0 preemption suite) — this proves the migration MECHANISM + the resume seam.
    let mig = SmpSched::new(ncpus);
    // Affinity: a task pinned to CPU 1 but seeded on CPU 0's queue is refused by every non-permitted
    // CPU and taken only by CPU 1 (an affinity-honored cross-core steal).
    mig.enqueue_on_affine(0, AFFINE_TASK, AffinityMask::only(1));
    let affinity_refused = mig.next_for(0).is_none()
        && (ncpus <= 2 || mig.next_for(2).is_none())
        && (ncpus <= 3 || mig.next_for(3).is_none());
    let affinity_taken = mig.next_for(1).map(|d| d.task) == Some(AFFINE_TASK);
    check!(
        affinity_refused && affinity_taken,
        "smp: CPU affinity honored — a pinned task runs only on its permitted CPU (REQ-SMP-005)"
    );
    // Migration: a task seeded on CPU 1's queue is stolen by CPU 0 (its own queue empty) — the task
    // now lives on a CPU other than the one it was enqueued on.
    mig.enqueue_on(1, MIG_TASK);
    let migrated = mig.next_for(0)
        == Some(Dispatch {
            task: MIG_TASK,
            stolen_from: Some(1),
        });
    check!(
        migrated,
        "smp: a task seeded on CPU 1 migrates to CPU 0 by stealing (cross-core task migration)"
    );
    // Resume the migrated task on the thief through the TaskContext seam (a minimal GPR restore).
    let mut ctx = GpCtx::new(MIG_MAGIC);
    ctx.resume();
    check!(
        ctx.restored == MIG_MAGIC,
        "smp: the migrated task resumes on the thief via the TaskContext seam (GPR restore ran here)"
    );

    // 22 — the machine is still coherent: every secondary parked, nothing regressed.
    PHASE.store(7, Ordering::SeqCst);
    check!(
        wait_until(deadline_after(5), || PARKED.load(Ordering::SeqCst)
            == secondaries)
            && ONLINE_MASK.load(Ordering::SeqCst) == all_mask
            && SHARED_COUNTER.load(Ordering::SeqCst) == (secondaries + 1) * COUNTER_ROUNDS,
        "smp: all secondaries parked; online mask + counters stable"
    );

    let _ = DONE_IPI.load(Ordering::SeqCst); // phase-4 completion is subsumed by PARKED
    Ok(n)
}
