//! U-mode (unprivileged) user-mode brick — the RISC-V S/U privilege boundary made real, bringing
//! this first-class target to parity with the aarch64 EL0 suite (`kernel/src/usermode.rs`) and the
//! x86-64 ring-3 suite. Until now every invariant was re-proved *in S-mode kernel space*; this wave
//! drops the CPU to **U-mode** (unprivileged), runs a genuinely less-privileged instruction stream
//! in its own U-only pages, and lets it reach the OS through *exactly one door*: an `ecall` trap
//! that lands in the S-mode vector and is authorized by the **same `CapEngine`** the deterministic
//! pipeline uses. Contract-honest (ADR-010): written outside-in, boot-verified; an *unexpected*
//! trap stays fatal (`exit 102`) so a real bug can never masquerade as a pass.
//!
//! WHAT IT PROVES (16 invariants, identical in spirit to the other two targets):
//!   1-2  cap-gated `ecall` syscall — no capability ⇒ denied, zero effect; granted ⇒ one event.
//!   3    hardware isolation — a U-mode load of a supervisor-only (no-`U`) page faults, contained.
//!   4-5  per-process address spaces — A reaches its own page; B cannot reach A's VA (own `satp`).
//!   6-8  cooperative round-robin scheduler — two tasks in distinct spaces run A,B,A,B… to exit.
//!   9-10 timer preemption — the S-mode timer IRQ preempts two non-yielding tasks; state survives.
//!   11-13 capability-secure IPC — kernel-mediated message across spaces; send/recv fail-closed.
//!   14-16 zero-copy shared memory — a memory.share grant maps one frame into two satp spaces
//!         (REQ-IPC-008); cap-gated fail-closed; revocation unmaps the grantee (see run_shared_memory).
//!
//! RISC-V SPECIFICS vs aarch64: `sscratch` holds the *current task's frame pointer* while it runs
//! (the trap entry swaps it into `sp` in one `csrrw`); `x0` is hardwired zero so there is no
//! "free x0 first" dance; SP is an ordinary GPR (saved with the rest); the resume PC is `sepc` and
//! the resume status is `sstatus` (`SPP`=target privilege, `SPIE`); freshly written user code is
//! made fetchable with `fence.i`; and there is NO interrupt-controller dance — the S-mode timer is
//! armed through the SBI TIME extension and enabled with `sie.STIE`, cleared purely by re-arming.
#![allow(clippy::missing_safety_doc)]
use crate::spine::{CapEngine, CapToken, Constraints, Decision, Scope, Store, Target};
use crate::{arch, frames, sbi, vm};
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::ptr::{addr_of, addr_of_mut};
// REQ-KERN-005: the RISC-V target DRIVES the shared arch-independent scheduling policy from
// kernel-core rather than hand-rolling its own rotation — kernel-core decides which task runs next;
// this module performs only the context-switch MECHANISM (run_one_shot + satp address-space switch).
use kernel_core::sched::{RoundRobin, TaskId};
// REQ-IPC-008: the shared grant-table is the arch-independent authority/lifecycle layer over a
// shared-memory region; THIS target's Sv39 `vm.rs` performs the real page mapping into each space.
use kernel_core::grant::{GrantTable, ShareMode};

// --- Syscall ABI (fail-closed): number in a7, arg in a0, result in a0 -----------------------
const SYS_EMIT: u64 = 1;
const SYS_YIELD: u64 = 2;
const SYS_EXIT: u64 = 3;
const SYS_SEND: u64 = 4;
const SYS_RECV: u64 = 5;

// --- User virtual addresses (deliberately in the empty level-2 slot 1, past the peripheral GiB
//     and below RAM at 0x8000_0000 — so they are unmapped by the identity map and prove real
//     U-only translations, never identity). -------------------------------------------------
const USER_CODE_VA: usize = 0x5000_0000;
const USER_STACK_VA: usize = 0x5000_1000;
const USER_STACK_TOP: usize = USER_STACK_VA + frames::FRAME_SIZE;
const VA_P: usize = 0x5000_3000; // per-process private data page (cross-process isolation test)

// --- sstatus / sie bit fields ---------------------------------------------------------------
const SSTATUS_SPP: u64 = 1 << 8; // Previous privilege (1=S, 0=U) — cleared to return to U-mode
const SSTATUS_SPIE: u64 = 1 << 5; // Previous interrupt-enable, restored into SIE on `sret`
const SIE_STIE: u64 = 1 << 5; // Supervisor Timer Interrupt Enable

// --- Timer preemption tuning (QEMU virt `time` CSR = 10 MHz) --------------------------------
const SLICE_TICKS: u64 = 50_000; // ~5 ms slice: long enough to run, short enough to preempt fast
const SPIN_COUNTDOWN: u64 = 0x1000_0000; // dead-timer escape: a never-firing timer self-exits < watchdog
const SLICES: usize = 6; // 3 preemptions per task
const NTASK: usize = 2;

// -------------------------------------------------------------------------------------------
// TrapFrame — the full register-save context. `#[repr(C)]` fixes the byte offsets the asm hard-
// codes: regs[i] at i*8 (x0..x31), sepc at 256, sstatus at 264. x0 is hardwired zero (its slot is
// never used); x2 (sp) rides in the array like any other GPR.
// -------------------------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy)]
struct TrapFrame {
    regs: [u64; 32],
    sepc: u64,
    sstatus: u64,
}

impl TrapFrame {
    const fn zeroed() -> Self {
        TrapFrame {
            regs: [0; 32],
            sepc: 0,
            sstatus: 0,
        }
    }
}

/// Build a fresh U-mode task frame: PC=`entry`, sp=`sp`, a0=`a0`, s2=`s2`, s3=`s3`, and an sstatus
/// that returns to U-mode (SPP=0) with SPIE set. Base sstatus is read live so FP/other state bits
/// are preserved.
fn make_frame(entry: usize, sp: usize, a0: u64, s2: u64, s3: u64) -> TrapFrame {
    let mut f = TrapFrame::zeroed();
    f.regs[10] = a0; // a0
    f.regs[18] = s2; // s2 — magic / progress counter (set once, echoed back to prove context)
    f.regs[19] = s3; // s3 — spin countdown (dead-timer escape)
    f.regs[2] = sp as u64; // sp
    f.sepc = entry as u64;
    let cur: u64;
    // SAFETY: reading sstatus is always sound at S-mode.
    unsafe { asm!("csrr {}, sstatus", out(reg) cur, options(nomem, nostack)) };
    f.sstatus = (cur & !SSTATUS_SPP) | SSTATUS_SPIE;
    f
}

/// Kernel-side callee-saved stash for the `resume_frame`/`resume_return` coroutine handoff:
/// `[ra, sp, s0..s11]` (RISC-V calling convention). One resume in flight at a time → one slot.
#[no_mangle]
static mut KERNEL_CTX: [u64; 14] = [0; 14];

global_asm!(
    r#"
.section .text
.balign 4

# resume_frame(frame: *mut TrapFrame) — save kernel callee-saved into KERNEL_CTX, load the user
# frame, and `sret` to U-mode. It "returns" (via resume_return) only once the task traps back.
.global resume_frame
resume_frame:
    la    t0, KERNEL_CTX
    sd    ra,  0*8(t0)
    sd    sp,  1*8(t0)
    sd    s0,  2*8(t0)
    sd    s1,  3*8(t0)
    sd    s2,  4*8(t0)
    sd    s3,  5*8(t0)
    sd    s4,  6*8(t0)
    sd    s5,  7*8(t0)
    sd    s6,  8*8(t0)
    sd    s7,  9*8(t0)
    sd    s8, 10*8(t0)
    sd    s9, 11*8(t0)
    sd    s10,12*8(t0)
    sd    s11,13*8(t0)
    csrw  sscratch, a0            # sscratch = current task frame ptr (recovered on next trap)
    ld    t0, 256(a0)
    csrw  sepc, t0
    ld    t0, 264(a0)
    csrw  sstatus, t0
    ld    x1,   1*8(a0)
    ld    x2,   2*8(a0)
    ld    x3,   3*8(a0)
    ld    x4,   4*8(a0)
    ld    x5,   5*8(a0)
    ld    x6,   6*8(a0)
    ld    x7,   7*8(a0)
    ld    x8,   8*8(a0)
    ld    x9,   9*8(a0)
    ld    x11, 11*8(a0)
    ld    x12, 12*8(a0)
    ld    x13, 13*8(a0)
    ld    x14, 14*8(a0)
    ld    x15, 15*8(a0)
    ld    x16, 16*8(a0)
    ld    x17, 17*8(a0)
    ld    x18, 18*8(a0)
    ld    x19, 19*8(a0)
    ld    x20, 20*8(a0)
    ld    x21, 21*8(a0)
    ld    x22, 22*8(a0)
    ld    x23, 23*8(a0)
    ld    x24, 24*8(a0)
    ld    x25, 25*8(a0)
    ld    x26, 26*8(a0)
    ld    x27, 27*8(a0)
    ld    x28, 28*8(a0)
    ld    x29, 29*8(a0)
    ld    x30, 30*8(a0)
    ld    x31, 31*8(a0)
    ld    x10, 10*8(a0)           # a0 last (it was the frame base)
    sret

# _user_trap_entry — S-mode trap vector while U-mode tasks run. Save-first: `csrrw sp, sscratch, sp`
# atomically brings the current frame ptr into sp (and stashes the user sp into sscratch), then every
# GPR is stored through it. Dispatches in Rust, then returns to the scheduler via resume_return.
.balign 4
.global _user_trap_entry
_user_trap_entry:
    csrrw sp, sscratch, sp        # sp = frame ptr; sscratch = user sp
    sd    x1,   1*8(sp)
    sd    x3,   3*8(sp)
    sd    x4,   4*8(sp)
    sd    x5,   5*8(sp)
    sd    x6,   6*8(sp)
    sd    x7,   7*8(sp)
    sd    x8,   8*8(sp)
    sd    x9,   9*8(sp)
    sd    x10, 10*8(sp)
    sd    x11, 11*8(sp)
    sd    x12, 12*8(sp)
    sd    x13, 13*8(sp)
    sd    x14, 14*8(sp)
    sd    x15, 15*8(sp)
    sd    x16, 16*8(sp)
    sd    x17, 17*8(sp)
    sd    x18, 18*8(sp)
    sd    x19, 19*8(sp)
    sd    x20, 20*8(sp)
    sd    x21, 21*8(sp)
    sd    x22, 22*8(sp)
    sd    x23, 23*8(sp)
    sd    x24, 24*8(sp)
    sd    x25, 25*8(sp)
    sd    x26, 26*8(sp)
    sd    x27, 27*8(sp)
    sd    x28, 28*8(sp)
    sd    x29, 29*8(sp)
    sd    x30, 30*8(sp)
    sd    x31, 31*8(sp)
    csrr  t0, sscratch            # user sp
    sd    t0,  2*8(sp)
    csrw  sscratch, sp            # keep sscratch = frame ptr for the next trap
    csrr  t0, sepc
    sd    t0, 256(sp)
    csrr  t0, sstatus
    sd    t0, 264(sp)
    mv    a0, sp                  # a0 = frame ptr (arg to the Rust handler)
    la    t0, KERNEL_CTX
    ld    sp, 1*8(t0)             # run the handler on the scheduler's kernel stack
    call  _user_trap_rust
    j     resume_return

# resume_return — restore kernel callee-saved from KERNEL_CTX and `ret`, so the Rust caller of
# resume_frame resumes exactly where it left off (its resume_frame call "returns").
.balign 4
.global resume_return
resume_return:
    la    t0, KERNEL_CTX
    ld    ra,  0*8(t0)
    ld    sp,  1*8(t0)
    ld    s0,  2*8(t0)
    ld    s1,  3*8(t0)
    ld    s2,  4*8(t0)
    ld    s3,  5*8(t0)
    ld    s4,  6*8(t0)
    ld    s5,  7*8(t0)
    ld    s6,  8*8(t0)
    ld    s7,  9*8(t0)
    ld    s8, 10*8(t0)
    ld    s9, 11*8(t0)
    ld    s10,12*8(t0)
    ld    s11,13*8(t0)
    ret

# User task stubs (position-independent: only relative branches, ecall, register ops). Copied byte-
# for-byte into a fresh U-code page and executed at USER_CODE_VA. Each is delimited by _s/_e labels
# so the Rust side knows its length.

.global _stub_emit_s
_stub_emit_s:
    mv   a0, s2
    li   a7, 1                    # SYS_EMIT
    ecall
1:  j    1b
.global _stub_emit_e
_stub_emit_e:

.global _stub_read_s
_stub_read_s:
    ld   a1, 0(a0)                # touch the address in a0 — faults if not U-accessible
1:  j    1b
.global _stub_read_e
_stub_read_e:

.global _stub_reademit_s
_stub_reademit_s:
    ld   a1, 0(a0)                # read own page (a0 = VA_P) — must NOT fault
    li   a7, 1                    # SYS_EMIT — reaching here proves the read succeeded
    ecall
1:  j    1b
.global _stub_reademit_e
_stub_reademit_e:

.global _stub_send_s
_stub_send_s:
    mv   a0, s2                   # message body = magic in s2
    li   a7, 4                    # SYS_SEND
    ecall
1:  j    1b
.global _stub_send_e
_stub_send_e:

.global _stub_recv_s
_stub_recv_s:
    li   a7, 5                    # SYS_RECV
    ecall
1:  j    1b
.global _stub_recv_e
_stub_recv_e:

.global _stub_sched_s
_stub_sched_s:
    mv   a0, s2
    li   a7, 2                    # SYS_YIELD (1)
    ecall
    mv   a0, s2
    li   a7, 2                    # SYS_YIELD (2)
    ecall
    mv   a0, s2
    li   a7, 2                    # SYS_YIELD (3)
    ecall
    mv   a0, s2
    li   a7, 3                    # SYS_EXIT
    ecall
1:  j    1b
.global _stub_sched_e
_stub_sched_e:

.global _stub_spin_s
_stub_spin_s:
1:  addi s2, s2, 1                # progress counter (proves state survives involuntary switch)
    addi s3, s3, -1               # bounded countdown — dead-timer escape
    bnez s3, 1b
    mv   a0, s2
    li   a7, 3                    # SYS_EXIT (only reached if the timer never fired)
    ecall
2:  j    2b
.global _stub_spin_e
_stub_spin_e:
"#
);

extern "C" {
    fn resume_frame(frame: *mut TrapFrame);
    fn _stub_emit_s();
    fn _stub_emit_e();
    fn _stub_read_s();
    fn _stub_read_e();
    fn _stub_reademit_s();
    fn _stub_reademit_e();
    fn _stub_send_s();
    fn _stub_send_e();
    fn _stub_recv_s();
    fn _stub_recv_e();
    fn _stub_sched_s();
    fn _stub_sched_e();
    fn _stub_spin_s();
    fn _stub_spin_e();
}

/// A stub's byte range in the kernel image `[start, end)`.
#[derive(Clone, Copy)]
struct Stub {
    start: usize,
    end: usize,
}
fn stub(start: unsafe extern "C" fn(), end: unsafe extern "C" fn()) -> Stub {
    Stub {
        start: start as usize,
        end: end as usize,
    }
}

// -------------------------------------------------------------------------------------------
// Trial state (single-shot capability trials) + scheduler / IPC statics.
// -------------------------------------------------------------------------------------------
struct Trial {
    engine: CapEngine,
    store: Store,
    caps: Vec<CapToken>,
    action: &'static str,
    armed: bool,   // true => a U-mode fault is EXPECTED (isolation test), not fatal
    allowed: bool, // outcome: was the syscall authorized
    isolation_held: bool, // outcome: did the armed fault actually occur
    fault_va: usize, // outcome: stval of the armed fault
}
static mut CURRENT: Option<Trial> = None;

fn current() -> Option<&'static mut Trial> {
    // SAFETY: single-core, no preemption of the kernel itself (SIE stays 0 in S-mode); the trap
    // handler and the scheduler never run concurrently.
    unsafe { (*addr_of_mut!(CURRENT)).as_mut() }
}

// Single-slot kernel-mediated IPC mailbox.
static mut ENDPOINT: Option<u64> = None;
static mut IPC_RECEIVED: u64 = 0;

// Scheduler signalling written by the trap handler, read by the run loops.
struct SchedState {
    last_magic: u64,
    exited: bool,
    preempted: bool,
}
static mut SCHED: SchedState = SchedState {
    last_magic: 0,
    exited: false,
    preempted: false,
};

// -------------------------------------------------------------------------------------------
// The Rust trap handler (called from _user_trap_entry) + the syscall / fault / timer logic.
// -------------------------------------------------------------------------------------------

/// Central trap dispatch. `frame` is the saved register file (in kernel RAM). Reads `scause`/`stval`
/// live; a trap that did not originate in U-mode, or any unexpected cause, is fatal (`exit 102`).
#[no_mangle]
extern "C" fn _user_trap_rust(frame: *mut TrapFrame) {
    let scause: u64;
    let stval: u64;
    let sstatus: u64;
    // SAFETY: reading trap CSRs is always sound inside the handler.
    unsafe {
        asm!("csrr {}, scause", out(reg) scause, options(nomem, nostack));
        asm!("csrr {}, stval", out(reg) stval, options(nomem, nostack));
        asm!("csrr {}, sstatus", out(reg) sstatus, options(nomem, nostack));
    }

    if scause >> 63 != 0 {
        // Interrupt. Supervisor timer = code 5.
        if scause & 0xff == 5 {
            timer_arm(); // re-arm (this is what clears the pending timer) BEFORE returning
                         // SAFETY: single-owner static; no concurrent access (see `current`).
            unsafe { (*addr_of_mut!(SCHED)).preempted = true };
        }
        return;
    }

    let from_user = sstatus & SSTATUS_SPP == 0;
    let code = scause & 0xff;
    if !from_user {
        kprintln!(
            "[usermode] FATAL S-mode trap scause={:#x} stval={:#x}",
            scause,
            stval
        );
        crate::exit::exit(102);
    }

    match code {
        8 => {
            // Environment call from U-mode. Advance past the `ecall`, dispatch, write result to a0.
            // SAFETY: `frame` points at the current task's saved register file.
            unsafe {
                (*frame).sepc = (*frame).sepc.wrapping_add(4);
                let num = (*frame).regs[17]; // a7
                let arg = (*frame).regs[10]; // a0
                let ret = el0_syscall(num, arg);
                (*frame).regs[10] = ret;
            }
        }
        12 | 13 | 15 => el0_page_fault(stval as usize), // instruction / load / store page fault
        _ => {
            let sepc = unsafe { (*frame).sepc };
            kprintln!(
                "[usermode] FATAL U-mode trap scause={:#x} stval={:#x} sepc={:#x}",
                scause,
                stval,
                sepc
            );
            crate::exit::exit(102);
        }
    }
}

/// The syscall handler — capability-gated through the SAME `CapEngine::evaluate` the deterministic
/// pipeline uses. EMIT/SEND/RECV authorize against the current trial's granted capabilities;
/// YIELD/EXIT report to the scheduler. Unknown numbers fail closed (`u64::MAX`).
fn el0_syscall(num: u64, arg: u64) -> u64 {
    match num {
        SYS_EMIT | SYS_SEND | SYS_RECV => {
            let t = match current() {
                Some(t) => t,
                None => return u64::MAX,
            };
            match t.engine.evaluate(t.action, &Target::default(), &t.caps) {
                Decision::Allow => {
                    t.allowed = true;
                    match num {
                        SYS_EMIT => {
                            t.store.record_event(t.action, "u-process");
                            0
                        }
                        SYS_SEND => {
                            // SAFETY: single-owner mailbox; no concurrency (see `current`).
                            unsafe { *addr_of_mut!(ENDPOINT) = Some(arg) };
                            0
                        }
                        SYS_RECV => {
                            // SAFETY: single-owner mailbox.
                            match unsafe { (*addr_of_mut!(ENDPOINT)).take() } {
                                Some(body) => {
                                    unsafe { *addr_of_mut!(IPC_RECEIVED) = body };
                                    body
                                }
                                None => u64::MAX, // authorized, but the endpoint was empty
                            }
                        }
                        _ => unreachable!(),
                    }
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
        _ => u64::MAX,
    }
}

fn sched_report(magic: u64, exited: bool) {
    // SAFETY: single-owner static; no concurrent access.
    unsafe {
        let s = &mut *addr_of_mut!(SCHED);
        s.last_magic = magic;
        s.exited = exited;
    }
}

/// U-mode page-fault handler. If the current trial armed for it (isolation test), record the fault
/// and resume the scheduler harmlessly. Any UNARMED fault is a real bug and is fatal.
fn el0_page_fault(fault_va: usize) {
    if let Some(t) = current() {
        if t.armed {
            t.isolation_held = true;
            t.fault_va = fault_va;
            t.armed = false;
            return;
        }
    }
    kprintln!(
        "[usermode] FATAL unarmed U-mode page fault stval={:#x}",
        fault_va
    );
    crate::exit::exit(102);
}

// --- Timer (SBI TIME extension + sie.STIE) --------------------------------------------------
fn timer_arm() {
    sbi::set_timer(arch::rdtime() + SLICE_TICKS);
}
fn timer_disable() {
    sbi::set_timer(u64::MAX); // push the deadline out of range
}
fn stie_enable() {
    // SAFETY: setting sie.STIE only affects S-mode timer-interrupt masking; sound at S-mode.
    unsafe { asm!("csrs sie, {}", in(reg) SIE_STIE, options(nomem, nostack)) };
}
fn stie_disable() {
    // SAFETY: clearing sie.STIE is sound at S-mode.
    unsafe { asm!("csrc sie, {}", in(reg) SIE_STIE, options(nomem, nostack)) };
}

/// Install `_user_trap_entry` in `stvec` (Direct mode) for the duration of the user-mode tests.
fn install_trap_vector() {
    extern "C" {
        fn _user_trap_entry();
    }
    // SAFETY: `_user_trap_entry` is 4-byte aligned; low two bits clear select Direct mode.
    unsafe {
        asm!("csrw stvec, {}", in(reg) _user_trap_entry as *const () as usize, options(nomem, nostack))
    };
}

// --- User-page setup ------------------------------------------------------------------------

/// Copy a stub into a fresh frame, `fence.i` so the write is fetchable, and map it U-executable.
fn map_user_code(root: usize, va: usize, s: Stub) -> Option<frames::PhysFrame> {
    let f = frames::alloc_zeroed()?;
    let len = s.end - s.start;
    // SAFETY: `s.start..s.end` is a stub in the kernel image (identity-accessible); `f` is a fresh
    // frame we own; both are within RAM. `fence.i` serializes the instruction stream after the write.
    unsafe {
        core::ptr::copy_nonoverlapping(s.start as *const u8, f.addr() as *mut u8, len);
        asm!("fence.i", options(nostack));
    }
    if !vm::map_page(root, va, f.addr(), vm::USER_CODE) {
        frames::free(f);
        return None;
    }
    Some(f)
}

/// Map a fresh zeroed frame as a U-mode data/stack page.
fn map_user_data(root: usize, va: usize) -> Option<frames::PhysFrame> {
    let f = frames::alloc_zeroed()?;
    if !vm::map_page(root, va, f.addr(), vm::USER_DATA) {
        frames::free(f);
        return None;
    }
    Some(f)
}

// --- Single-excursion primitive -------------------------------------------------------------

/// Run one U-mode excursion in the CURRENTLY active address space. Returns when the task traps.
fn run_one_shot(frame: &mut TrapFrame) {
    // SAFETY: `resume_frame` saves kernel callee-saved and `sret`s to U-mode; it returns (via
    // resume_return) once the task traps back. `frame` stays borrowed for the whole call.
    unsafe { resume_frame(frame as *mut TrapFrame) };
}

// --- Invariants 1-2: cap-gated syscall ------------------------------------------------------
fn run_syscall(grant: bool) -> (bool, usize) {
    let root_main = vm::active_root();
    let root = vm::build_identity().expect("proc root");
    map_user_code(root, USER_CODE_VA, stub(_stub_emit_s, _stub_emit_e)).expect("code");
    map_user_data(root, USER_STACK_VA).expect("stack");

    let mut engine = CapEngine::new(0xA5A5, 1000);
    let mut caps = Vec::new();
    if grant {
        caps.push(engine.mint("u-process", "event.emit", Scope::All, Constraints::none()));
    }
    // SAFETY: single-owner trial slot; the trap handler reads it during the excursion below.
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

    let mut f = make_frame(USER_CODE_VA, USER_STACK_TOP, 0, 0, 0);
    // SAFETY: `root` replicates the kernel identity map (build_identity), so switching to it and
    // back is safe; the excursion runs the stub then traps.
    unsafe {
        vm::switch_address_space(root);
        run_one_shot(&mut f);
        vm::switch_address_space(root_main);
    }
    let (allowed, events) = {
        let t = current().unwrap();
        (t.allowed, t.store.event_count())
    };
    unsafe { *addr_of_mut!(CURRENT) = None };
    (allowed, events)
}

// --- Invariant 3: U-mode read of kernel memory faults (isolation) ---------------------------
fn run_isolation() -> (bool, usize) {
    let root_main = vm::active_root();
    let root = vm::build_identity().expect("proc root");
    map_user_code(root, USER_CODE_VA, stub(_stub_read_s, _stub_read_e)).expect("code");
    map_user_data(root, USER_STACK_VA).expect("stack");

    // SAFETY: single-owner trial slot.
    unsafe {
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine: CapEngine::new(0xA5A5, 1000),
            store: Store::new(),
            caps: Vec::new(),
            action: "event.emit",
            armed: true, // a U-mode read of a supervisor-only page SHOULD fault
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
    }
    // KERNEL_CTX lives in kernel RAM — identity-mapped WITHOUT the U bit, so a U-mode load faults.
    let kaddr = addr_of!(KERNEL_CTX) as usize;
    let mut f = make_frame(USER_CODE_VA, USER_STACK_TOP, kaddr as u64, 0, 0);
    // SAFETY: see run_syscall.
    unsafe {
        vm::switch_address_space(root);
        run_one_shot(&mut f);
        vm::switch_address_space(root_main);
    }
    let (held, fva) = {
        let t = current().unwrap();
        (t.isolation_held, t.fault_va)
    };
    unsafe { *addr_of_mut!(CURRENT) = None };
    (held, fva)
}

// --- Invariants 4-5: per-process address spaces ---------------------------------------------
fn run_cross_process_isolation() -> (bool, bool, usize) {
    let root_main = vm::active_root();

    // Process A: its own space; maps a private data page at VA_P; reads it (must NOT fault) then emits.
    let ra = vm::build_identity().expect("A root");
    map_user_code(ra, USER_CODE_VA, stub(_stub_reademit_s, _stub_reademit_e)).expect("A code");
    map_user_data(ra, USER_STACK_VA).expect("A stack");
    map_user_data(ra, VA_P).expect("A private page");
    let mut ea = CapEngine::new(0xA5A5, 1000);
    let ca = alloc::vec![ea.mint("process-a", "event.emit", Scope::All, Constraints::none())];
    // SAFETY: single-owner trial slot.
    unsafe {
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine: ea,
            store: Store::new(),
            caps: ca,
            action: "event.emit",
            armed: false,
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
    }
    let mut fa = make_frame(USER_CODE_VA, USER_STACK_TOP, VA_P as u64, 0, 0);
    // SAFETY: see run_syscall.
    unsafe {
        vm::switch_address_space(ra);
        run_one_shot(&mut fa);
        vm::switch_address_space(root_main);
    }
    let a_reached = current().unwrap().allowed;
    unsafe { *addr_of_mut!(CURRENT) = None };

    // Process B: its own space; does NOT map VA_P; reads VA_P -> must fault (per-process isolation).
    let rb = vm::build_identity().expect("B root");
    map_user_code(rb, USER_CODE_VA, stub(_stub_read_s, _stub_read_e)).expect("B code");
    map_user_data(rb, USER_STACK_VA).expect("B stack");
    // SAFETY: single-owner trial slot.
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
    let mut fb = make_frame(USER_CODE_VA, USER_STACK_TOP, VA_P as u64, 0, 0);
    // SAFETY: see run_syscall.
    unsafe {
        vm::switch_address_space(rb);
        run_one_shot(&mut fb);
        vm::switch_address_space(root_main);
    }
    let (b_isolated, b_fault_va) = {
        let t = current().unwrap();
        (t.isolation_held, t.fault_va)
    };
    unsafe { *addr_of_mut!(CURRENT) = None };

    (a_reached, b_isolated, b_fault_va)
}

// --- Invariants 6-8: cooperative round-robin scheduler --------------------------------------
fn run_scheduler() -> (bool, bool, bool) {
    let root_main = vm::active_root();
    let magics = [0x111u64, 0x222u64];
    let mut roots = [0usize; NTASK];
    for r in roots.iter_mut() {
        let root = vm::build_identity().expect("sched root");
        map_user_code(root, USER_CODE_VA, stub(_stub_sched_s, _stub_sched_e)).expect("sched code");
        map_user_data(root, USER_STACK_VA).expect("sched stack");
        *r = root;
    }
    let mut tcb = [
        make_frame(USER_CODE_VA, USER_STACK_TOP, 0, magics[0], 0),
        make_frame(USER_CODE_VA, USER_STACK_TOP, 0, magics[1], 0),
    ];
    // Scheduling POLICY driven by the shared kernel_core::sched::RoundRobin (REQ-KERN-005): the
    // RISC-V target drives the SAME scheduler proved on the host and used by the aarch64 backend,
    // performing only the context-switch MECHANISM (run_one_shot + satp switch) behind the
    // TaskContext seam. `schedule_next` picks the task; a yielded task is rotated to the tail; an
    // exited task is `finish`ed. Reproduces the same A,B,A,B,A,B,A,B order (asserted below).
    let mut policy = RoundRobin::new();
    for i in 0..NTASK {
        policy.spawn(TaskId(i as u64));
    }
    let mut order: Vec<usize> = Vec::new();
    let mut magic_ok = true;
    let mut guard = 0usize;

    while let Some(TaskId(id)) = policy.schedule_next() {
        let i = id as usize;
        // Cooperative tasks report through SCHED, not CURRENT.
        unsafe {
            *addr_of_mut!(CURRENT) = None;
            let s = &mut *addr_of_mut!(SCHED);
            s.exited = false;
            s.last_magic = 0;
        }
        // SAFETY: each `roots[i]` replicates the kernel identity map; switching per slice is safe.
        unsafe {
            vm::switch_address_space(roots[i]);
            run_one_shot(&mut tcb[i]);
            vm::switch_address_space(root_main);
        }
        order.push(i);
        let (magic, exited) = unsafe {
            let s = &*addr_of!(SCHED);
            (s.last_magic, s.exited)
        };
        if magic != magics[i] {
            magic_ok = false;
        }
        if exited {
            policy.finish(TaskId(id));
        }
        guard += 1;
        if guard > 4 * NTASK {
            break; // safety bound: never spin on a scheduler bug
        }
    }

    let order_ok = order == [0, 1, 0, 1, 0, 1, 0, 1];
    let spaces_distinct = roots[0] != roots[1] && roots[0] != root_main && roots[1] != root_main;
    (order_ok, magic_ok, spaces_distinct)
}

// --- Invariants 9-10: timer preemption ------------------------------------------------------
fn run_preemptive() -> (bool, bool) {
    let root_main = vm::active_root();
    let mut roots = [0usize; NTASK];
    for r in roots.iter_mut() {
        let root = vm::build_identity().expect("preempt root");
        map_user_code(root, USER_CODE_VA, stub(_stub_spin_s, _stub_spin_e)).expect("spin code");
        map_user_data(root, USER_STACK_VA).expect("spin stack");
        *r = root;
    }
    let mut tcb = [
        make_frame(USER_CODE_VA, USER_STACK_TOP, 0, 0, SPIN_COUNTDOWN),
        make_frame(USER_CODE_VA, USER_STACK_TOP, 0, 0, SPIN_COUNTDOWN),
    ];
    let mut counts = [0u64; NTASK];
    let mut last_prog = [0u64; NTASK];
    let mut progress_ok = true;
    let mut clean = true;

    stie_enable();
    timer_arm();
    let mut cur = 0usize;
    for _ in 0..SLICES {
        unsafe {
            *addr_of_mut!(CURRENT) = None;
            let s = &mut *addr_of_mut!(SCHED);
            s.preempted = false;
            s.exited = false;
        }
        // SAFETY: roots replicate the kernel identity map; tasks run in U-mode where the delegated
        // S-timer interrupt fires regardless of sstatus.SIE.
        unsafe {
            vm::switch_address_space(roots[cur]);
            run_one_shot(&mut tcb[cur]);
            vm::switch_address_space(root_main);
        }
        let (was_preempt, was_exit) = unsafe {
            let s = &*addr_of!(SCHED);
            (s.preempted, s.exited)
        };
        if was_exit || !was_preempt {
            clean = false; // a dead timer would let the task self-exit -> fail fast, no hang
            break;
        }
        counts[cur] += 1;
        let prog = tcb[cur].regs[18]; // s2 progress counter
        if prog <= last_prog[cur] {
            progress_ok = false;
        }
        last_prog[cur] = prog;
        cur = 1 - cur;
    }
    timer_disable();
    stie_disable();

    let fair = clean && counts.iter().all(|&c| c > 0);
    (fair, progress_ok)
}

// --- Invariants 11-13: capability-secure kernel-mediated IPC --------------------------------
fn run_endpoint_excursion(action: &'static str, grant: bool, body: u64, code: Stub) -> bool {
    let root_main = vm::active_root();
    let root = vm::build_identity().expect("ipc root");
    map_user_code(root, USER_CODE_VA, code).expect("ipc code");
    map_user_data(root, USER_STACK_VA).expect("ipc stack");
    let mut engine = CapEngine::new(0xA5A5, 1000);
    let mut caps = Vec::new();
    if grant {
        caps.push(engine.mint("ipc-process", action, Scope::All, Constraints::none()));
    }
    // SAFETY: single-owner trial slot.
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
    // body -> a0 and s2 (SYS_SEND reads the body from s2 via the stub).
    let mut f = make_frame(USER_CODE_VA, USER_STACK_TOP, body, body, 0);
    // SAFETY: see run_syscall.
    unsafe {
        vm::switch_address_space(root);
        run_one_shot(&mut f);
        vm::switch_address_space(root_main);
    }
    let allowed = current().unwrap().allowed;
    unsafe { *addr_of_mut!(CURRENT) = None };
    allowed
}

fn run_ipc() -> (bool, bool, bool) {
    let body = 0xBEEF_u64;
    // 11 — authorized send (space 1) then authorized recv (space 2); message crosses spaces.
    unsafe {
        *addr_of_mut!(ENDPOINT) = None;
        *addr_of_mut!(IPC_RECEIVED) = 0;
    }
    let sent = run_endpoint_excursion("ipc.send", true, body, stub(_stub_send_s, _stub_send_e));
    let recvd = run_endpoint_excursion("ipc.recv", true, 0, stub(_stub_recv_s, _stub_recv_e));
    let delivered = sent && recvd && unsafe { *addr_of!(IPC_RECEIVED) } == body;

    // 12 — send WITHOUT the ipc.send capability is denied; the endpoint is untouched.
    unsafe { *addr_of_mut!(ENDPOINT) = None };
    let send_ok =
        run_endpoint_excursion("ipc.send", false, 0xDEAD, stub(_stub_send_s, _stub_send_e));
    let send_denied = !send_ok && unsafe { (*addr_of!(ENDPOINT)).is_none() };

    // 13 — recv WITHOUT the ipc.recv capability is denied; the queued message is intact.
    unsafe { *addr_of_mut!(ENDPOINT) = Some(0xCAFE) };
    let recv_ok = run_endpoint_excursion("ipc.recv", false, 0, stub(_stub_recv_s, _stub_recv_e));
    let recv_denied = !recv_ok && unsafe { *addr_of!(ENDPOINT) } == Some(0xCAFE);

    (delivered, send_denied, recv_denied)
}

/// Shared VA for the grant-table test — an unused slot in the same `0x5000_xxxx` hole as the other
/// user pages (below RAM at 0x8000_0000, so NOT identity-mapped: a real per-process translation).
const SHARED_VA: usize = 0x5000_5000;

/// Prove the zero-copy shared-memory grant-table (REQ-IPC-008) through the REAL RISC-V Sv39 MMU path,
/// exactly as the aarch64 backend does — the shared `GrantTable` is the arch-independent
/// authority/lifecycle layer; THIS target's `vm.rs` performs the actual page mapping. Proves, live:
///   * a `memory.share` grant maps ONE physical frame into TWO distinct process address spaces
///     (their own `satp` roots), so both resolve the SAME physical frame — zero-copy across AS;
///   * establishing the grant is capability-gated (no `memory.share` ⇒ no grant, nothing mapped);
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

    let mut engine = CapEngine::new(0x5EED, 1000);
    let share_cap = engine.mint("proc-a", "memory.share", Scope::All, Constraints::none());
    let mut gt = GrantTable::new("memory.share");
    let region = gt.create_region("proc-a", pa as u64, frames::FRAME_SIZE);

    // (cap_gated) Fail-closed without the capability; authorized with it.
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
    let granted = gt.share(
        &engine,
        region,
        "proc-a",
        "proc-b",
        ShareMode::ReadWrite,
        &[share_cap],
    );
    let cap_gated = denied && granted.is_ok();

    // Map the ONE frame into BOTH roots at the shared VA.
    let mapped = granted.is_ok()
        && vm::map_page(root_a, SHARED_VA, pa, vm::USER_DATA)
        && vm::map_page(root_b, SHARED_VA, pa, vm::USER_DATA);

    // (shared_across_spaces) Both distinct roots translate the shared VA to the SAME frame.
    let shared_across_spaces = mapped
        && root_a != root_b
        && vm::translate(root_a, SHARED_VA) == Some(pa)
        && vm::translate(root_b, SHARED_VA) == Some(pa);

    // (revoke_unmaps) Revocation PATH: consult the grant-table's revoke authority, and ONLY on
    // success tear down the grantee's mapping — the unmap is a consequence of a successful revoke,
    // not unconditional. The grantor keeps its own access.
    let grant_id = granted.unwrap_or(0);
    let revoke_unmaps = if gt.revoke(grant_id) {
        vm::unmap_page(root_b, SHARED_VA);
        vm::translate(root_b, SHARED_VA).is_none() && vm::translate(root_a, SHARED_VA) == Some(pa)
    } else {
        false
    };

    vm::unmap_page(root_a, SHARED_VA);
    frames::free(shf);

    (cap_gated, shared_across_spaces, revoke_unmaps)
}

// -------------------------------------------------------------------------------------------
// Selftest — 16 U-mode boundary invariants, riscv64-only. `Ok(n)` all passed; `Err((idx,name))`
// = failure (the caller exits the VM with 80+idx). An unexpected/unarmed trap is fatal (exit 102).
// -------------------------------------------------------------------------------------------
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    install_trap_vector();

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

    let (allowed0, ev0) = run_syscall(false);
    check!(
        !allowed0 && ev0 == 0,
        "u-mode: uncapable process — syscall denied at the boundary, zero effect"
    );
    let (allowed1, ev1) = run_syscall(true);
    check!(
        allowed1 && ev1 == 1,
        "u-mode: capable process — syscall authorized via the same CapEngine, one event recorded"
    );

    let (held, fva) = run_isolation();
    check!(
        held && fva == addr_of!(KERNEL_CTX) as usize,
        "u-mode: U-mode read of kernel memory faults — address-space isolation holds"
    );

    let (a_reached, b_isolated, b_fva) = run_cross_process_isolation();
    check!(
        a_reached,
        "u-mode: process A reaches a page in its own address space (mapped VA resolves)"
    );
    check!(
        b_isolated && b_fva == VA_P,
        "u-mode: process B cannot reach A's page at the same VA — per-process isolation holds"
    );

    let (order_ok, magic_ok, distinct) = run_scheduler();
    check!(
        order_ok,
        "u-mode: round-robin scheduler runs two tasks (each in its own space) A,B,A,B,... to completion"
    );
    check!(
        magic_ok,
        "u-mode: each task resumes with its own magic at the shared VA — full context + per-slice satp switch"
    );
    check!(
        distinct,
        "u-mode: the two scheduled tasks occupy distinct satp address spaces"
    );

    let (fair, prog) = run_preemptive();
    check!(
        fair,
        "u-mode: S-mode timer IRQ preempts two non-yielding tasks — scheduler round-robins both"
    );
    check!(
        prog,
        "u-mode: each task's register counter advances across timer preemptions — state preserved"
    );

    let (delivered, send_denied, recv_denied) = run_ipc();
    check!(
        delivered,
        "u-mode: capability-secure IPC — message delivered kernel-mediated across distinct address spaces"
    );
    check!(
        send_denied,
        "u-mode: IPC send without the ipc.send capability is denied — endpoint untouched (fail-closed)"
    );
    check!(
        recv_denied,
        "u-mode: IPC recv without the ipc.recv capability is denied — queued message intact (fail-closed)"
    );

    // 14, 15 & 16 — zero-copy shared memory (gap register Issue 2 / REQ-IPC-008): a memory.share
    // grant maps ONE physical frame into TWO distinct satp address spaces (zero-copy across AS),
    // establishing it is capability-gated (fail-closed), and revocation unmaps the grantee's page.
    let (cap_gated, shared_across_spaces, revoke_unmaps) = run_shared_memory();
    check!(
        cap_gated,
        "u-mode: shared-memory grant is capability-gated — no memory.share ⇒ no grant, nothing mapped (fail-closed)"
    );
    check!(
        shared_across_spaces,
        "u-mode: grant-table maps one frame into two distinct satp spaces — zero-copy shared memory across address spaces"
    );
    check!(
        revoke_unmaps,
        "u-mode: a successful grant revoke gates the unmap of the grantee's page; the grantor keeps access"
    );

    Ok(n)
}
