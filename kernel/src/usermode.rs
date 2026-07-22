//! EL0 user-mode, the capability-gated syscall boundary, and cooperative multitasking
//! (PRD P5, mm bricks 3–5).
//!
//! WHY THIS MATTERS: until this layer every invariant was re-proved *in kernel space* (EL1) —
//! the benchmark's own honesty note says the measured loop "crosses no privilege/address-space
//! boundary," so isolation was logical, not hardware-enforced. This module makes the boundary
//! REAL: it drops the CPU to **EL0** (unprivileged), runs genuinely less-privileged instruction
//! streams in EL0-only pages, and lets them reach the OS through one door — an `svc` trap that
//! lands in the EL1 vector and is authorized by the **same `CapEngine`** the deterministic
//! pipeline uses. It then gives each process its **own TTBR0 address space** (isolation across
//! processes, not just EL0-vs-EL1) and, finally, **context-switches** between EL0 tasks under a
//! round-robin scheduler — the first executed Aletheia multitasking.
//!
//! ONE TRAP PATH (save-first). Every EL0 entry to `0x400` saves the FULL register file
//! (x0–x30 + ELR + SPSR + SP_EL0) into the running task's `TrapFrame` BEFORE touching anything,
//! then decodes `ESR_EL1.EC` and dispatches. `resume_frame` restores a whole frame and `eret`s,
//! so a task resumes *after* its yield — the same primitive both starts a fresh task and resumes
//! a preempted one. `TPIDR_EL1` holds the current frame pointer; `TPIDR_EL0` is the save-time
//! scratch. This unification means the capability/isolation invariants and the scheduler all run
//! through one audited trap path.
//!
//! SCHEDULING — both cooperative (tasks `SYS_YIELD`) and PREEMPTIVE: a GICv2 + EL1 generic-timer
//! IRQ (vector `0x480`) preempts non-yielding EL0 tasks and the round-robin scheduler switches
//! them, each resuming with full register state. Contract-honest (ADR-010): every line executes
//! under QEMU and is asserted by `usermode::selftest()`; an *unexpected* fault stays fatal
//! (`exit 102`), the preemption test's task loop is *bounded* so a dead timer fails cleanly rather
//! than hanging, and `-machine virt,gic-version=2` is pinned. aarch64 dev backend only; the
//! `#[path]`-shared `spine.rs` is untouched. Requires the MMU (`vm::enable`).
use crate::spine::{CapEngine, CapToken, Constraints, Decision, Scope, Store, Target};
use crate::{frames, vm};
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::ptr::{addr_of, addr_of_mut};
// REQ-KERN-005: the aarch64 target DRIVES the shared arch-independent scheduling policy from
// kernel-core rather than hand-rolling its own rotation. kernel-core decides which task runs next;
// this module performs only the context-switch MECHANISM (resume_frame + address-space switch).
use kernel_core::sched::{RoundRobin, TaskId};
// REQ-IPC-008: the shared grant-table is the arch-independent authority/lifecycle layer over a
// shared-memory region; THIS target's `vm.rs` performs the real page mapping into each address space.
use kernel_core::grant::{GrantTable, ShareMode};

global_asm!(
    r#"
.section .text

// resume_frame(frame: *mut TrapFrame /*x0*/)
// Save the caller's (scheduler/trial) callee-saved context so the 0x400 handler can resume it,
// set TPIDR_EL1 = the frame so a trap saves back into it, restore the whole frame, and ERET to
// EL0. Starts a fresh task and resumes a yielded one identically.
.global resume_frame
resume_frame:
    adrp    x9, KERNEL_CTX
    add     x9, x9, :lo12:KERNEL_CTX
    mov     x10, sp
    stp     x19, x20, [x9, #0]
    stp     x21, x22, [x9, #16]
    stp     x23, x24, [x9, #32]
    stp     x25, x26, [x9, #48]
    stp     x27, x28, [x9, #64]
    stp     x29, x30, [x9, #80]
    str     x10, [x9, #96]
    msr     tpidr_el1, x0           // current-frame pointer for the trap handler
    ldr     x1, [x0, #248]          // SP_EL0
    msr     sp_el0, x1
    ldr     x1, [x0, #256]          // ELR_EL1 (resume PC)
    msr     elr_el1, x1
    ldr     x1, [x0, #264]          // SPSR_EL1 (PSTATE)
    msr     spsr_el1, x1
    ldp     x1,  x2,  [x0, #8]
    ldp     x3,  x4,  [x0, #24]
    ldp     x5,  x6,  [x0, #40]
    ldp     x7,  x8,  [x0, #56]
    ldp     x9,  x10, [x0, #72]
    ldp     x11, x12, [x0, #88]
    ldp     x13, x14, [x0, #104]
    ldp     x15, x16, [x0, #120]
    ldp     x17, x18, [x0, #136]
    ldp     x19, x20, [x0, #152]
    ldp     x21, x22, [x0, #168]
    ldp     x23, x24, [x0, #184]
    ldp     x25, x26, [x0, #200]
    ldp     x27, x28, [x0, #216]
    ldp     x29, x30, [x0, #232]
    ldr     x0, [x0, #0]            // x0 last (was the frame base)
    isb
    eret

// resume_return: restore the caller's context saved by resume_frame and RET to it. The 0x400
// handler branches here after dispatch — a non-local resume back into the scheduler/trial.
.global resume_return
resume_return:
    adrp    x9, KERNEL_CTX
    add     x9, x9, :lo12:KERNEL_CTX
    ldp     x19, x20, [x9, #0]
    ldp     x21, x22, [x9, #16]
    ldp     x23, x24, [x9, #32]
    ldp     x25, x26, [x9, #48]
    ldp     x27, x28, [x9, #64]
    ldp     x29, x30, [x9, #80]
    ldr     x10, [x9, #96]
    mov     sp, x10
    ret

// SAVE-FIRST: stash the full register file into the current frame (TPIDR_EL1) before any clobber,
// using TPIDR_EL0 to bootstrap x0. Shared by the synchronous (svc/fault) and IRQ entries.
.macro SAVE_EL0_FRAME
    msr     tpidr_el0, x0           // scratch-stash x0 (frees it to hold the frame base)
    mrs     x0, tpidr_el1           // x0 = current frame base
    stp     x1,  x2,  [x0, #8]
    stp     x3,  x4,  [x0, #24]
    stp     x5,  x6,  [x0, #40]
    stp     x7,  x8,  [x0, #56]
    stp     x9,  x10, [x0, #72]
    stp     x11, x12, [x0, #88]
    stp     x13, x14, [x0, #104]
    stp     x15, x16, [x0, #120]
    stp     x17, x18, [x0, #136]
    stp     x19, x20, [x0, #152]
    stp     x21, x22, [x0, #168]
    stp     x23, x24, [x0, #184]
    stp     x25, x26, [x0, #200]
    stp     x27, x28, [x0, #216]
    stp     x29, x30, [x0, #232]
    mrs     x1, tpidr_el0           // recover original x0
    str     x1, [x0, #0]
    mrs     x1, sp_el0
    str     x1, [x0, #248]
    mrs     x1, elr_el1
    str     x1, [x0, #256]
    mrs     x1, spsr_el1
    str     x1, [x0, #264]
.endm

// Vector 0x400 (Lower EL, AArch64, Synchronous). Save, then decode ESR_EL1.EC: SVC(0x15) ->
// el0_trap(num=x8, arg=x0); Data Abort lower-EL(0x24) -> el0_data_abort; else fatal (exit 102).
.global el0_sync_entry
el0_sync_entry:
    SAVE_EL0_FRAME
    mrs     x1, esr_el1
    lsr     x1, x1, #26
    and     x1, x1, #0x3f
    cmp     x1, #0x15
    b.eq    30f
    cmp     x1, #0x24
    b.eq    40f
    b       default_exception
30: mov     x2, x0                  // x2 = frame base
    ldr     x0, [x2, #64]           // num = saved x8
    ldr     x1, [x2, #0]            // arg = saved x0
    bl      el0_trap
    b       resume_return
40: mrs     x0, far_el1
    bl      el0_data_abort
    b       resume_return

// Vector 0x480 (Lower EL, AArch64, IRQ). Save the preempted task's full frame, then hand off to
// the Rust IRQ handler (ack GIC, re-arm timer, EOI, mark preempted) and resume the scheduler.
.global el0_irq_entry
el0_irq_entry:
    SAVE_EL0_FRAME
    bl      el0_irq
    b       resume_return
"#
);

extern "C" {
    /// Restore a full `TrapFrame` and `eret` to EL0; returns (via `resume_return`) when the task
    /// traps back and the handler resumes the caller.
    fn resume_frame(frame: *mut TrapFrame);
    /// The EL1 vector table (from `vectors.s`); its address goes into `VBAR_EL1`.
    static exc_vectors: u8;
}

/// A full EL0 register context. `#[repr(C)]` fixes the byte offsets the trap asm hard-codes:
/// `regs[N]` at `N*8`, `sp` at 248, `elr` at 256, `spsr` at 264.
#[repr(C)]
#[derive(Clone, Copy)]
struct TrapFrame {
    regs: [u64; 31], // x0..x30
    sp: u64,         // SP_EL0
    elr: u64,        // ELR_EL1 (resume PC)
    spsr: u64,       // SPSR_EL1 (PSTATE)
}

/// SPSR for a fresh EL0 task: M = EL0t (AArch64), DAIF masked.
const SPSR_EL0T: u64 = 0x3C0;
/// SPSR for a *preemptible* EL0 task: EL0t with the IRQ mask (I, bit 7) CLEAR, so the timer
/// interrupt is delivered while the task runs. D/A/F stay masked.
const SPSR_EL0T_IRQ: u64 = 0x340;

impl TrapFrame {
    const fn zeroed() -> Self {
        TrapFrame {
            regs: [0; 31],
            sp: 0,
            elr: 0,
            spsr: 0,
        }
    }
    /// A fresh-task frame: entry PC, EL0 stack top, primed `x0`/`x8`, EL0t PSTATE.
    fn new_entry(entry: usize, sp: usize, x0: u64, x8: u64) -> Self {
        let mut f = Self::zeroed();
        f.regs[0] = x0;
        f.regs[8] = x8;
        f.sp = sp as u64;
        f.elr = entry as u64;
        f.spsr = SPSR_EL0T;
        f
    }
}

/// Callee-saved kernel context stash used by `resume_frame`/`resume_return`. 13 × u64: x19..x30
/// (pairs at byte offsets 0..80) then SP at offset 96. One resume is ever in flight, so one slot.
#[no_mangle]
static mut KERNEL_CTX: [u64; 13] = [0; 13];

/// Syscall numbers the EL0 boundary understands. Anything else is denied (fail closed).
const SYS_EMIT: u64 = 1;
const SYS_YIELD: u64 = 2;
const SYS_EXIT: u64 = 3;
/// Capability-secure kernel IPC: send a message body to the kernel endpoint / receive one from it.
/// Both are authorized by the SAME `CapEngine` (`ipc.send` / `ipc.recv` capabilities) — the message
/// crosses process boundaries only through the kernel, never shared memory.
const SYS_SEND: u64 = 4;
const SYS_RECV: u64 = 5;

/// Virtual addresses for one process's pages — past the identity-mapped RAM (so they are unmapped
/// until we map them, proving they are real EL0-only mappings, not identity RAM).
const USER_CODE_VA: usize = 0x5000_0000;
const USER_STACK_VA: usize = 0x5000_1000;
const USER_STACK_TOP: usize = USER_STACK_VA + frames::FRAME_SIZE;

/// A per-process private data page VA (same 2 MiB block, distinct L3 slot) for the cross-process
/// isolation test: mapped in process A's space, deliberately absent from process B's.
const VA_P: usize = 0x5000_3000;

// ---------------------------------------------------------------------------
// One-shot trial state (capability + isolation invariants) — reached by the handlers.
// ---------------------------------------------------------------------------

/// Single-threaded kernel (secondary harts parked); one excursion at a time.
struct Trial {
    engine: CapEngine,
    store: Store,
    caps: Vec<CapToken>,
    action: &'static str,
    /// When set, a Data Abort from EL0 is the *expected* isolation test, not a fatal bug.
    armed: bool,
    // outcomes, read back after the excursion returns
    allowed: bool,
    isolation_held: bool,
    fault_va: usize,
}

static mut CURRENT: Option<Trial> = None;

/// SAFETY: single-threaded; `CURRENT` is set immediately before an excursion and mutated only by
/// the handler that excursion drives. No concurrent access exists.
#[inline]
fn current() -> Option<&'static mut Trial> {
    unsafe { (*addr_of_mut!(CURRENT)).as_mut() }
}

// ---------------------------------------------------------------------------
// Capability-secure kernel IPC endpoint (a single-slot mailbox). A `SYS_SEND` deposits a body here;
// a `SYS_RECV` drains it. Because sender and receiver run in SEPARATE TTBR0 address spaces, the
// only path the body can travel is THROUGH this kernel object — never shared user memory.
// ---------------------------------------------------------------------------

/// The kernel endpoint's single message slot (`None` = empty).
static mut ENDPOINT: Option<u64> = None;
/// The body the most recent authorized `SYS_RECV` drained from the endpoint.
static mut IPC_RECEIVED: u64 = 0;

// ---------------------------------------------------------------------------
// Scheduler state (multitasking invariants).
// ---------------------------------------------------------------------------

/// What the last-resumed task did, read by the scheduler after `resume_frame` returns.
struct SchedState {
    last_magic: u64,
    exited: bool,
    /// Set by the IRQ handler: the task was involuntarily preempted by the timer (not a yield/exit).
    preempted: bool,
}
static mut SCHED: SchedState = SchedState {
    last_magic: 0,
    exited: false,
    preempted: false,
};

/// A task control block: its resumable register frame and run state. (The register-magic each
/// task must keep presenting lives in the `magics` table + the stub code, not here.)
#[derive(Clone, Copy)]
struct Tcb {
    frame: TrapFrame,
    done: bool,
}
impl Tcb {
    const fn new() -> Self {
        Tcb {
            frame: TrapFrame::zeroed(),
            done: false,
        }
    }
}
const NTASK: usize = 2;
static mut TCBS: [Tcb; NTASK] = [Tcb::new(); NTASK];

/// Trap dispatch — the capability-gated syscall AND the scheduler hooks, over one path.
/// `SYS_EMIT` runs the one-shot capability check (records an event on Allow); `SYS_YIELD`/
/// `SYS_EXIT` report to the scheduler (carrying the task's register-magic as `arg`).
#[no_mangle]
pub extern "C" fn el0_trap(num: u64, arg: u64) -> u64 {
    match num {
        SYS_EMIT => {
            let t = match current() {
                Some(t) => t,
                None => return u64::MAX,
            };
            match t.engine.evaluate(t.action, &Target::default(), &t.caps) {
                Decision::Allow => {
                    t.store.record_event(t.action, "el0-process");
                    t.allowed = true;
                    0
                }
                _ => {
                    t.allowed = false;
                    u64::MAX
                }
            }
        }
        SYS_YIELD => {
            sched_report(arg, false);
            0
        }
        SYS_EXIT => {
            sched_report(arg, true);
            0
        }
        SYS_SEND => {
            // Authorize with the SAME CapEngine, then deposit `arg` into the kernel endpoint.
            let t = match current() {
                Some(t) => t,
                None => return u64::MAX,
            };
            match t.engine.evaluate(t.action, &Target::default(), &t.caps) {
                Decision::Allow => {
                    // SAFETY: single-threaded; only the running task's trap writes the endpoint.
                    unsafe { *addr_of_mut!(ENDPOINT) = Some(arg) };
                    t.allowed = true;
                    0
                }
                _ => {
                    t.allowed = false;
                    u64::MAX
                }
            }
        }
        SYS_RECV => {
            // Authorize, then drain the kernel endpoint into IPC_RECEIVED and hand the body back.
            let t = match current() {
                Some(t) => t,
                None => return u64::MAX,
            };
            match t.engine.evaluate(t.action, &Target::default(), &t.caps) {
                Decision::Allow => {
                    t.allowed = true;
                    // SAFETY: single-threaded; only the running task's trap touches the endpoint.
                    let msg = unsafe { (*addr_of_mut!(ENDPOINT)).take() };
                    match msg {
                        Some(body) => {
                            unsafe { *addr_of_mut!(IPC_RECEIVED) = body };
                            body
                        }
                        None => u64::MAX, // authorized, but the endpoint was empty
                    }
                }
                _ => {
                    t.allowed = false;
                    u64::MAX
                }
            }
        }
        _ => u64::MAX, // unknown syscall — fail closed
    }
}

/// Record what the running task reported this slice.
fn sched_report(magic: u64, exited: bool) {
    // SAFETY: single-threaded; only the running task's trap writes this, read by the scheduler
    // after the excursion returns.
    unsafe {
        let s = &mut *addr_of_mut!(SCHED);
        s.last_magic = magic;
        s.exited = exited;
    }
}

/// Data-abort dispatch. An armed isolation trial treats an EL0 fault as the expected proof and
/// resumes; any UNEXPECTED abort stays fatal (`exit 102`) so bugs cannot hide here.
#[no_mangle]
pub extern "C" fn el0_data_abort(far: u64) -> u64 {
    match current() {
        Some(t) if t.armed => {
            t.isolation_held = true;
            t.fault_va = far as usize;
            t.armed = false;
            0
        }
        _ => {
            kprintln!("[usermode] UNEXPECTED EL0 data abort at {:#x}", far);
            crate::semihosting::exit(102);
        }
    }
}

// ---------------------------------------------------------------------------
// GICv2 + generic timer — the interrupt path that drives INVOLUNTARY preemption.
// QEMU 'virt' GICv2: distributor @ 0x0800_0000, CPU interface @ 0x0801_0000 (already Device-mapped
// in the peripheral GiB by vm.rs). The EL1 physical timer is PPI INTID 30. Requires the machine to
// expose a GICv2 (pin `-machine virt,gic-version=2`).
// ---------------------------------------------------------------------------

const GICD_BASE: usize = 0x0800_0000;
const GICC_BASE: usize = 0x0801_0000;
const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_IAR: usize = 0x00C;
const GICC_EOIR: usize = 0x010;
const TIMER_INTID: u32 = 30; // EL1 physical timer PPI
const SPURIOUS: u32 = 1023;
/// Preemption slice length in timer ticks (CNTFRQ ~62.5 MHz on QEMU 'virt' → ~8 ms). Value is not
/// correctness-critical: a deadline missed while the scheduler runs at EL1 just fires on the next
/// `eret` to EL0 — re-arming `CNTP_TVAL` each IRQ guarantees the next task gets a fresh full slice.
const TIMER_SLICE: u64 = 500_000;

#[inline]
fn gicd_w32(off: usize, v: u32) {
    // SAFETY: GICD is a valid Device-mapped MMIO register at a fixed platform address.
    unsafe { core::ptr::write_volatile((GICD_BASE + off) as *mut u32, v) };
}
#[inline]
fn gicc_w32(off: usize, v: u32) {
    // SAFETY: GICC is a valid Device-mapped MMIO register at a fixed platform address.
    unsafe { core::ptr::write_volatile((GICC_BASE + off) as *mut u32, v) };
}
#[inline]
fn gicc_r32(off: usize) -> u32 {
    // SAFETY: GICC is a valid Device-mapped MMIO register at a fixed platform address.
    unsafe { core::ptr::read_volatile((GICC_BASE + off) as *const u32) }
}

/// Bring up the GIC to deliver the timer PPI. Order matters (advisor + GICv2 spec): enable the
/// distributor, give the timer INTID top priority (0x00), enable that INTID, open the CPU
/// interface's priority mask (PMR=0xF0; a source is delivered only if its priority < PMR), then
/// enable the CPU interface.
fn gic_init() {
    gicd_w32(GICD_CTLR, 1); // enable group 0
                            // IPRIORITYR is byte-addressed per INTID.
                            // SAFETY: valid Device-mapped byte register for INTID 30.
    unsafe {
        core::ptr::write_volatile(
            (GICD_BASE + GICD_IPRIORITYR + TIMER_INTID as usize) as *mut u8,
            0x00,
        )
    };
    gicd_w32(
        GICD_ISENABLER + (TIMER_INTID as usize / 32) * 4,
        1 << (TIMER_INTID % 32),
    );
    gicc_w32(GICC_PMR, 0xF0);
    gicc_w32(GICC_CTLR, 1);
}

/// Tear the GIC + timer back down so the later benchmark (EL1, IRQ-masked) is unperturbed.
fn gic_teardown() {
    timer_disable();
    gicd_w32(
        GICD_ICENABLER + (TIMER_INTID as usize / 32) * 4,
        1 << (TIMER_INTID % 32),
    );
    gicc_w32(GICC_CTLR, 0);
    gicd_w32(GICD_CTLR, 0);
}

/// Arm the EL1 physical timer to fire `TIMER_SLICE` ticks from now (enabled, unmasked).
fn timer_arm() {
    // SAFETY: CNTP_TVAL_EL0 / CNTP_CTL_EL0 are accessible at EL1 (NS-EL1, no EL2 here).
    unsafe {
        asm!("msr cntp_tval_el0, {v}", v = in(reg) TIMER_SLICE, options(nomem, nostack));
        asm!("msr cntp_ctl_el0, {v}", v = in(reg) 1u64, options(nomem, nostack));
        // ENABLE, IMASK=0
    }
}

/// Disable the EL1 physical timer.
fn timer_disable() {
    // SAFETY: CNTP_CTL_EL0 is accessible at EL1.
    unsafe { asm!("msr cntp_ctl_el0, {v}", v = in(reg) 0u64, options(nomem, nostack)) };
}

/// IRQ dispatch (from vector 0x480). Acknowledge the interrupt, RE-ARM the timer BEFORE EOI (the
/// timer condition is level-triggered — EOI without a fresh deadline re-asserts instantly → storm),
/// then mark the running task preempted so the scheduler round-robins to the next one.
#[no_mangle]
pub extern "C" fn el0_irq() {
    let iar = gicc_r32(GICC_IAR) & 0x3FF;
    if iar == SPURIOUS {
        return; // spurious read — no EOI, just resume
    }
    timer_arm(); // re-arm FIRST (level-triggered), before EOI
    gicc_w32(GICC_EOIR, iar);
    // SAFETY: single-threaded; only the running task's IRQ writes this, read by the scheduler.
    unsafe {
        let s = &mut *addr_of_mut!(SCHED);
        s.preempted = true;
    }
}

/// Point `VBAR_EL1` at our vector table so an EL0 `svc`/fault traps to `el0_sync_entry`. The
/// benchmark also does this later; setting it twice is harmless (idempotent).
fn install_vectors() {
    // SAFETY: `exc_vectors` is a valid, 2 KiB-aligned in-image vector table; writing VBAR_EL1
    // + `isb` is the architected way to install it at EL1.
    unsafe {
        let addr = addr_of!(exc_vectors) as u64;
        asm!("msr vbar_el1, {a}", "isb", a = in(reg) addr, options(nostack));
    }
}

/// Clean the data cache to PoU then invalidate the instruction cache over `[addr, addr+len)`,
/// so instructions freshly written into a code frame are actually fetched. Line stride 64 B
/// (QEMU 'virt'); harmless if caches are disabled.
///
/// SAFETY: `addr..addr+len` is inside an identity-mapped, kernel-writable RAM frame.
unsafe fn sync_icache(addr: usize, len: usize) {
    let end = addr + len;
    let mut p = addr & !63;
    while p < end {
        asm!("dc cvau, {a}", a = in(reg) p, options(nostack));
        p += 64;
    }
    asm!("dsb ish", options(nostack));
    let mut p = addr & !63;
    while p < end {
        asm!("ic ivau, {a}", a = in(reg) p, options(nostack));
        p += 64;
    }
    asm!("dsb ish", "isb", options(nostack));
}

/// Map a fresh frame at `va` as EL0-executable code, writing `code` (aarch64 machine words) into
/// it first. Returns the backing frame (caller unmaps+frees).
fn map_user_code(root: usize, va: usize, code: &[u32]) -> Option<frames::PhysFrame> {
    let f = frames::alloc_zeroed()?;
    let pa = f.addr();
    // SAFETY: `pa` is a fresh, identity-mapped, kernel-writable frame; `code` ≤ one page.
    unsafe {
        for (i, w) in code.iter().enumerate() {
            core::ptr::write_volatile((pa + i * 4) as *mut u32, *w);
        }
        sync_icache(pa, code.len() * 4);
    }
    if !vm::map_page(root, va, pa, vm::USER_CODE) {
        frames::free(f);
        return None;
    }
    Some(f)
}

/// Map a fresh EL0 data/stack frame at `va`.
fn map_user_stack(root: usize, va: usize) -> Option<frames::PhysFrame> {
    let f = frames::alloc_zeroed()?;
    if !vm::map_page(root, va, f.addr(), vm::USER_DATA) {
        frames::free(f);
        return None;
    }
    Some(f)
}

/// Tear down a mapped user page and reclaim its frame.
fn drop_user_page(root: usize, va: usize, f: frames::PhysFrame) {
    vm::unmap_page(root, va);
    frames::free(f);
}

/// Run a one-shot EL0 excursion in the current address space: build a fresh-task frame and resume
/// it. Results land in `CURRENT`; the task is never resumed (it traps once, we return).
fn run_one_shot(entry: usize, sp: usize, x0: u64, x8: u64) {
    let mut f = TrapFrame::new_entry(entry, sp, x0, x8);
    // SAFETY: the frame lives for the call; `resume_frame` restores it, runs EL0, and the trap
    // handler resumes us. No other excursion is in flight.
    unsafe { resume_frame(&mut f as *mut TrapFrame) };
}

/// Machine code for a leaf EL0 stub that issues one syscall then parks: `svc #0 ; b .`
/// (the syscall number/arg arrive in x8/x0, primed by the frame).
const STUB_SYSCALL: [u32; 2] = [0xD400_0001, 0x1400_0000];
/// EL0 stub that reads the address handed to it in x0, then parks: `ldr x1,[x0] ; b .`
const STUB_READ_X0: [u32; 2] = [0xF940_0001, 0x1400_0000];
/// EL0 stub that reads x0, then issues a syscall, then parks: `ldr x1,[x0] ; svc #0 ; b .`.
/// If the read faults the `svc` is never reached — a successful syscall is positive proof the
/// read landed (used for the cross-process A case).
const STUB_READ_THEN_SYSCALL: [u32; 3] = [0xF940_0001, 0xD400_0001, 0x1400_0000];

/// Run one EL0 syscall excursion. `grant` decides whether the process holds the `event.emit`
/// capability. Returns `(authorized, event_count_after)`.
fn run_syscall(grant: bool) -> (bool, usize) {
    let mut engine = CapEngine::new(0xA5A5, 1000);
    let mut caps = Vec::new();
    if grant {
        caps.push(engine.mint("el0-process", "event.emit", Scope::All, Constraints::none()));
    }
    let root = vm::active_root();
    // SAFETY: single-threaded; install the trial the handler reads this excursion.
    unsafe {
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine,
            store: Store::new(),
            caps,
            action: "event.emit",
            armed: false,
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
    }
    let code = match map_user_code(root, USER_CODE_VA, &STUB_SYSCALL) {
        Some(f) => f,
        None => return (false, usize::MAX),
    };
    let stack = match map_user_stack(root, USER_STACK_VA) {
        Some(f) => f,
        None => {
            drop_user_page(root, USER_CODE_VA, code);
            return (false, usize::MAX);
        }
    };
    run_one_shot(USER_CODE_VA, USER_STACK_TOP, 0, SYS_EMIT);
    drop_user_page(root, USER_STACK_VA, stack);
    drop_user_page(root, USER_CODE_VA, code);
    // SAFETY: excursion complete; take the trial.
    let t = unsafe { (*addr_of_mut!(CURRENT)).take() }.expect("trial present");
    (t.allowed, t.store.event_count())
}

/// Run the isolation excursion: hand the EL0 stub a kernel-only address and prove it cannot read
/// it — the access must fault and be contained. Returns `(isolation_held, fault_va)`.
fn run_isolation() -> (bool, usize) {
    let root = vm::active_root();
    let kernel_va = addr_of!(KERNEL_CTX) as u64; // kernel .bss, AP = EL1-only
                                                 // SAFETY: single-threaded; arm the isolation test so the abort handler treats the fault as
                                                 // expected rather than fatal.
    unsafe {
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine: CapEngine::new(0xA5A5, 1000),
            store: Store::new(),
            caps: Vec::new(),
            action: "event.emit",
            armed: true,
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
    }
    let code = match map_user_code(root, USER_CODE_VA, &STUB_READ_X0) {
        Some(f) => f,
        None => return (false, 0),
    };
    let stack = match map_user_stack(root, USER_STACK_VA) {
        Some(f) => f,
        None => {
            drop_user_page(root, USER_CODE_VA, code);
            return (false, 0);
        }
    };
    run_one_shot(USER_CODE_VA, USER_STACK_TOP, kernel_va, 0);
    drop_user_page(root, USER_STACK_VA, stack);
    drop_user_page(root, USER_CODE_VA, code);
    let t = unsafe { (*addr_of_mut!(CURRENT)).take() }.expect("trial present");
    (t.isolation_held, t.fault_va)
}

/// Run one EL0 process under a *dedicated* address-space root (switch `TTBR0` around the
/// excursion, restore `root_main` after). `armed` marks whether an EL0 fault is expected.
/// Returns the taken `Trial`. Precondition: `root` replicates the kernel identity map.
fn run_in_space(
    root: usize,
    root_main: usize,
    x0val: u64,
    x8val: u64,
    engine: CapEngine,
    caps: Vec<CapToken>,
    armed: bool,
) -> Trial {
    // SAFETY: single-threaded; install the trial the handler reads this excursion.
    unsafe {
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine,
            store: Store::new(),
            caps,
            action: "event.emit",
            armed,
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
    }
    let mut f = TrapFrame::new_entry(USER_CODE_VA, USER_STACK_TOP, x0val, x8val);
    // SAFETY: `root` identity-maps the running kernel (build_identity), so switching TTBR0
    // mid-execution is safe; restored to root_main immediately after the excursion.
    unsafe {
        vm::switch_address_space(root);
        resume_frame(&mut f as *mut TrapFrame);
        vm::switch_address_space(root_main);
    }
    // SAFETY: excursion complete, no other access to CURRENT exists.
    unsafe { (*addr_of_mut!(CURRENT)).take() }.expect("trial present")
}

/// Prove **per-process address-space isolation**: two EL0 processes in separate TTBR0 spaces,
/// where a page private to process A is unreachable from process B — even at the *same* virtual
/// address. Returns `(a_reached_own_page, b_isolated, b_fault_va)`.
fn run_cross_process_isolation() -> (bool, bool, usize) {
    let root_main = vm::active_root();
    let root_a = match vm::build_identity() {
        Some(r) => r,
        None => return (false, false, 0),
    };
    let root_b = match vm::build_identity() {
        Some(r) => r,
        None => return (false, false, 0),
    };
    // Process A's space: stub + stack + the private data page VA_P.
    let a_code = match map_user_code(root_a, USER_CODE_VA, &STUB_READ_THEN_SYSCALL) {
        Some(f) => f,
        None => return (false, false, 0),
    };
    let a_stack = match map_user_stack(root_a, USER_STACK_VA) {
        Some(f) => f,
        None => return (false, false, 0),
    };
    let a_data = match map_user_stack(root_a, VA_P) {
        Some(f) => f,
        None => return (false, false, 0),
    };
    // Process B's space: stub + stack only — VA_P deliberately left unmapped.
    let b_code = match map_user_code(root_b, USER_CODE_VA, &STUB_READ_THEN_SYSCALL) {
        Some(f) => f,
        None => return (false, false, 0),
    };
    let b_stack = match map_user_stack(root_b, USER_STACK_VA) {
        Some(f) => f,
        None => return (false, false, 0),
    };

    // A reads its own VA_P (mapped) then makes an authorized syscall -> allowed proves both.
    let mut a_engine = CapEngine::new(0xA5A5, 1000);
    let a_caps =
        alloc::vec![a_engine.mint("process-a", "event.emit", Scope::All, Constraints::none())];
    let a = run_in_space(
        root_a,
        root_main,
        VA_P as u64,
        SYS_EMIT,
        a_engine,
        a_caps,
        false,
    );
    // B reads the SAME VA_P (unmapped in its space) -> armed translation fault, contained.
    let b = run_in_space(
        root_b,
        root_main,
        VA_P as u64,
        0,
        CapEngine::new(0xA5A5, 1000),
        Vec::new(),
        true,
    );

    // Reclaim leaf frames (the two identity page-table trees are an intentional, bounded,
    // one-time boot-test leak — the pool has ~30k frames and this runs once).
    drop_user_page(root_a, VA_P, a_data);
    drop_user_page(root_a, USER_STACK_VA, a_stack);
    drop_user_page(root_a, USER_CODE_VA, a_code);
    drop_user_page(root_b, USER_STACK_VA, b_stack);
    drop_user_page(root_b, USER_CODE_VA, b_code);

    (a.allowed, b.isolation_held, b.fault_va)
}

// ---------------------------------------------------------------------------
// Capability-secure kernel IPC (architecture gap register, Issue 2). Two EL0 processes in SEPARATE
// address spaces exchange a message through a kernel endpoint — the message crosses the boundary
// only via the kernel, authorized by the SAME `CapEngine`, never through shared user memory.
// ---------------------------------------------------------------------------

/// Run one endpoint excursion in address space `root`: an EL0 process with (optionally) an `action`
/// capability issues `x8`(syscall)/`x0`(arg) and traps once. Returns whether the syscall was
/// authorized. Precondition: `root` already maps the syscall stub + stack at the user VAs.
fn run_endpoint_excursion(
    root: usize,
    root_main: usize,
    action: &'static str,
    grant: bool,
    x0: u64,
    x8: u64,
) -> bool {
    let mut engine = CapEngine::new(0xA5A5, 1000);
    let mut caps = Vec::new();
    if grant {
        caps.push(engine.mint("ipc-process", action, Scope::All, Constraints::none()));
    }
    // SAFETY: single-threaded; install the trial the handler reads for this excursion.
    unsafe {
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine,
            store: Store::new(),
            caps,
            action,
            armed: false,
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
    }
    let mut f = TrapFrame::new_entry(USER_CODE_VA, USER_STACK_TOP, x0, x8);
    // SAFETY: `root` identity-maps the running kernel; switch into it, run the EL0 excursion until
    // it traps, restore the caller's space. The frame lives in kernel RAM (identity-mapped in root).
    unsafe {
        vm::switch_address_space(root);
        resume_frame(&mut f as *mut TrapFrame);
        vm::switch_address_space(root_main);
    }
    let t = unsafe { (*addr_of_mut!(CURRENT)).take() }.expect("trial present");
    t.allowed
}

/// Prove **capability-secure kernel IPC**: a message sent by one EL0 process is delivered to another
/// EL0 process in a DIFFERENT address space, through the kernel endpoint, only when both hold the
/// authorizing capability. Returns `(delivered_across_spaces, uncapable_send_denied,
/// uncapable_recv_denied)`.
fn run_ipc() -> (bool, bool, bool) {
    let root_main = vm::active_root();
    let root_a = match vm::build_identity() {
        Some(r) => r,
        None => return (false, false, false),
    };
    let root_b = match vm::build_identity() {
        Some(r) => r,
        None => return (false, false, false),
    };
    // Sender (A) and receiver (B) each get the syscall stub + a stack in their OWN space.
    let a_code = map_user_code(root_a, USER_CODE_VA, &STUB_SYSCALL);
    let a_stack = map_user_stack(root_a, USER_STACK_VA);
    let b_code = map_user_code(root_b, USER_CODE_VA, &STUB_SYSCALL);
    let b_stack = map_user_stack(root_b, USER_STACK_VA);
    if a_code.is_none() || a_stack.is_none() || b_code.is_none() || b_stack.is_none() {
        if let Some(f) = a_stack {
            drop_user_page(root_a, USER_STACK_VA, f);
        }
        if let Some(f) = a_code {
            drop_user_page(root_a, USER_CODE_VA, f);
        }
        if let Some(f) = b_stack {
            drop_user_page(root_b, USER_STACK_VA, f);
        }
        if let Some(f) = b_code {
            drop_user_page(root_b, USER_CODE_VA, f);
        }
        return (false, false, false);
    }

    let body: u64 = 0xC0FF_EE42;

    // 1 — capability-secure delivery: capable sender deposits, capable receiver drains, and the body
    // survives the trip through the kernel between two genuinely distinct address spaces.
    // SAFETY: single-threaded reset of the endpoint before the exchange.
    unsafe {
        *addr_of_mut!(ENDPOINT) = None;
        *addr_of_mut!(IPC_RECEIVED) = 0;
    }
    let send_ok = run_endpoint_excursion(root_a, root_main, "ipc.send", true, body, SYS_SEND);
    let recv_ok = run_endpoint_excursion(root_b, root_main, "ipc.recv", true, 0, SYS_RECV);
    let received = unsafe { *addr_of!(IPC_RECEIVED) };
    let spaces_distinct = root_a != root_b && root_a != root_main && root_b != root_main;
    let delivered = send_ok && recv_ok && received == body && spaces_distinct;

    // 2 — an EL0 process WITHOUT `ipc.send` cannot post to the endpoint (fail-closed, slot untouched).
    // SAFETY: single-threaded reset.
    unsafe { *addr_of_mut!(ENDPOINT) = None };
    let bad_send = run_endpoint_excursion(root_a, root_main, "ipc.send", false, body, SYS_SEND);
    let send_denied = !bad_send && unsafe { (*addr_of!(ENDPOINT)).is_none() };

    // 3 — an EL0 process WITHOUT `ipc.recv` cannot drain a queued message (fail-closed, slot intact).
    // SAFETY: single-threaded seed of a queued message.
    unsafe { *addr_of_mut!(ENDPOINT) = Some(body) };
    let bad_recv = run_endpoint_excursion(root_b, root_main, "ipc.recv", false, 0, SYS_RECV);
    let recv_denied = !bad_recv && unsafe { (*addr_of!(ENDPOINT)).is_some() };

    // SAFETY: excursions complete; reclaim the leaf pages (the table trees are the same intentional
    // bounded one-time boot-test leak the other multi-space tests take).
    drop_user_page(root_a, USER_STACK_VA, a_stack.expect("mapped"));
    drop_user_page(root_a, USER_CODE_VA, a_code.expect("mapped"));
    drop_user_page(root_b, USER_STACK_VA, b_stack.expect("mapped"));
    drop_user_page(root_b, USER_CODE_VA, b_code.expect("mapped"));

    (delivered, send_denied, recv_denied)
}

// ---------------------------------------------------------------------------
// Cooperative multitasking: two EL0 tasks context-switch via yield under a round-robin scheduler.
// ---------------------------------------------------------------------------

// Scheduler tasks share the SAME user VAs (`USER_CODE_VA`/`USER_STACK_VA`) but live in SEPARATE
// address spaces — so the per-slice TTBR0 switch is the ONLY thing routing that VA to the right
// task's code. Timer-driven preemption is the follow-on.

/// EL0 task stub for a given 16-bit `magic`: set `x19 = magic` once, then yield three times and
/// exit — replaying `magic` in `x0` (the syscall arg) before every `svc`. Because `x19` and `x8`
/// are only ever set at the top yet survive across yields, a task that keeps presenting its own
/// magic proves the WHOLE register file is saved/restored on each context switch.
///
/// `mov x19,#M ; (mov x0,x19 ; svc)×3 with x8=YIELD ; mov x8,#EXIT ; mov x0,x19 ; svc ; b .`
fn stub_for(magic: u64) -> [u32; 12] {
    let m = (magic & 0xFFFF) as u32;
    [
        0xD280_0013 | (m << 5), // movz x19, #magic
        0xAA13_03E0,            // mov  x0, x19
        0xD280_0048,            // movz x8, #2  (SYS_YIELD)
        0xD400_0001,            // svc  #0      (yield 1)
        0xAA13_03E0,            // mov  x0, x19
        0xD400_0001,            // svc  #0      (yield 2, x8 still YIELD)
        0xAA13_03E0,            // mov  x0, x19
        0xD400_0001,            // svc  #0      (yield 3)
        0xAA13_03E0,            // mov  x0, x19
        0xD280_0068,            // movz x8, #3  (SYS_EXIT)
        0xD400_0001,            // svc  #0      (exit)
        0x1400_0000,            // b .          (never reached)
    ]
}

/// Unmap+free each task's mapped leaf pages in its own root. (The identity page-table trees are an
/// intentional, bounded, one-time boot-test leak — the pool has ~30k frames, this runs once.)
fn cleanup_tasks(
    roots: &[usize; NTASK],
    code: &mut [Option<frames::PhysFrame>; NTASK],
    stack: &mut [Option<frames::PhysFrame>; NTASK],
) {
    for i in 0..NTASK {
        if let Some(f) = stack[i].take() {
            drop_user_page(roots[i], USER_STACK_VA, f);
        }
        if let Some(f) = code[i].take() {
            drop_user_page(roots[i], USER_CODE_VA, f);
        }
    }
}

/// Run the round-robin scheduler over two cooperative EL0 tasks, EACH IN ITS OWN ADDRESS SPACE.
/// Returns `(round_robin_and_both_exited, every_slice_presented_its_own_magic, spaces_distinct)`.
fn run_scheduler() -> (bool, bool, bool) {
    let root_main = vm::active_root();
    let magics: [u64; NTASK] = [0xA1A1, 0xB2B2];
    let mut roots = [0usize; NTASK];
    let mut code: [Option<frames::PhysFrame>; NTASK] = [None, None];
    let mut stack: [Option<frames::PhysFrame>; NTASK] = [None, None];
    // Each task gets its own TTBR0 root; both use the SAME user VAs (isolated only by the space).
    for i in 0..NTASK {
        roots[i] = match vm::build_identity() {
            Some(r) => r,
            None => {
                cleanup_tasks(&roots, &mut code, &mut stack);
                return (false, false, false);
            }
        };
        code[i] = map_user_code(roots[i], USER_CODE_VA, &stub_for(magics[i]));
        stack[i] = map_user_stack(roots[i], USER_STACK_VA);
        if code[i].is_none() || stack[i].is_none() {
            cleanup_tasks(&roots, &mut code, &mut stack);
            return (false, false, false);
        }
    }
    // SAFETY: single-threaded; init the TCBs before any resume. Same entry/stack VA for both —
    // the distinct roots are what make them separate address spaces.
    unsafe {
        let tcbs = &mut *addr_of_mut!(TCBS);
        for tcb in tcbs.iter_mut() {
            *tcb = Tcb {
                frame: TrapFrame::new_entry(USER_CODE_VA, USER_STACK_TOP, 0, 0),
                done: false,
            };
        }
    }

    // Scheduling POLICY driven by the shared arch-independent kernel_core::sched::RoundRobin
    // (REQ-KERN-005): the target no longer hand-rolls the rotation — it drives the SAME scheduler
    // proved on the host and performs only the context-switch MECHANISM (resume_frame +
    // address-space switch) behind the TaskContext seam. `schedule_next` decides which task runs; a
    // yielded task stays Running and is rotated to the tail on the next call; an exited task is
    // `finish`ed and leaves the rotation. This reproduces the exact A,B,A,B,A,B,A,B sequence the
    // bespoke loop did (proved by the assertions below), now from one source of truth.
    let mut policy = RoundRobin::new();
    for i in 0..NTASK {
        policy.spawn(TaskId(i as u64));
    }
    let mut order: Vec<(usize, u64)> = Vec::new();
    while let Some(TaskId(id)) = policy.schedule_next() {
        let slot = id as usize;
        sched_report(0, false); // reset for this slice
                                // SAFETY: roots[slot] identity-maps the kernel; switch into the task's space, resume it
                                // until it yields/exits, then restore the scheduler's space. The frame lives in the static
                                // TCB (kernel data, identity-mapped in every root), so the entry-time save works pre-switch.
        unsafe {
            vm::switch_address_space(roots[slot]);
            resume_frame(&mut (*addr_of_mut!(TCBS))[slot].frame as *mut TrapFrame);
            vm::switch_address_space(root_main);
        }
        let (mag, exited) = unsafe {
            let s = &*addr_of!(SCHED);
            (s.last_magic, s.exited)
        };
        order.push((slot, mag));
        if exited {
            // SAFETY: single-threaded write of run state.
            unsafe { (*addr_of_mut!(TCBS))[slot].done = true };
            policy.finish(TaskId(id)); // leaves the rotation; schedule_next ends when none remain
        }
        if order.len() > 4 * NTASK {
            break; // safety bound — a correct run is exactly 2*NTASK*... (8) slices
        }
    }

    cleanup_tasks(&roots, &mut code, &mut stack);

    // Expected: 4 slices per task (3 yields + 1 exit), strictly alternating A,B,A,B,A,B,A,B.
    let expected_slots = [0usize, 1, 0, 1, 0, 1, 0, 1];
    let order_ok = order.len() == 8
        && order
            .iter()
            .zip(expected_slots.iter())
            .all(|((slot, _), exp)| slot == exp);
    let both_done = unsafe {
        let t = &*addr_of!(TCBS);
        t[0].done && t[1].done
    };
    // Every slice must report the magic of the task that actually ran it — proof the full register
    // file (x19 magic + x8 syscall number) rode through each context switch intact. And because
    // both tasks share ONE code VA in DIFFERENT spaces, a correct magic each slice ALSO proves the
    // per-slice TTBR0 switch happened (else a task would execute the other's stub at that VA).
    let magic_ok = order.len() == 8 && order.iter().all(|(slot, mag)| *mag == magics[*slot]);
    let spaces_distinct = roots[0] != roots[1] && roots[0] != root_main && roots[1] != root_main;
    (order_ok && both_done, magic_ok, spaces_distinct)
}

/// EL0 spin-task stub for the preemption test: increment x19 (progress) while counting x20 down;
/// if it ever drains x20 it exits (`SYS_EXIT`) — a *bounded* fallback so a NEVER-FIRING timer fails
/// cleanly (the task self-exits, the scheduler sees an unexpected exit) instead of hanging. A
/// working timer preempts long before x20 drains.
/// `loop: add x19,x19,#1 ; subs x20,x20,#1 ; b.ne loop ; mov x8,#EXIT ; svc ; b .`
const STUB_SPIN: [u32; 6] = [
    0x9100_0673, // add  x19, x19, #1   (loop:)
    0xF100_0694, // subs x20, x20, #1
    0x54FF_FFC1, // b.ne loop  (-8)
    0xD280_0068, // movz x8, #3 (SYS_EXIT)
    0xD400_0001, // svc  #0
    0x1400_0000, // b .
];

/// Countdown preloaded into x20: large enough that a working timer preempts before it drains,
/// small enough that a BROKEN timer drains it (task self-exits) well within the VM watchdog.
const SPIN_COUNTDOWN: u64 = 0x2000_0000;

/// Prove **timer-driven (involuntary) preemption**: two EL0 tasks that never yield (tight
/// increment loops, IRQ unmasked) are preempted by the generic-timer IRQ and round-robined by the
/// scheduler. Returns `(both_tasks_preempted_fairly, each_task_progressed_across_preemptions)`.
fn run_preemptive() -> (bool, bool) {
    let root_main = vm::active_root();
    let mut roots = [0usize; NTASK];
    let mut code: [Option<frames::PhysFrame>; NTASK] = [None, None];
    let mut stack: [Option<frames::PhysFrame>; NTASK] = [None, None];
    for i in 0..NTASK {
        roots[i] = match vm::build_identity() {
            Some(r) => r,
            None => {
                cleanup_tasks(&roots, &mut code, &mut stack);
                return (false, false);
            }
        };
        code[i] = map_user_code(roots[i], USER_CODE_VA, &STUB_SPIN);
        stack[i] = map_user_stack(roots[i], USER_STACK_VA);
        if code[i].is_none() || stack[i].is_none() {
            cleanup_tasks(&roots, &mut code, &mut stack);
            return (false, false);
        }
    }
    // Preemptible frames: IRQ unmasked (SPSR 0x340), x20 = bounded fallback countdown.
    // SAFETY: single-threaded; init the TCBs before any resume.
    unsafe {
        let tcbs = &mut *addr_of_mut!(TCBS);
        for tcb in tcbs.iter_mut() {
            let mut f = TrapFrame::new_entry(USER_CODE_VA, USER_STACK_TOP, 0, 0);
            f.spsr = SPSR_EL0T_IRQ;
            f.regs[20] = SPIN_COUNTDOWN;
            *tcb = Tcb {
                frame: f,
                done: false,
            };
        }
    }

    gic_init();
    timer_arm();

    const SLICES: usize = 6;
    let mut counts = [0usize; NTASK];
    let mut last_x19 = [0u64; NTASK];
    let mut seen = [false; NTASK];
    let mut progress_ok = true;
    let mut clean = true; // no task self-exited (i.e. the timer actually fired every slice)
    let mut cur = 0usize;
    for _ in 0..SLICES {
        let slot = cur % NTASK;
        // SAFETY: single-threaded reset of the slice report.
        unsafe {
            let s = &mut *addr_of_mut!(SCHED);
            s.preempted = false;
            s.exited = false;
        }
        // SAFETY: roots[slot] identity-maps the kernel; run the task until the timer preempts it.
        unsafe {
            vm::switch_address_space(roots[slot]);
            resume_frame(&mut (*addr_of_mut!(TCBS))[slot].frame as *mut TrapFrame);
            vm::switch_address_space(root_main);
        }
        let (was_preempt, was_exit, x19) = unsafe {
            let s = &*addr_of!(SCHED);
            (
                s.preempted,
                s.exited,
                (*addr_of!(TCBS))[slot].frame.regs[19],
            )
        };
        if was_exit || !was_preempt {
            clean = false; // timer never fired (countdown drained) or an unexpected return
            break;
        }
        if seen[slot] && x19 <= last_x19[slot] {
            progress_ok = false; // counter did not advance across the involuntary switch
        }
        seen[slot] = true;
        last_x19[slot] = x19;
        counts[slot] += 1;
        cur = (cur + 1) % NTASK;
    }

    gic_teardown();
    cleanup_tasks(&roots, &mut code, &mut stack);

    let fair = clean && counts.iter().all(|&c| c > 0);
    (fair, progress_ok && clean)
}

/// Shared VA for the grant-table test — an unused slot in the same `0x5000_xxxx` hole as the other
/// user pages (above QEMU RAM, so it is NOT identity-mapped: a real per-process translation).
const SHARED_VA: usize = 0x5000_5000;

/// Prove the zero-copy shared-memory grant-table (REQ-IPC-008) through the REAL aarch64 MMU path,
/// converting it from hosted-only to VM-gated. The shared `GrantTable` is the arch-independent
/// authority/lifecycle layer (capability check + attenuation + revocation); THIS target's `vm.rs`
/// performs the actual page mapping — the seam the module documents. Proves, live:
///   * a `memory.share` grant maps ONE physical frame into TWO distinct process address spaces
///     (their own TTBR0 roots), so both resolve the SAME physical frame — zero-copy across AS;
///   * establishing the grant is capability-gated (no `memory.share` ⇒ no grant, and we map nothing);
///   * revoking the grant unmaps the grantee's page while leaving the grantor's intact.
///
/// Returns `(cap_gated, shared_across_spaces, revoke_unmaps)`.
fn run_shared_memory() -> (bool, bool, bool) {
    let (root_a, root_b) = match (vm::build_identity(), vm::build_identity()) {
        (Some(a), Some(b)) => (a, b),
        _ => return (false, false, false),
    };
    let shf = match frames::alloc_zeroed() {
        Some(f) => f,
        None => return (false, false, false),
    };
    let pa = shf.addr();

    // Authority to share is a `memory.share` capability, checked by the SAME CapEngine the pipeline
    // uses. The grant-table records the region owned by proc-a at the frame's physical base.
    let mut engine = CapEngine::new(0x5EED, 1000);
    let share_cap = engine.mint("proc-a", "memory.share", Scope::All, Constraints::none());
    let mut gt = GrantTable::new("memory.share");
    let region = gt.create_region("proc-a", pa as u64, frames::FRAME_SIZE);

    // (cap_gated) Fail-closed: with NO capability offered, the share is refused and nothing maps.
    let denied = gt
        .share(
            &engine,
            region,
            "proc-a",
            "proc-b",
            ShareMode::ReadWrite,
            &[],
        )
        .is_err();
    // …and WITH the capability the share is authorized (attenuation checked in the hosted suite).
    let granted = gt.share(
        &engine,
        region,
        "proc-a",
        "proc-b",
        ShareMode::ReadWrite,
        &[share_cap],
    );
    let cap_gated = denied && granted.is_ok();

    // On the authorized grant, the target maps the ONE frame into BOTH roots at the shared VA.
    let mapped = granted.is_ok()
        && vm::map_page(root_a, SHARED_VA, pa, vm::USER_DATA)
        && vm::map_page(root_b, SHARED_VA, pa, vm::USER_DATA);

    // (shared_across_spaces) Both distinct roots translate the shared VA to the SAME frame — one
    // physical page present in two separate address spaces is exactly zero-copy shared memory.
    let shared_across_spaces = mapped
        && root_a != root_b
        && vm::translate(root_a, SHARED_VA) == Some(pa)
        && vm::translate(root_b, SHARED_VA) == Some(pa);

    // (revoke_unmaps) Revoking the grant tears down the grantee's mapping (the per-target seam:
    // revocation ⇒ unmap) while the grantor keeps its own access.
    let grant_id = granted.unwrap_or(0);
    let revoked = gt.revoke(grant_id);
    vm::unmap_page(root_b, SHARED_VA);
    let revoke_unmaps = revoked
        && vm::translate(root_b, SHARED_VA).is_none()
        && vm::translate(root_a, SHARED_VA) == Some(pa);

    // Cleanup: unmap the grantor and reclaim the shared frame (the two identity roots are left, as
    // the other multitasking tests do — one-shot boot excursions on a 41k-frame pool).
    vm::unmap_page(root_a, SHARED_VA);
    frames::free(shf);

    (cap_gated, shared_across_spaces, revoke_unmaps)
}

/// Prove the EL0 boundary + multitasking invariants live. `Ok(n)` all passed; `Err((idx,name))`.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    install_vectors();
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

    // 1 — an EL0 process with NO capability cannot cross the boundary: syscall denied, no effect.
    let (allowed, events) = run_syscall(false);
    check!(
        !allowed && events == 0,
        "el0: uncapable process — syscall denied at the boundary, zero effect"
    );

    // 2 — a capability-granted EL0 process performs EXACTLY the authorized effect (one event).
    let (allowed, events) = run_syscall(true);
    check!(
        allowed && events == 1,
        "el0: capable process — syscall authorized via the same CapEngine, one event recorded"
    );

    // 3 — hardware isolation: an EL0 read of kernel memory faults and is contained (not fatal).
    let (held, fault_va) = run_isolation();
    check!(
        held && fault_va == addr_of!(KERNEL_CTX) as usize,
        "el0: EL0 read of kernel memory faults — address-space isolation holds"
    );

    // 4 & 5 — per-process address spaces: a page private to process A is reachable by A but NOT
    // by process B at the SAME virtual address (each process has its own TTBR0 space).
    let (a_reached, b_isolated, b_fault_va) = run_cross_process_isolation();
    check!(
        a_reached,
        "el0: process A reaches a page in its own address space (mapped VA resolves)"
    );
    check!(
        b_isolated && b_fault_va == VA_P,
        "el0: process B cannot reach A's page at the same VA — per-process isolation holds"
    );

    // 6, 7 & 8 — cooperative multitasking with per-task address spaces: two EL0 tasks in SEPARATE
    // TTBR0 spaces context-switch via yield under a round-robin scheduler, each resuming with full
    // register state, and the two tasks occupy genuinely distinct address spaces.
    let (order_ok, magic_ok, spaces_distinct) = run_scheduler();
    check!(
        order_ok,
        "el0: round-robin scheduler runs two tasks (each in its own space) A,B,A,B,... to completion"
    );
    check!(
        magic_ok,
        "el0: each task resumes with its own magic at the shared VA — full context + per-slice AS switch"
    );
    check!(
        spaces_distinct,
        "el0: the two scheduled tasks occupy distinct TTBR0 address spaces"
    );

    // 9 & 10 — timer-driven (involuntary) preemption: two non-yielding EL0 tasks are preempted by
    // the generic-timer IRQ and round-robined; each resumes with its progress counter intact.
    let (preempt_fair, preempt_progress) = run_preemptive();
    check!(
        preempt_fair,
        "el0: generic-timer IRQ preempts two non-yielding tasks — scheduler round-robins both"
    );
    check!(
        preempt_progress,
        "el0: each task's register counter advances across timer preemptions — state preserved"
    );

    // 11, 12 & 13 — capability-secure kernel IPC (gap register Issue 2): a message crosses from one
    // EL0 process to another in a DIFFERENT address space only through the kernel endpoint, gated by
    // the same CapEngine; an uncapable sender/receiver is denied fail-closed.
    let (delivered, send_denied, recv_denied) = run_ipc();
    check!(
        delivered,
        "el0: capability-secure IPC — message delivered kernel-mediated across distinct address spaces"
    );
    check!(
        send_denied,
        "el0: IPC send without the ipc.send capability is denied — endpoint untouched (fail-closed)"
    );
    check!(
        recv_denied,
        "el0: IPC recv without the ipc.recv capability is denied — queued message intact (fail-closed)"
    );

    // 14, 15 & 16 — zero-copy shared memory (gap register Issue 2 / REQ-IPC-008): a memory.share
    // grant maps ONE physical frame into TWO distinct process address spaces (zero-copy across AS),
    // establishing it is capability-gated (fail-closed), and revocation unmaps the grantee's page.
    let (cap_gated, shared_across_spaces, revoke_unmaps) = run_shared_memory();
    check!(
        cap_gated,
        "el0: shared-memory grant is capability-gated — no memory.share ⇒ no grant, nothing mapped (fail-closed)"
    );
    check!(
        shared_across_spaces,
        "el0: grant-table maps one frame into two distinct TTBR0 spaces — zero-copy shared memory across address spaces"
    );
    check!(
        revoke_unmaps,
        "el0: revoking the grant unmaps the grantee's page while the grantor keeps access"
    );

    Ok(n)
}
