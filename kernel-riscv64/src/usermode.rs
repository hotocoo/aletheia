//! U-mode (unprivileged) user-mode brick — the RISC-V S/U privilege boundary made real, bringing
//! this first-class target to parity with the aarch64 EL0 suite (`kernel/src/usermode.rs`) and the
//! x86-64 ring-3 suite. Until now every invariant was re-proved *in S-mode kernel space*; this wave
//! drops the CPU to **U-mode** (unprivileged), runs a genuinely less-privileged instruction stream
//! in its own U-only pages, and lets it reach the OS through *exactly one door*: an `ecall` trap
//! that lands in the S-mode vector and is authorized by the **same `CapEngine`** the deterministic
//! pipeline uses. Contract-honest (ADR-010): written outside-in, boot-verified; an *unexpected*
//! trap stays fatal (`exit 102`) so a real bug can never masquerade as a pass.
//!
//! WHAT IT PROVES (29 invariants, identical in spirit to the other two targets):
//!   1-2  cap-gated `ecall` syscall — no capability ⇒ denied, zero effect; granted ⇒ one event.
//!   3    hardware isolation — a U-mode load of a supervisor-only (no-`U`) page faults, contained.
//!   4-5  per-process address spaces — A reaches its own page; B cannot reach A's VA (own `satp`).
//!   6-8  cooperative round-robin scheduler — two tasks in distinct spaces run A,B,A,B… to exit.
//!   9-10 timer preemption — the S-mode timer IRQ preempts two non-yielding tasks; state survives.
//!   11-13 capability-secure IPC — kernel-mediated message across spaces; send/recv fail-closed.
//!   14-16 zero-copy shared memory — a memory.share grant maps one frame into two satp spaces
//!         (REQ-IPC-008); cap-gated fail-closed; revocation unmaps the grantee (see run_shared_memory).
//!   17-19 blocking IPC (REQ-IPC-010) — recv on empty BLOCKS, send WAKES + delivers across spaces,
//!         the woken receiver resumes and reports the body (see run_blocking_ipc).
//!   20-22 priority inheritance (REQ-IPC-009) — a blocked HIGH donates to the LOW endpoint holder so
//!         the boosted LOW is dispatched over a Ready MEDIUM (see run_priority_ipc).
//!   28-29 process-info service — `process.inspect` denies without a capability and returns live
//!         supervisor counters when authorized.
//!   23-27 supervisor continuation — policy classification, undeclared-fault termination, teardown,
//!         non-runnable dead task, and a later U-mode task still running.
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
use kernel_core::frameown::Owner;
use kernel_core::sched::{RoundRobin, TaskId, TaskState};
use kernel_core::syscall::{
    pack_process_info, Syscall, SYS_BRK, SYS_CLOCK, SYS_EMIT, SYS_EXIT, SYS_FS_READ,
    SYS_POLL_INPUT, SYS_PRESENT, SYS_PROCESS_INFO, SYS_RECV, SYS_SEND, SYS_WRITE_CONSOLE,
    SYS_YIELD,
};
// REQ-IPC-008: the shared grant-table is the arch-independent authority/lifecycle layer over a
// shared-memory region; THIS target's Sv39 `vm.rs` performs the real page mapping into each space.
use kernel_core::grant::{GrantTable, ShareMode};
// REQ-IPC-009/010: shared priority-inheritance scheduler for the blocking-IPC dispatch decision.
use kernel_core::priosched::{Endpoint, Priority, PriorityScheduler};

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
/// Real timer interrupts taken since boot — the monotone clock the resident governor is driven by
/// on this target (ADR-167).
static TIMER_IRQS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Reported die temperature, in milli-degrees C. A STAND-IN, and named as one: QEMU 'virt' exposes
/// no thermal sensor to a guest, so the handler reports a fixed benign temperature rather than
/// inventing a curve. The power contract still owns what a trip would mean (ADR-056).
const THERMAL_STANDIN_MC: i32 = 40_000;

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

# Blocking-IPC receiver: recv (blocks on empty; the kernel delivers the body into a0 on wake), then
# EXIT carrying a0 as the arg — so the received body is reported back through sched_report.
.global _stub_recv_exit_s
_stub_recv_exit_s:
    li   a7, 5                    # SYS_RECV — a0 = received body (blocks if empty)
    ecall
    li   a7, 3                    # SYS_EXIT (a0 unchanged = received body)
    ecall
1:  j    1b
.global _stub_recv_exit_e
_stub_recv_exit_e:

# Read-only process-info syscall, then EXIT carrying returned a0.
.global _stub_process_info_exit_s
_stub_process_info_exit_s:
    li   a7, 7                    # SYS_PROCESS_INFO
    ecall
    li   a7, 3                    # SYS_EXIT (a0 unchanged = packed counters)
    ecall
1:  j    1b
.global _stub_process_info_exit_e
_stub_process_info_exit_e:

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
    fn _stub_recv_exit_s();
    fn _stub_recv_exit_e();
    fn _stub_process_info_exit_s();
    fn _stub_process_info_exit_e();
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
static mut PROCESS_INFO_RESULT: u64 = u64::MAX;

fn current() -> Option<&'static mut Trial> {
    // SAFETY: single-core, no preemption of the kernel itself (SIE stays 0 in S-mode); the trap
    // handler and the scheduler never run concurrently.
    unsafe { (*addr_of_mut!(CURRENT)).as_mut() }
}

// Single-slot kernel-mediated IPC mailbox.
static mut ENDPOINT: Option<u64> = None;
static mut IPC_RECEIVED: u64 = 0;
// Blocking IPC (REQ-IPC-010): when IPC_BLOCK_MODE is set (only during run_blocking_ipc/
// run_priority_ipc), an authorized SYS_RECV on an empty endpoint records that the caller must BLOCK
// (IPC_RECV_BLOCKED) instead of returning fail-value; the scheduler then deschedules it until a
// SYS_SEND wakes it. Default off ⇒ run_ipc's non-blocking mailbox semantics are untouched.
static mut IPC_BLOCK_MODE: bool = false;
static mut IPC_RECV_BLOCKED: bool = false;

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

            // ADR-167 — the resident governor stands its watch on this real S-mode timer interrupt.
            // A U-mode task was running when it fired, so the slice just closed was genuinely BUSY.
            // Both calls are no-ops until the watch is commissioned, and neither ever waits on the
            // watch lock — spinning for a lock held by the interrupted code would deadlock the core.
            let slice = TIMER_IRQS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
            kernel_core::lethed::resident::account(0, 1, 0);
            kernel_core::lethed::resident::on_timer_tick(slice, THERMAL_STANDIN_MC);
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
                let ret = if num == SYS_WRITE_CONSOLE {
                    write_console(arg, (*frame).regs[11]) // a0 = address, a1 = length
                } else if num == SYS_FS_READ {
                    // Admitted here, served by the run loop once the program is off the CPU
                    // (ADR-207): a0..a3 = name, name length, buffer, buffer length.
                    match kernel_core::progout::admit_read(
                        (*addr_of!(PROGRAM_GRANT)).as_ref(),
                        arg,
                        (*frame).regs[11],
                        (*frame).regs[12],
                        (*frame).regs[13],
                        *addr_of!(PROGRAM_WINDOW_NOW),
                    ) {
                        Some(req) => {
                            *addr_of_mut!(PENDING_READ) = Some(req);
                            0
                        }
                        None => u64::MAX,
                    }
                } else if num == SYS_CLOCK {
                    use crate::hal::{ActiveHal, Hal};
                    // The machine's monotonic clock (ADR-222), answered here: it reads a counter
                    // and touches nothing of the program's. The result goes back in a0 below.
                    ActiveHal::ticks_to_ns(ActiveHal::timer_ticks())
                } else if num == SYS_BRK {
                    // A query is answered here; growth is mapped by the run loop, which holds the
                    // program's frames (ADR-230).
                    match kernel_core::progout::admit_brk(*addr_of!(PROGRAM_WINDOW_NOW), arg) {
                        Some(kernel_core::progout::Brk::Now(top)) => top,
                        Some(kernel_core::progout::Brk::Grow(pages)) => {
                            *addr_of_mut!(PENDING_BRK) = Some(pages);
                            0
                        }
                        None => u64::MAX,
                    }
                } else if num == SYS_POLL_INPUT {
                    // Admitted here, answered by the run loop from the program's window (ADR-216).
                    if kernel_core::progout::admit_poll((*addr_of!(PROGRAM_GRANT)).as_ref()) {
                        *addr_of_mut!(PENDING_POLL) = true;
                        0
                    } else {
                        u64::MAX
                    }
                } else if num == SYS_PRESENT {
                    // Admitted here, served by the run loop once the program is off the CPU
                    // (ADR-215): a0..a2 = buffer, width, height.
                    match kernel_core::progout::admit_present(
                        (*addr_of!(PROGRAM_GRANT)).as_ref(),
                        arg,
                        (*frame).regs[11],
                        (*frame).regs[12],
                        *addr_of!(PROGRAM_WINDOW_NOW),
                    ) {
                        Some(req) => {
                            *addr_of_mut!(PENDING_PRESENT) = Some(req);
                            0
                        }
                        None => u64::MAX,
                    }
                } else {
                    el0_syscall(num, arg)
                };
                (*frame).regs[10] = ret;
            }
        }
        12 | 13 | 15 => el0_page_fault(stval as usize), // instruction / load / store page fault
        // Every other exception taken FROM U-mode was raised by the task's own instruction - an
        // illegal instruction, a breakpoint, a misaligned or faulting access - so it costs that task
        // and never the machine (ADR-202).
        _ => {
            let sepc = unsafe { (*frame).sepc };
            if !supervise_user_exception(scause, sepc) {
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
}

/// `SYS_WRITE_CONSOLE(addr, len)` for the running program (ADR-204). S-mode may read a U page only
/// with `sstatus.SUM` set, so it is set for exactly this service and cleared before returning.
fn write_console(addr: u64, len: u64) -> u64 {
    const SUM: usize = 1 << 18;
    #[allow(clippy::let_unit_value)]
    let () = PROGRAM_WINDOW;
    // SAFETY: setting SUM only lets this hart's S-mode loads reach U pages; cleared below.
    unsafe { asm!("csrs sstatus, {s}", s = in(reg) SUM, options(nomem, nostack)) };
    // SAFETY: single-threaded; the grant and sink belong to the program running now.
    let kept = unsafe {
        kernel_core::progout::serve_write(
            (*addr_of!(PROGRAM_GRANT)).as_ref(),
            &mut *addr_of_mut!(PROGRAM_OUT),
            addr,
            len,
            USER_CODE_VA as u64,
            (*addr_of!(PROGRAM_WINDOW_NOW)).end(),
            // SAFETY: the range lies inside the program's window, which IS its two mapped pages
            // (asserted at `PROGRAM_WINDOW`), and this trap runs under its satp.
            |r| core::slice::from_raw_parts(r.addr() as *const u8, r.len()),
        )
    };
    // SAFETY: clearing SUM restores the default: S-mode loads of U pages fault.
    unsafe { asm!("csrc sstatus, {s}", s = in(reg) SUM, options(nomem, nostack)) };
    kept
}

/// The syscall handler — capability-gated through the SAME `CapEngine::evaluate` the deterministic
/// pipeline uses. EMIT/SEND/RECV authorize against the current trial's granted capabilities;
/// YIELD/EXIT report to the scheduler. Unknown numbers fail closed (`u64::MAX`).
fn el0_syscall(num: u64, arg: u64) -> u64 {
    if Syscall::decode(num).is_none() {
        return u64::MAX;
    }
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
                                None => {
                                    // Empty. In blocking mode, signal the scheduler to deschedule
                                    // this caller until a SYS_SEND wakes it; else non-blocking MAX.
                                    if unsafe { IPC_BLOCK_MODE } {
                                        unsafe { *addr_of_mut!(IPC_RECV_BLOCKED) = true };
                                    }
                                    u64::MAX
                                }
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
        SYS_PROCESS_INFO => {
            let t = match current() {
                Some(t) => t,
                None => return u64::MAX,
            };
            match t.engine.evaluate(
                Syscall::ProcessInfo
                    .capability()
                    .unwrap_or("process.inspect"),
                &Target::default(),
                &t.caps,
            ) {
                Decision::Allow => {
                    t.allowed = true;
                    let response =
                        pack_process_info(supervisor().terminated(), supervisor().escalations());
                    unsafe { *addr_of_mut!(PROCESS_INFO_RESULT) = response };
                    response
                }
                _ => {
                    t.allowed = false;
                    u64::MAX
                }
            }
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

/// The task supervisor (REQ-REL-002, ADR-042). An UNEXPECTED user fault used to end the boot; now it
/// terminates that task and the system continues — the same policy `kernel-core` applies on every target.
static mut SUPERVISOR: kernel_core::supervisor::Supervisor =
    kernel_core::supervisor::Supervisor::new();
/// The id the supervisor knows the running excursion by (one runs at a time here). Left at 0 on this
/// target: no excursion here takes an UNDECLARED fault yet, so nothing has needed a distinct id. The
/// x86-64 backend bumps it per excursion because its suite deliberately kills one.
static mut CURRENT_TASK: u64 = 0;

/// Read-only view of the supervisor, for the boot invariants.
///
/// SAFETY: single-threaded, interrupts masked for the suite; no concurrent access exists.
pub fn supervisor() -> &'static kernel_core::supervisor::Supervisor {
    unsafe { &*core::ptr::addr_of!(SUPERVISOR) }
}

/// Ask the supervisor about an UNEXPECTED user fault. Returns true if the task was terminated and the
/// caller may abandon it and continue; false means escalate (the caller then exits).
/// A non-paging exception from U-mode, to the supervisor (ADR-202): built the way x86-64's `#UD` is.
/// Returns false only on escalation, which the caller makes fatal.
fn supervise_user_exception(scause: u64, sepc: u64) -> bool {
    use kernel_core::faultclass::{classify, kind_name, verdict, Fault};
    use kernel_core::sched::TaskId;
    use kernel_core::supervisor::SupervisorAction;
    let f = Fault {
        present: true,
        write: false,
        user: true,
        exec: true,
        reserved_bit: false,
        from_kernel: false,
        unrecognized: None,
    };
    let kind = classify(&f);
    // SAFETY: single-threaded; only this dispatcher mutates the supervisor.
    let (action, id) = unsafe {
        let id = TaskId(*core::ptr::addr_of!(CURRENT_TASK));
        (
            (*core::ptr::addr_of_mut!(SUPERVISOR)).on_fault(Some(id), kind, verdict(kind)),
            id,
        )
    };
    match action {
        SupervisorAction::TaskTerminated(reason) => {
            kprintln!(
                "[usermode] U-mode exception scause={:#x} at {:#x} -> {} : task {} TERMINATED ({:?}); system continues",
                scause,
                sepc,
                kind_name(kind),
                id.0,
                reason
            );
            true
        }
        SupervisorAction::Escalate(_) => false,
    }
}

fn supervise_user_fault(fault_va: usize) -> bool {
    use kernel_core::faultclass::{classify, kind_name, verdict, Fault};
    use kernel_core::sched::TaskId;
    use kernel_core::supervisor::SupervisorAction;
    // This path is reached only from a user-privilege abort, so the fault is a user fault by
    // construction; the classifier says what KIND, which is what the log needs and the policy consumes.
    let f = Fault {
        present: false,
        write: false,
        user: true,
        exec: false,
        reserved_bit: false,
        from_kernel: false,
        unrecognized: None,
    };
    let kind = classify(&f);
    // SAFETY: single-threaded; only this dispatcher mutates the supervisor.
    let (action, id) = unsafe {
        let id = TaskId(*core::ptr::addr_of!(CURRENT_TASK));
        (
            (*core::ptr::addr_of_mut!(SUPERVISOR)).on_fault(Some(id), kind, verdict(kind)),
            id,
        )
    };
    match action {
        SupervisorAction::TaskTerminated(reason) => {
            kprintln!(
                "[usermode] user fault at {:#x} -> {} : task {} TERMINATED ({:?}); system continues",
                fault_va,
                kind_name(kind),
                id.0,
                reason
            );
            true
        }
        SupervisorAction::Escalate(k) => {
            kprintln!(
                "[usermode] user fault at {:#x} ESCALATED ({})",
                fault_va,
                kind_name(k)
            );
            false
        }
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
    // Unarmed U-mode fault: to the supervisor (REQ-REL-002, ADR-042). Returning abandons the task and
    // resumes the scheduler, exactly as the armed path does.
    if supervise_user_fault(fault_va) {
        return;
    }
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
    let f = frames::alloc_zeroed_as(Owner::USER)?;
    let len = s.end - s.start;
    // SAFETY: `s.start..s.end` is a stub in the kernel image (identity-accessible); `f` is a fresh
    // frame we own; both are within RAM. `fence.i` serializes the instruction stream after the write.
    unsafe {
        core::ptr::copy_nonoverlapping(s.start as *const u8, f.addr() as *mut u8, len);
        asm!("fence.i", options(nostack));
    }
    if !vm::map_page(root, va, f.addr(), vm::USER_CODE) {
        frames::free_as(f, Owner::USER);
        return None;
    }
    Some(f)
}

/// Map a fresh zeroed frame as a U-mode data/stack page.
fn map_user_data(root: usize, va: usize) -> Option<frames::PhysFrame> {
    let f = frames::alloc_zeroed_as(Owner::USER)?;
    if !vm::map_page(root, va, f.addr(), vm::USER_DATA) {
        frames::free_as(f, Owner::USER);
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

/// Kill one genuinely faulting U-mode task, reclaim its private pages and address space, then
/// return its supervisor id. Proves fault containment plus teardown on RISC-V, not only policy.
fn run_unexpected_fault() -> u64 {
    let root_main = vm::active_root();
    let root = match vm::build_identity() {
        Some(root) => root,
        None => return 0,
    };
    let code = match map_user_code(root, USER_CODE_VA, stub(_stub_read_s, _stub_read_e)) {
        Some(frame) => frame,
        None => {
            vm::destroy_space(root);
            return 0;
        }
    };
    let stack = match map_user_data(root, USER_STACK_VA) {
        Some(frame) => frame,
        None => {
            vm::unmap_page(root, USER_CODE_VA);
            frames::free_as(code, Owner::USER);
            vm::destroy_space(root);
            return 0;
        }
    };
    unsafe { *addr_of_mut!(CURRENT_TASK) += 1 };
    let id = unsafe { *addr_of!(CURRENT_TASK) };
    let engine = CapEngine::new(0xBADC_0002, 1000);
    unsafe {
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine,
            store: Store::new(),
            caps: Vec::new(),
            action: "event.emit",
            armed: false,
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
    }
    let mut frame = make_frame(
        USER_CODE_VA,
        USER_STACK_TOP,
        addr_of!(KERNEL_CTX) as u64,
        0,
        0,
    );
    unsafe {
        vm::switch_address_space(root);
        run_one_shot(&mut frame);
        vm::switch_address_space(root_main);
    }
    let _trial = unsafe { (*addr_of_mut!(CURRENT)).take() }.expect("fault trial present");
    vm::unmap_page(root, USER_STACK_VA);
    frames::free_as(stack, Owner::USER);
    vm::unmap_page(root, USER_CODE_VA);
    frames::free_as(code, Owner::USER);
    vm::destroy_space(root);
    id
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

/// Run one read-only process-info syscall and carry response through `SYS_EXIT`.
fn run_process_info(grant: bool) -> (u64, bool) {
    let root_main = vm::active_root();
    let root = vm::build_identity().expect("process-info root");
    map_user_code(
        root,
        USER_CODE_VA,
        stub(_stub_process_info_exit_s, _stub_process_info_exit_e),
    )
    .expect("process-info code");
    map_user_data(root, USER_STACK_VA).expect("process-info stack");

    let mut engine = CapEngine::new(0xA5A5, 1000);
    let mut caps = Vec::new();
    if grant {
        caps.push(engine.mint(
            "u-process",
            "process.inspect",
            Scope::All,
            Constraints::none(),
        ));
    }
    unsafe {
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine,
            store: Store::new(),
            caps,
            action: "process.inspect",
            armed: false,
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
        *addr_of_mut!(PROCESS_INFO_RESULT) = u64::MAX;
        (*addr_of_mut!(SCHED)).last_magic = 0;
        (*addr_of_mut!(SCHED)).exited = false;
    }
    let mut f = make_frame(USER_CODE_VA, USER_STACK_TOP, 0, 0, 0);
    unsafe {
        vm::switch_address_space(root);
        run_one_shot(&mut f);
        vm::switch_address_space(root_main);
    }
    let result = unsafe { *addr_of!(PROCESS_INFO_RESULT) };
    let allowed = unsafe { (*addr_of_mut!(CURRENT)).take() }
        .expect("process-info trial present")
        .allowed;
    vm::unmap_page(root, USER_STACK_VA);
    vm::unmap_page(root, USER_CODE_VA);
    (result, allowed)
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

/// The console's `tasks` (ADR-199): the same advised run the boot suite proves, started at the
/// operator's word, so the resident advisor is consulted during the machine's life and not only
/// during its boot.
///
/// Interrupts stay off for the whole run, as they are during the boot suite. `sstatus.SIE` alone is
/// not enough: an S-mode interrupt is taken while a U-mode task runs whatever SIE says, so every
/// source in `sie` is cleared for the run and restored afterwards; the desktop's pump, which runs
/// off the timer, then sees one late tick instead of running while a task's address space is live.
///
/// The console runs on the kernel's own trap vector (`shellio` re-installs it), which assumes the
/// trap came from S-mode. A task's `ecall` taken there is stored through the TASK's stack pointer,
/// so the user-mode vector is installed for exactly the run and the console's put back after it.
pub fn run_tasks_live() -> kernel_core::shell::TaskRun {
    let (all_exited, own_magic, advised) = fenced(run_advised_scheduler);
    kernel_core::shell::TaskRun {
        tasks: NTASK,
        all_exited,
        own_magic,
        advised,
    }
}

/// The live-run fence of [`run_tasks_live`], shared with `run` (ADR-199, ADR-201).
fn fenced<R>(f: impl FnOnce() -> R) -> R {
    let were_enabled = crate::heap::irq_save();
    let sources: usize;
    // SAFETY: swapping `sie` with zero only masks this hart's interrupt sources; restored below.
    unsafe { asm!("csrrw {s}, sie, zero", s = out(reg) sources, options(nomem, nostack)) };
    install_trap_vector();
    let r = f();
    crate::trap::init();
    // SAFETY: restores exactly the sources that were enabled before the run.
    unsafe { asm!("csrw sie, {s}", s = in(reg) sources, options(nomem, nostack)) };
    crate::heap::irq_restore(were_enabled);
    r
}

/// The window a program's `SYS_WRITE_CONSOLE` range is checked against is exactly its code page and
/// its stack page, contiguous: the check is sufficient only because the window IS the mapping.
const PROGRAM_WINDOW: () =
    assert!(USER_STACK_VA == USER_CODE_VA + 0x1000 && USER_STACK_TOP == USER_CODE_VA + 0x2000);
/// A console-started program's own layout (ADR-211), independent of the boot suites' stubs: up to
/// [`PROGRAM_CODE_PAGES`] pages of code, then one stack page, then up to [`PROGRAM_DATA_PAGES`]
/// writable data pages (ADR-214), all
/// contiguous so one range check still covers a program's whole address space.
pub const PROGRAM_CODE_PAGES: u64 = 16;
const PROGRAM_STACK_VA: usize = USER_CODE_VA + PROGRAM_CODE_PAGES as usize * frames::FRAME_SIZE;
const PROGRAM_STACK_TOP: usize = PROGRAM_STACK_VA + frames::FRAME_SIZE;
const PROGRAM_DATA_VA: usize = PROGRAM_STACK_TOP;
/// Pages of writable data a program may declare above its stack (ADR-214).
pub const PROGRAM_DATA_PAGES: u64 = 16;

/// The running program's `console.output` grant (ADR-204); `None` whenever no `run` is in progress.
static mut PROGRAM_GRANT: Option<kernel_core::progout::Grant> = None;
/// The running program's pages (ADR-210): its data page exists only if it declared one.
static mut PROGRAM_WINDOW_NOW: kernel_core::progout::Window = kernel_core::progout::Window {
    code_va: USER_CODE_VA as u64,
    stack_va: PROGRAM_STACK_VA as u64,
    stack_top: PROGRAM_STACK_TOP as u64,
    data_top: PROGRAM_STACK_TOP as u64,
};
/// A `SYS_POLL_INPUT` the handler admitted, for the run loop to answer (ADR-216).
static mut PENDING_POLL: bool = false;
/// Pages a `SYS_BRK` the handler admitted asks for, for the run loop to map (ADR-230).
static mut PENDING_BRK: Option<usize> = None;
/// A `SYS_PRESENT` the handler admitted, for the run loop to serve (ADR-215).
static mut PENDING_PRESENT: Option<kernel_core::progout::PresentRequest> = None;
/// A `SYS_FS_READ` the handler admitted, for the run loop to serve (ADR-207).
static mut PENDING_READ: Option<kernel_core::progout::ReadRequest> = None;
/// What the running program has written (ADR-204).
static mut PROGRAM_OUT: kernel_core::progout::OutputSink = kernel_core::progout::OutputSink::new();

/// `hello`, built from Rust source in `userland/` for this CPU and checked in (ADR-205);
/// `scripts/check-userland.sh` requires it to be exactly what the source builds.
pub const USERLAND_HELLO: &[u8] = include_bytes!("../../userland/bin/riscv64/hello.elf");
/// `show`, from `userland/` (ADR-207): seeded, prints an object from the namespace.
pub const USERLAND_SHOW: &[u8] = include_bytes!("../../userland/bin/riscv64/show.elf");
/// `probe`, from `userland/` (ADR-207): the boot suite's probe of `SYS_FS_READ`; never seeded.
const USERLAND_PROBE: &[u8] = include_bytes!("../../userland/bin/riscv64/probe.elf");
/// `counter`, from `userland/` (ADR-210): seeded, a program with real mutable globals.
pub const USERLAND_COUNTER: &[u8] = include_bytes!("../../userland/bin/riscv64/counter.elf");
/// `big`, from `userland/` (ADR-211): three pages of code, so it runs only if the machine maps
/// every page the segment declares. Not seeded; the boot suite's.
const USERLAND_BIG: &[u8] = include_bytes!("../../userland/bin/riscv64/big.elf");
/// `wide`, from `userland/` (ADR-214): three pages of `.bss`, so it runs only if the machine maps,
/// zeroes and serves reads into every data page it declares. Not seeded; the boot suite's.
const USERLAND_WIDE: &[u8] = include_bytes!("../../userland/bin/riscv64/wide.elf");
/// `draw`, from `userland/` (ADR-215): seeded, a program with its own desktop window.
pub const USERLAND_DRAW: &[u8] = include_bytes!("../../userland/bin/riscv64/draw.elf");
/// `snake`, from `userland/` (ADR-222): seeded, a game in its own colour window.
pub const USERLAND_SNAKE: &[u8] = include_bytes!("../../userland/bin/riscv64/snake.elf");

/// The run storms (ADR-202's faulting runs, ADR-204's writes, ADR-207's reads) prove bounds under
/// load; like `kmain`'s storms (ADR-163) they run in the gate image only, so an interactive boot
/// pays for the contracts and not for them.
const STORMS: bool = !cfg!(feature = "interactive");

/// Timer slices a console-started program gets before it is abandoned (ADR-201, ADR-203).
const PROGRAM_SLICES: u32 = 64;
/// Returns to the kernel a program gets in all, syscalls included (ADR-204): what abandons a
/// program that traps forever without ever being preempted.
const PROGRAM_DISPATCHES: u32 = 65_536;
/// The boot suite's spinner budget: the same proof (preempted every slice, abandoned at the
/// budget) without spending 64 real slices of every boot on it.
const BOOT_SPIN_SLICES: u32 = 4;

/// Where this target places a program: U-mode code at [`USER_CODE_VA`] (ADR-201).
pub const PROGRAM_TARGET: kernel_core::elf::Target = kernel_core::elf::Target {
    machine: kernel_core::elf::Machine::Riscv64,
    code_va: USER_CODE_VA as u64,
    code_pages: PROGRAM_CODE_PAGES,
    data_va: PROGRAM_DATA_VA as u64,
    data_pages: PROGRAM_DATA_PAGES,
};

/// The console's `run NAME` and `together` (ADR-201, ADR-212): each program one U-mode task in its
/// own address space, admitted through the resident advisor and dispatched by the priority
/// scheduler, under [`fenced`]. Every frame they take is given back.
pub fn run_programs_live(
    set: &[(&kernel_core::elf::Placement, &[u8])],
    services: &mut dyn kernel_core::progout::ProgramServices,
) -> Option<Vec<kernel_core::shell::ProgramRun>> {
    fenced(|| run_programs(set, services, true, PROGRAM_SLICES))
}

fn run_program(
    program: &kernel_core::elf::Placement,
    args: &[u8],
    services: &mut dyn kernel_core::progout::ProgramServices,
    live: bool,
    budget: u32,
) -> Option<kernel_core::shell::ProgramRun> {
    run_programs(&[(program, args)], services, live, budget).and_then(|mut runs| runs.pop())
}

/// One program of a set run together (ADR-212): the pages and address space it owns while the set
/// runs, and its share of the "program on the CPU" state (`PROGRAM_GRANT`, `PROGRAM_OUT`,
/// `PROGRAM_WINDOW_NOW`), swapped in for each of its slices and back out after, so the trap handler
/// serves whichever program is running without knowing there are others.
struct Slot {
    /// Where its code pages start, so they can be unmapped.
    vaddr: usize,
    root: usize,
    code: Vec<frames::PhysFrame>,
    stack: Option<frames::PhysFrame>,
    /// Its writable pages, from [`PROGRAM_DATA_VA`] up (ADR-210, ADR-214).
    data: Vec<frames::PhysFrame>,
    frame: TrapFrame,
    /// The supervisor's id for this program, so a fault is charged to it (ADR-202).
    task: u64,
    grant: Option<kernel_core::progout::Grant>,
    window: kernel_core::progout::Window,
    out: kernel_core::progout::OutputSink,
    run: kernel_core::shell::ProgramRun,
}

impl Slot {
    /// Exchange this slot's state with the "program on the CPU" statics: once before its slice
    /// (the handler then serves it) and once after (the statics hold what they held before).
    ///
    /// SAFETY: single-threaded, interrupts masked by the caller's fence; nothing else touches
    /// these statics meanwhile.
    unsafe fn swap_onto_cpu(&mut self) {
        core::ptr::swap(&mut self.grant, addr_of_mut!(PROGRAM_GRANT));
        core::ptr::swap(&mut self.out, addr_of_mut!(PROGRAM_OUT));
        core::ptr::swap(&mut self.window, addr_of_mut!(PROGRAM_WINDOW_NOW));
    }

    /// The program's report, its output included, with every page it held given back.
    fn finish(mut self) -> kernel_core::shell::ProgramRun {
        // A window it drew into goes with it (ADR-215).
        #[cfg(feature = "interactive")]
        crate::desktop::close_app(self.task);
        let mut run = core::mem::take(&mut self.run);
        run.output = self.out.bytes().to_vec();
        run.dropped = self.out.dropped();
        self.free();
        run
    }

    fn free(self) {
        free_program_pages(self.root, self.vaddr, self.code, self.stack, self.data);
    }
}

/// Give back every page a program mapped, and its address space.
fn free_program_pages(
    root: usize,
    vaddr: usize,
    code: Vec<frames::PhysFrame>,
    stack: Option<frames::PhysFrame>,
    data: Vec<frames::PhysFrame>,
) {
    for (i, f) in data.into_iter().enumerate() {
        vm::unmap_page(root, PROGRAM_DATA_VA + i * frames::FRAME_SIZE);
        frames::free_as(f, Owner::USER);
    }
    if let Some(f) = stack {
        vm::unmap_page(root, PROGRAM_STACK_VA);
        frames::free_as(f, Owner::USER);
    }
    for (i, f) in code.into_iter().enumerate() {
        vm::unmap_page(root, vaddr + i * frames::FRAME_SIZE);
        frames::free_as(f, Owner::USER);
    }
    vm::destroy_space(root);
}

/// Give a program its address space: code pages (ADR-211), stack page with its arguments at the
/// top (ADR-206), data page if declared (ADR-210), and an entry frame. `None` = it cannot be
/// placed; whatever was taken is given back.
fn place_program(program: &kernel_core::elf::Placement, args: &[u8]) -> Option<Slot> {
    if args.len() > kernel_core::elf::MAX_ARGS {
        return None;
    }
    let root = vm::build_identity()?;
    // The judged bytes live in kernel RAM, identity-accessible, exactly as a linked stub does.
    // As many code pages as the segment declares (ADR-211), each mapped read+execute.
    let mut code: Vec<frames::PhysFrame> = Vec::new();
    for (i, chunk) in program.code.chunks(frames::FRAME_SIZE).enumerate() {
        let part = Stub {
            start: chunk.as_ptr() as usize,
            end: chunk.as_ptr() as usize + chunk.len(),
        };
        match map_user_code(root, program.vaddr as usize + i * frames::FRAME_SIZE, part) {
            Some(f) => code.push(f),
            None => break,
        }
    }
    let code_ok = code.len() == program.code.len().div_ceil(frames::FRAME_SIZE).max(1);
    let stack = map_user_data(root, PROGRAM_STACK_VA);
    // The writable pages the program declared (ADR-210, ADR-214), contiguous above the stack,
    // zero past the bytes the image carries, so `.bss` starts zeroed.
    let pages = (program.data_memsz as usize).div_ceil(frames::FRAME_SIZE);
    let mut data: Vec<frames::PhysFrame> = Vec::with_capacity(pages);
    for i in 0..pages {
        let Some(f) = map_user_data(root, PROGRAM_DATA_VA + i * frames::FRAME_SIZE) else {
            break;
        };
        let bytes = program
            .data
            .get(i * frames::FRAME_SIZE..)
            .map_or(&[][..], |rest| &rest[..rest.len().min(frames::FRAME_SIZE)]);
        // SAFETY: `f` is the zeroed page just mapped, identity-mapped and kernel-writable; `bytes`
        // is at most one page of the image's data.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), f.addr() as *mut u8, bytes.len()) };
        data.push(f);
    }
    let data_ok = data.len() == pages;
    let Some(st) = stack.filter(|_| code_ok && data_ok) else {
        free_program_pages(root, program.vaddr as usize, code, stack, data);
        return None;
    };
    // The arguments sit at the top of the stack page, 16-aligned, and the stack starts below them
    // (ADR-206); the program is entered with their address and length in a0 and a1.
    let args_at = (args.len() + 15) & !15;
    let args_va = PROGRAM_STACK_TOP - args_at;
    // SAFETY: `st` is the zeroed stack frame just allocated, identity-mapped and kernel-writable;
    // the copy stays inside its last `args_at` bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(
            args.as_ptr(),
            (st.addr() + frames::FRAME_SIZE - args_at) as *mut u8,
            args.len(),
        )
    };
    let mut frame = make_frame(program.entry as usize, args_va, args_va as u64, 0, 0);
    frame.regs[11] = args.len() as u64; // a1
                                        // A fresh supervisor id per program, so a fault is charged to THIS program and its record can
                                        // be reaped (ADR-202). SAFETY: single-threaded; the run owns this counter while it runs.
    let task = unsafe {
        *addr_of_mut!(CURRENT_TASK) += 1;
        *addr_of!(CURRENT_TASK)
    };
    let data_top = (PROGRAM_DATA_VA + data.len() * frames::FRAME_SIZE) as u64;
    Some(Slot {
        vaddr: program.vaddr as usize,
        root,
        code,
        stack,
        data,
        frame,
        task,
        grant: Some(kernel_core::progout::Grant::new(task ^ 0xA11E_7A0A)),
        window: kernel_core::progout::Window {
            code_va: USER_CODE_VA as u64,
            stack_va: PROGRAM_STACK_VA as u64,
            stack_top: PROGRAM_STACK_TOP as u64,
            data_top,
        },
        out: kernel_core::progout::OutputSink::new(),
        run: kernel_core::shell::ProgramRun::default(),
    })
}

/// Give one program one turn on the CPU (ADR-212, ADR-213): its state onto the CPU, its address
/// space, resume until a timer slice or a syscall ends the turn, then back. Counts the slice,
/// serves a read it asked for, and says how it ended - `None` while it is still running.
fn run_slice(
    slot: &mut Slot,
    root_main: usize,
    services: &mut dyn kernel_core::progout::ProgramServices,
) -> Option<kernel_core::taskfeat::Outcome> {
    use kernel_core::mlsched::resident;
    use kernel_core::taskfeat::Outcome;

    let dead_before = supervisor().terminated();
    // SAFETY: single-threaded scheduler state, reset before the slice as in
    // `run_advised_scheduler`; the slot's state goes onto the CPU for exactly this slice.
    // `CURRENT_TASK` is the supervisor's high-water id between turns (every id is handed out by
    // bumping it), so it is put back after the turn and the next id handed out is never one a live
    // program holds.
    let high = unsafe {
        *addr_of_mut!(CURRENT) = None;
        let s = &mut *addr_of_mut!(SCHED);
        s.exited = false;
        s.last_magic = 0;
        s.preempted = false;
        let high = *addr_of!(CURRENT_TASK);
        *addr_of_mut!(CURRENT_TASK) = slot.task;
        slot.swap_onto_cpu();
        // The slot's root replicates the kernel identity map; switch for the slice.
        vm::switch_address_space(slot.root);
        run_one_shot(&mut slot.frame);
        vm::switch_address_space(root_main);
        slot.swap_onto_cpu();
        high
    };
    resident::observe_schedule();
    slot.run.slices += 1;
    // SAFETY: single-threaded; the slice is over.
    let (preempted, status, exited) = unsafe {
        *addr_of_mut!(CURRENT_TASK) = high;
        let s = &*addr_of!(SCHED);
        (s.preempted, s.last_magic, s.exited)
    };
    if preempted {
        slot.run.preempted += 1;
    }
    if supervisor().terminated() != dead_before {
        // The supervisor terminated it for a fault: the slice was its last (ADR-202).
        // SAFETY: single-threaded; the run owns this id's record and gives it back here, and a
        // terminated program's request is never served.
        let reason = unsafe {
            *addr_of_mut!(PENDING_READ) = None;
            *addr_of_mut!(PENDING_PRESENT) = None;
            *addr_of_mut!(PENDING_POLL) = false;
            *addr_of_mut!(PENDING_BRK) = None;
            (*addr_of_mut!(SUPERVISOR)).reap(TaskId(slot.task))
        };
        slot.run.terminated = Some(reason.map_or("fault", |r| r.name()));
        return Some(Outcome::Failed);
    }
    if exited {
        slot.run.exited = true;
        slot.run.status = status;
        return Some(Outcome::Finished);
    }
    // A read the handler admitted is served now, through THIS program's frames, and its result
    // lands in its saved a0 (ADR-207).
    // SAFETY: single-threaded; the program is off the CPU.
    if let Some(req) = unsafe { (*addr_of_mut!(PENDING_READ)).take() } {
        if let (Some(c), Some(st)) = (slot.code.first().copied(), slot.stack) {
            // SAFETY: `c`, `st` and the data frames are this program's own identity-mapped frames,
            // one page each and distinct, untouched while it is off the CPU.
            let mut views: [&mut [u8]; kernel_core::progout::DATA_CEILING_PAGES] =
                core::array::from_fn(|i| match slot.data.get(i) {
                    Some(d) => unsafe {
                        core::slice::from_raw_parts_mut(d.addr() as *mut u8, frames::FRAME_SIZE)
                    },
                    None => &mut [],
                });
            // SAFETY: as above.
            slot.frame.regs[10] = unsafe {
                kernel_core::progout::serve_read(
                    &req,
                    services,
                    core::slice::from_raw_parts(c.addr() as *const u8, frames::FRAME_SIZE),
                    core::slice::from_raw_parts_mut(st.addr() as *mut u8, frames::FRAME_SIZE),
                    &mut views[..slot.data.len()],
                )
            };
        }
    }
    // A frame the handler admitted is gathered from THIS program's data frames into its window,
    // and the verdict lands in its saved a0 (ADR-215).
    // SAFETY: single-threaded; the program is off the CPU.
    if let Some(req) = unsafe { (*addr_of_mut!(PENDING_PRESENT)).take() } {
        slot.frame.regs[10] = if present_frame(slot, &req) {
            0
        } else {
            u64::MAX
        };
    }
    // An input poll is answered from the program's own window (ADR-216).
    // SAFETY: single-threaded; the program is off the CPU.
    if unsafe { core::mem::take(&mut *addr_of_mut!(PENDING_POLL)) } {
        #[cfg(feature = "interactive")]
        let event = crate::desktop::poll_app(slot.task);
        #[cfg(not(feature = "interactive"))]
        let event = u64::MAX;
        slot.frame.regs[10] = event;
    }
    // Growth is mapped into THIS program's space, zeroed, above what it holds (ADR-230).
    // SAFETY: single-threaded; the program is off the CPU.
    if let Some(pages) = unsafe { (*addr_of_mut!(PENDING_BRK)).take() } {
        slot.frame.regs[10] = grow_data(slot, pages);
    }
    None
}

/// Map `pages` zeroed writable pages above a program's data and widen its window to them (ADR-230):
/// the new top, or `u64::MAX` with nothing kept when the frames run out.
fn grow_data(slot: &mut Slot, pages: usize) -> u64 {
    let held = slot.data.len();
    for i in held..held + pages {
        match map_user_data(slot.root, PROGRAM_DATA_VA + i * frames::FRAME_SIZE) {
            Some(f) => slot.data.push(f),
            None => {
                for (j, f) in slot.data.drain(held..).enumerate() {
                    vm::unmap_page(slot.root, PROGRAM_DATA_VA + (held + j) * frames::FRAME_SIZE);
                    frames::free_as(f, Owner::USER);
                }
                return u64::MAX;
            }
        }
    }
    slot.window.data_top = (PROGRAM_DATA_VA + slot.data.len() * frames::FRAME_SIZE) as u64;
    slot.window.data_top
}

/// Show an admitted frame in the program's desktop window (ADR-215). `false` when there is no live
/// desktop, another program holds the window, or the operator closed it.
fn present_frame(slot: &Slot, req: &kernel_core::progout::PresentRequest) -> bool {
    // SAFETY: the data frames are this program's own identity-mapped frames, one page each,
    // untouched while it is off the CPU.
    let views: [&[u8]; kernel_core::progout::DATA_CEILING_PAGES] =
        core::array::from_fn(|i| match slot.data.get(i) {
            Some(d) => unsafe {
                core::slice::from_raw_parts(d.addr() as *const u8, frames::FRAME_SIZE)
            },
            None => &[],
        });
    let pages = &views[..slot.data.len()];
    #[cfg(feature = "interactive")]
    {
        crate::desktop::present_app(slot.task, req.spec(), &mut |out| {
            kernel_core::progout::gather_present(req, pages, out)
        })
    }
    #[cfg(not(feature = "interactive"))]
    {
        // No live desktop in a gate image: refused.
        let _ = (pages, req);
        false
    }
}

/// Run programs together (ADR-212): each placed in its own address space, each admitted through
/// the resident advisor as its own task, all dispatched by one priority scheduler so they take
/// turns - a timer slice or a syscall ends a turn - until each has exited, faulted or spent
/// `budget` timer slices. One report per program, in the order given; `None` if any cannot be
/// placed or admitted (nothing runs then, and every frame is given back).
fn run_programs(
    set: &[(&kernel_core::elf::Placement, &[u8])],
    services: &mut dyn kernel_core::progout::ProgramServices,
    live: bool,
    budget: u32,
) -> Option<Vec<kernel_core::shell::ProgramRun>> {
    use crate::hal::{ActiveHal, Hal};
    use kernel_core::mlsched::resident;
    use kernel_core::priosched::{Priority, PriorityScheduler};
    use kernel_core::taskfeat::{JobId, Outcome, TaskSubmission, UserId};

    let root_main = vm::active_root();
    let mut slots: Vec<Slot> = Vec::with_capacity(set.len());
    for (program, args) in set {
        match place_program(program, args) {
            Some(slot) => slots.push(slot),
            None => {
                slots.into_iter().for_each(Slot::free);
                return None;
            }
        }
    }

    let mut policy = PriorityScheduler::default();
    let now_secs = ActiveHal::ticks_to_ns(ActiveHal::timer_ticks()) / 1_000_000_000;
    let _ = resident::observe_memory(kernel_core::mlsched::MemoryMeter {
        total_pages: frames::total_count() as u64,
        free_pages: frames::free_count() as u64,
    });
    for i in 0..slots.len() {
        let submission = TaskSubmission {
            sched_class: 2,
            priority: 5,
            cpu_millis: 500,
            memory_pages: 2,
            disk_pages: None,
            diff_machine: false,
            task_index: i as u32,
            job: JobId(2),
            user: UserId(0),
        };
        if resident::admit(
            &mut policy,
            TaskId(i as u64),
            Priority(5),
            now_secs,
            &submission,
        )
        .is_err()
        {
            slots.into_iter().for_each(Slot::free);
            return None;
        }
    }

    // SAFETY: single-threaded; no read or frame is pending before any program has run.
    unsafe {
        *addr_of_mut!(PENDING_READ) = None;
        *addr_of_mut!(PENDING_PRESENT) = None;
        *addr_of_mut!(PENDING_POLL) = false;
        *addr_of_mut!(PENDING_BRK) = None;
    }
    // Preemptible (ADR-203): the S-timer is the only source enabled while the program runs, so a
    // slice the program never yields ends at the deadline; a fresh one, so none is already pending.
    timer_arm();
    stie_enable();
    // The budget is timer slices; a syscall returns here without spending one, so a separate cap
    // bounds a program that does nothing but trap (ADR-204).
    let mut dispatched = 0u32;
    while let Some(id) = policy.schedule_next() {
        dispatched += 1;
        let slot = &mut slots[id.0 as usize];
        slot.run.ended_at = dispatched;
        let ended = run_slice(slot, root_main, services);
        let spent = slot.run.preempted >= budget || slot.run.slices >= PROGRAM_DISPATCHES;
        // Ended, or its budget spent (abandoned): the others keep their turns either way.
        if let Some(outcome) = ended.or(spent.then_some(Outcome::Evicted)) {
            policy.finish(id);
            resident::observe_outcome(JobId(2), UserId(0), outcome);
        }
    }
    stie_disable();
    // The live desktop is pumped from this same timer: left set, its handler re-arms at its own rate.
    // A run outside the live console (the boot suite) leaves the deadline off, as it found it.
    if !live {
        timer_disable();
    }
    Some(slots.into_iter().map(Slot::finish).collect())
}

/// Programs left running in the background (ADR-213). Touched only by the console's thread with
/// interrupts masked.
static mut JOBS: kernel_core::jobs::Jobs<Slot> = kernel_core::jobs::Jobs::new();

/// `start NAME` (ADR-213): place the program and admit it through the resident advisor exactly as
/// `run` does, then leave it in [`JOBS`] for the console's idle loop to give turns to.
pub fn start_program_live(
    name: &str,
    program: &kernel_core::elf::Placement,
    args: &[u8],
) -> Result<u32, &'static str> {
    use crate::hal::{ActiveHal, Hal};
    use kernel_core::mlsched::resident;
    use kernel_core::priosched::{Priority, PriorityScheduler};
    use kernel_core::taskfeat::{JobId, TaskSubmission, UserId};

    let were_enabled = crate::heap::irq_save();
    let started = (|| {
        let slot = place_program(program, args)
            .ok_or("it cannot be placed: too many arguments or not enough free memory")?;
        let mut policy = PriorityScheduler::default();
        let _ = resident::observe_memory(kernel_core::mlsched::MemoryMeter {
            total_pages: frames::total_count() as u64,
            free_pages: frames::free_count() as u64,
        });
        let submission = TaskSubmission {
            sched_class: 2,
            priority: 5,
            cpu_millis: 500,
            memory_pages: 2,
            disk_pages: None,
            diff_machine: false,
            task_index: 0,
            job: JobId(2),
            user: UserId(0),
        };
        let now_secs = ActiveHal::ticks_to_ns(ActiveHal::timer_ticks()) / 1_000_000_000;
        if resident::admit(&mut policy, TaskId(0), Priority(5), now_secs, &submission).is_err() {
            slot.free();
            return Err("the resident advisor refused it at the memory boundary");
        }
        // SAFETY: single-threaded, interrupts masked; nothing else touches the table meanwhile.
        unsafe { (*addr_of_mut!(JOBS)).add(name, slot) }.map_err(|slot| {
            slot.free();
            "every background place is taken; `kill` one first"
        })
    })();
    crate::heap::irq_restore(were_enabled);
    started
}

/// Whether any program is left running (ADR-213).
pub fn jobs_live() -> bool {
    // SAFETY: a read of the table on the console's own thread, which is the only one that writes it.
    unsafe { !(*addr_of!(JOBS)).is_empty() }
}

/// One turn for the next background program (ADR-213), from the console's idle loop, under
/// [`fenced`] like a foreground run.
pub fn tick_jobs_live(
    services: &mut dyn kernel_core::progout::ProgramServices,
    ended: &mut dyn FnMut(u32, &kernel_core::jobs::JobName, &kernel_core::shell::ProgramRun),
) {
    fenced(|| tick_jobs(services, ended, true))
}

/// The turn itself, with the S-timer the program's slice ends on. A program that ended in it is
/// taken out, its pages given back, and handed to `ended`. `live`: the deadline is left set for the
/// desktop's handler to re-arm, as a live `run` leaves it; otherwise it is taken off.
fn tick_jobs(
    services: &mut dyn kernel_core::progout::ProgramServices,
    ended: &mut dyn FnMut(u32, &kernel_core::jobs::JobName, &kernel_core::shell::ProgramRun),
    live: bool,
) {
    use kernel_core::mlsched::resident;
    use kernel_core::taskfeat::{JobId, UserId};

    let root_main = vm::active_root();
    // SAFETY: single-threaded, interrupts masked; the table is the console thread's own.
    let jobs = unsafe { &mut *addr_of_mut!(JOBS) };
    timer_arm();
    stie_enable();
    let done = jobs.next_turn().and_then(|job| {
        run_slice(&mut job.slot, root_main, services).map(|outcome| (job.id, outcome))
    });
    stie_disable();
    if !live {
        timer_disable();
    }
    // The program's turn ended on the timer this desktop is pumped from, at U-mode, where nothing
    // pumps it: pump it here (interrupts are masked), so a program that never yields cannot freeze
    // the screen.
    #[cfg(feature = "interactive")]
    crate::desktop::tick_pump();
    let finished = done.and_then(|(id, outcome)| {
        resident::observe_outcome(JobId(2), UserId(0), outcome);
        jobs.take(id)
    });
    if let Some(job) = finished {
        let id = job.id;
        let run = job.slot.finish();
        ended(id, &job.name, &run);
    }
}

/// Every background program, as `jobs` shows it (ADR-213).
pub fn list_jobs(each: &mut dyn FnMut(&kernel_core::jobs::JobFacts)) {
    // SAFETY: a read of the table on the console's own thread, which is the only one that writes it.
    let jobs = unsafe { &*addr_of!(JOBS) };
    for j in jobs.iter() {
        each(&kernel_core::jobs::JobFacts {
            id: j.id,
            name: j.name,
            slices: j.slot.run.slices,
        });
    }
}

/// `kill ID` (ADR-213): end a background program now and give back everything it holds.
pub fn kill_job_live(
    id: u32,
) -> Option<(kernel_core::jobs::JobName, kernel_core::shell::ProgramRun)> {
    use kernel_core::mlsched::resident;
    use kernel_core::taskfeat::{JobId, Outcome, UserId};

    let were_enabled = crate::heap::irq_save();
    // SAFETY: single-threaded, interrupts masked; the table is the console thread's own.
    let job = unsafe { (*addr_of_mut!(JOBS)).take(id) };
    crate::heap::irq_restore(were_enabled);
    let job = job?;
    resident::observe_outcome(JobId(2), UserId(0), Outcome::Evicted);
    Some((job.name, job.slot.finish()))
}

/// Run two **real U-mode tasks** — own address spaces, own trap frames, real `sret` context switches
/// — admitted through the machine's **resident risk advisor** and dispatched by the shared
/// `PriorityScheduler` (REQ-ML-003, ADR-056).
///
/// The RISC-V counterpart of the aarch64 scenario, and it exists for the same reason: until it did,
/// the forest was resident and consulted for every admission on the priority-scheduler path, but this
/// target span its U-mode tasks through `RoundRobin` in [`run_scheduler`], so a *real user-mode task*
/// never reached it.
///
/// The mechanism is untouched — the same `run_one_shot` + `satp` switch [`run_scheduler`] exercises.
/// What differs: each task is described to the advisor at admission with the memory it actually
/// mapped, dispatch comes from `PriorityScheduler::schedule_next`, and both the dispatch and the exit
/// are fed back into what the NEXT advice reads.
///
/// Returns `(both_ran_and_exited, every_slice_presented_its_own_magic, both_were_advised)`.
fn run_advised_scheduler() -> (bool, bool, bool) {
    use crate::hal::{ActiveHal, Hal};
    use kernel_core::mlsched::resident;
    use kernel_core::priosched::{Priority, PriorityScheduler};
    use kernel_core::taskfeat::{JobId, Outcome, TaskSubmission, UserId};

    let root_main = vm::active_root();
    let magics = [0x333u64, 0x444u64];
    let mut roots = [0usize; NTASK];
    // Kept so the run can give every frame back: the console runs this again and again (ADR-199).
    let mut pages = [None; NTASK];
    for (r, p) in roots.iter_mut().zip(pages.iter_mut()) {
        let root = vm::build_identity().expect("advised sched root");
        let code = map_user_code(root, USER_CODE_VA, stub(_stub_sched_s, _stub_sched_e))
            .expect("advised sched code");
        let stack = map_user_data(root, USER_STACK_VA).expect("advised sched stack");
        *r = root;
        *p = Some((code, stack));
    }
    let mut tcb = [
        make_frame(USER_CODE_VA, USER_STACK_TOP, 0, magics[0], 0),
        make_frame(USER_CODE_VA, USER_STACK_TOP, 0, magics[1], 0),
    ];

    let advices_before = resident::stats().map(|s| s.advices).unwrap_or(0);

    // Admission: each task is described with what it ACTUALLY holds on this machine — one code page
    // and one stack page — rather than with a plausible-looking constant. A feature vector that does
    // not describe this task is exactly the failure `taskfeat.rs` exists to prevent.
    let mut policy = PriorityScheduler::default();
    let now_secs = ActiveHal::ticks_to_ns(ActiveHal::timer_ticks()) / 1_000_000_000;
    for i in 0..NTASK {
        let submission = TaskSubmission {
            sched_class: 2,
            priority: 5,
            cpu_millis: 500,
            memory_pages: 2,
            // No per-task disk request this kernel can measure, and it says so rather than reporting
            // a zero it never observed.
            disk_pages: None,
            diff_machine: false,
            task_index: i as u32,
            job: JobId(1),
            user: UserId(0),
        };
        // The allocator's word first (ADR-081): the bounded door judges this REAL task against the
        // frames actually free right now, and a refusal here is a kernel that is out of memory for
        // a two-page task - a failure to be named, never routed around.
        let _ = resident::observe_memory(kernel_core::mlsched::MemoryMeter {
            total_pages: frames::total_count() as u64,
            free_pages: frames::free_count() as u64,
        });
        if let Err(refusal) = resident::admit(
            &mut policy,
            TaskId(i as u64),
            Priority(5),
            now_secs,
            &submission,
        ) {
            kprintln!(
                "[usermode] the memory boundary refused a real task: {:?}",
                refusal
            );
            return (false, false, false);
        }
    }

    let mut order: Vec<usize> = Vec::new();
    let mut magic_ok = true;
    let mut guard = 0usize;
    let mut exits = 0usize;

    while let Some(TaskId(id)) = policy.schedule_next() {
        let i = id as usize;
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
        resident::observe_schedule();
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
            resident::observe_outcome(JobId(1), UserId(0), Outcome::Finished);
            exits += 1;
        }
        guard += 1;
        if guard > 4 * NTASK {
            break;
        }
    }

    // The interleaving is deliberately NOT asserted: advice may reorder equals, and demanding a fixed
    // order would be asserting that the advisor had no effect. That every task gets every slice and
    // exits IS asserted — nothing invented, dropped or starved.
    let slices = [
        order.iter().filter(|s| **s == 0).count(),
        order.iter().filter(|s| **s == 1).count(),
    ];
    let ran_ok = order.len() == 8 && slices[0] == 4 && slices[1] == 4 && exits == NTASK;
    let both_advised = resident::stats()
        .map(|s| s.advices == advices_before + NTASK as u64)
        .unwrap_or(false);
    for (root, p) in roots.iter().zip(pages.iter_mut()) {
        if let Some((code, stack)) = p.take() {
            vm::unmap_page(*root, USER_STACK_VA);
            frames::free_as(stack, Owner::USER);
            vm::unmap_page(*root, USER_CODE_VA);
            frames::free_as(code, Owner::USER);
        }
        vm::destroy_space(*root);
    }
    (ran_ok, magic_ok, both_advised)
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
        // The slice budget must start when the TASK starts, not when the previous preemption was
        // handled. The handler re-arms (that is what clears the pending timer), but everything
        // between there and here — the bookkeeping above, two address-space switches — spends that
        // budget in the KERNEL, and `rdtime` is wall-clock under TCG: on a loaded host the deadline
        // could already be past when the task resumed, so it took its interrupt before executing a
        // single `addi s2, s2, 1` and invariant 10 (progress) failed for a reason that had nothing
        // to do with state preservation. Re-arming here bounds that window to the resume itself.
        timer_arm();
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
                                      // State preserved means the counter NEVER goes backwards: a lost or crossed context
                                      // resumes the task with another value (the fresh frame's 0, or the other task's count).
                                      // A slice with ZERO progress is not a lost context. `rdtime` is wall-clock under TCG, and
                                      // on a saturated host (2026-09-25: a llama.cpp server beside the gate) the resume path
                                      // alone outlasted the 5 ms slice, the task took its interrupt before one `addi`, and this
                                      // invariant failed for the host's load. That the task advances at all is checked over the
                                      // whole run, below.
        if prog < last_prog[cur] {
            progress_ok = false;
        }
        last_prog[cur] = prog;
        cur = 1 - cur;
    }
    timer_disable();
    stie_disable();

    let fair = clean && counts.iter().all(|&c| c > 0);
    let advanced = last_prog.iter().all(|&p| p > 0);
    (fair, progress_ok && advanced)
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

/// IPC across address spaces, TIMED (ADR-179): the RISC-V twin of the x86-64 benchmark. Each space
/// maps the send stub at `USER_CODE_VA` and the receive stub one page above it, because this
/// target's stubs name their syscall in code. One round trip is A sends, B receives, B sends the
/// reply, A receives: four U-mode entries, four `ecall` traps, eight `satp` switches. One Trial
/// serves every excursion, and the heap must not move across the timed loop.
///
/// Returns `(every_body_crossed_intact, nanoseconds_per_round_trip, heap_bytes_grown)`.
fn run_ipc_pingpong(n: u64) -> (bool, u64, isize) {
    use crate::hal::{ActiveHal, Hal};
    const RECV_VA: usize = USER_CODE_VA + 2 * frames::FRAME_SIZE;
    let root_main = vm::active_root();
    let (Some(root_a), Some(root_b)) = (vm::build_identity(), vm::build_identity()) else {
        return (false, 0, 0);
    };
    let mut pages: [(usize, usize, Option<frames::PhysFrame>); 6] = [
        (
            root_a,
            USER_CODE_VA,
            map_user_code(root_a, USER_CODE_VA, stub(_stub_send_s, _stub_send_e)),
        ),
        (
            root_a,
            RECV_VA,
            map_user_code(root_a, RECV_VA, stub(_stub_recv_s, _stub_recv_e)),
        ),
        (root_a, USER_STACK_VA, map_user_data(root_a, USER_STACK_VA)),
        (
            root_b,
            USER_CODE_VA,
            map_user_code(root_b, USER_CODE_VA, stub(_stub_send_s, _stub_send_e)),
        ),
        (
            root_b,
            RECV_VA,
            map_user_code(root_b, RECV_VA, stub(_stub_recv_s, _stub_recv_e)),
        ),
        (root_b, USER_STACK_VA, map_user_data(root_b, USER_STACK_VA)),
    ];
    let mut ok = pages.iter().all(|p| p.2.is_some());
    let mut ns = 0u64;
    let mut grown = 0isize;
    if ok {
        let mut engine = CapEngine::new(0x1BC0, 1000);
        let caps =
            alloc::vec![engine.mint("ipc-bench", "ipc.msg", Scope::All, Constraints::none())];
        // SAFETY: single-owner trial slot for the whole loop; the endpoint starts empty.
        unsafe {
            *addr_of_mut!(CURRENT) = Some(Trial {
                engine,
                store: Store::new(),
                caps,
                action: "ipc.msg",
                armed: false,
                allowed: false,
                isolation_held: false,
                fault_va: 0,
            });
            *addr_of_mut!(ENDPOINT) = None;
        }
        let trip = |root: usize, entry: usize, body: u64| {
            let mut f = make_frame(entry, USER_STACK_TOP, body, body, 0);
            // SAFETY: `root` identity-maps the running kernel; one U-mode syscall, then back.
            unsafe {
                vm::switch_address_space(root);
                run_one_shot(&mut f);
                vm::switch_address_space(root_main);
            }
        };
        let round = |i: u64| -> bool {
            trip(root_a, USER_CODE_VA, i);
            trip(root_b, RECV_VA, 0);
            let got = unsafe { *addr_of!(IPC_RECEIVED) };
            trip(root_b, USER_CODE_VA, got ^ 0xA5A5_A5A5);
            trip(root_a, RECV_VA, 0);
            got == i && unsafe { *addr_of!(IPC_RECEIVED) } == i ^ 0xA5A5_A5A5
        };
        ok &= round(0); // warm-up outside the meter
        let heap0 = crate::heap::used_bytes();
        let t0 = ActiveHal::timer_ticks();
        for i in 1..=n {
            ok &= round(i);
        }
        let ticks = ActiveHal::timer_ticks().wrapping_sub(t0);
        grown = crate::heap::used_bytes() as isize - heap0 as isize;
        ns = ActiveHal::ticks_to_ns(ticks) / n.max(1);
        // SAFETY: excursions complete; retire the trial.
        unsafe { *addr_of_mut!(CURRENT) = None };
    }
    for (root, va, f) in pages.iter_mut() {
        if let Some(f) = f.take() {
            vm::unmap_page(*root, *va);
            frames::free_as(f, Owner::USER);
        }
    }
    vm::destroy_space(root_a);
    vm::destroy_space(root_b);
    (ok, ns, grown)
}

// --- Invariants 17-19: real blocking IPC (REQ-IPC-010) --------------------------------------
/// Prove real blocking IPC on RISC-V (the aarch64 twin): a receiver that `recv`s an EMPTY endpoint
/// BLOCKS (descheduled via `kernel_core::sched`), a sender's `send` WAKES it and the kernel delivers
/// the body across `satp` address spaces (into the receiver's saved `a0`), and the woken receiver
/// RESUMES past its `ecall` and exits reporting the body. Returns
/// `(recv_blocked, send_woke_and_delivered, receiver_resumed_with_body)`.
fn run_blocking_ipc() -> (bool, bool, bool) {
    let root_main = vm::active_root();
    let root_r = vm::build_identity().expect("recv root");
    let root_s = vm::build_identity().expect("send root");
    map_user_code(
        root_r,
        USER_CODE_VA,
        stub(_stub_recv_exit_s, _stub_recv_exit_e),
    )
    .expect("r code");
    map_user_data(root_r, USER_STACK_VA).expect("r stack");
    map_user_code(root_s, USER_CODE_VA, stub(_stub_send_s, _stub_send_e)).expect("s code");
    map_user_data(root_s, USER_STACK_VA).expect("s stack");

    const BODY: u64 = 0xB10C_CAFE;
    let mut engine = CapEngine::new(0xB10C, 1000);
    let caps = alloc::vec![engine.mint("ipc", "ipc.msg", Scope::All, Constraints::none())];
    // SAFETY: single-owner trial + endpoint state, reset before any excursion.
    unsafe {
        *addr_of_mut!(ENDPOINT) = None;
        *addr_of_mut!(IPC_RECEIVED) = 0;
        *addr_of_mut!(IPC_RECV_BLOCKED) = false;
        *addr_of_mut!(IPC_BLOCK_MODE) = true;
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine,
            store: Store::new(),
            caps,
            action: "ipc.msg",
            armed: false,
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
    }
    // Receiver: recv→exit stub (a0=0). Sender: send stub with body in s2/a0.
    let mut recv_frame = make_frame(USER_CODE_VA, USER_STACK_TOP, 0, 0, 0);
    let mut send_frame = make_frame(USER_CODE_VA, USER_STACK_TOP, BODY, BODY, 0);
    let mut sched = RoundRobin::new();
    sched.spawn(TaskId(0)); // receiver
    sched.spawn(TaskId(1)); // sender

    // Step 1 — receiver recv's the empty endpoint and must BLOCK.
    // SAFETY: root_r replicates the kernel identity map.
    unsafe {
        vm::switch_address_space(root_r);
        run_one_shot(&mut recv_frame);
        vm::switch_address_space(root_main);
    }
    let recv_blocked = unsafe { *addr_of!(IPC_RECV_BLOCKED) };
    if recv_blocked {
        sched.block(TaskId(0));
    }

    // Step 2 — sender sends; because a receiver is blocked-waiting, the kernel WAKES it, delivers
    // the body into the receiver's a0 (regs[10]), and drains the slot.
    // SAFETY: root_s replicates the kernel identity map.
    unsafe {
        vm::switch_address_space(root_s);
        run_one_shot(&mut send_frame);
        vm::switch_address_space(root_main);
    }
    let sent = unsafe { (*addr_of!(ENDPOINT)).is_some() };
    let send_woke_and_delivered = if sent && recv_blocked {
        let body = unsafe { (*addr_of_mut!(ENDPOINT)).take() }.unwrap_or(0);
        unsafe { *addr_of_mut!(IPC_RECEIVED) = body };
        recv_frame.regs[10] = body; // deliver into the woken receiver's a0
        sched.unblock(TaskId(0));
        body == BODY && sched.state(TaskId(0)) == Some(TaskState::Ready)
    } else {
        false
    };

    // Step 3 — resume the woken receiver: continues past its recv `ecall` with a0 = body, then EXITs
    // reporting a0 — so a reported magic == BODY proves it received across spaces.
    sched_report(0, false);
    // SAFETY: root_r replicates the kernel identity map.
    unsafe {
        vm::switch_address_space(root_r);
        run_one_shot(&mut recv_frame);
        vm::switch_address_space(root_main);
    }
    let (reported, exited) = unsafe {
        let s = &*addr_of!(SCHED);
        (s.last_magic, s.exited)
    };
    let receiver_resumed_with_body = exited && reported == BODY;

    unsafe {
        *addr_of_mut!(IPC_BLOCK_MODE) = false;
        (*addr_of_mut!(CURRENT)).take();
    }
    (
        recv_blocked,
        send_woke_and_delivered,
        receiver_resumed_with_body,
    )
}

// --- Invariants 20-22: priority inheritance end-to-end (REQ-IPC-009) ------------------------
/// Prove priority inheritance through the real blocking-IPC path on RISC-V (the aarch64 twin): a HIGH
/// U-mode receiver blocks on the endpoint a LOW task services; the blocked HIGH donates its priority
/// (`PriorityScheduler`) so the boosted LOW is dispatched ahead of a Ready MEDIUM (inversion avoided),
/// LOW services, and HIGH wakes. MEDIUM is a scheduler-only Ready competitor. Returns
/// `(inversion_avoided, low_serviced, high_received)`.
fn run_priority_ipc() -> (bool, bool, bool) {
    let root_main = vm::active_root();
    let root_h = vm::build_identity().expect("high root");
    let root_l = vm::build_identity().expect("low root");
    map_user_code(
        root_h,
        USER_CODE_VA,
        stub(_stub_recv_exit_s, _stub_recv_exit_e),
    )
    .expect("h code");
    map_user_data(root_h, USER_STACK_VA).expect("h stack");
    map_user_code(root_l, USER_CODE_VA, stub(_stub_send_s, _stub_send_e)).expect("l code");
    map_user_data(root_l, USER_STACK_VA).expect("l stack");

    const BODY: u64 = 0x9A9A_5C5C;
    const LOW: TaskId = TaskId(0);
    const MED: TaskId = TaskId(1);
    const HIGH: TaskId = TaskId(2);
    const EP: Endpoint = Endpoint(1);

    let mut engine = CapEngine::new(0x9A9A, 1000);
    let caps = alloc::vec![engine.mint("ipc", "ipc.msg", Scope::All, Constraints::none())];
    // SAFETY: single-owner trial + endpoint state.
    unsafe {
        *addr_of_mut!(ENDPOINT) = None;
        *addr_of_mut!(IPC_RECEIVED) = 0;
        *addr_of_mut!(IPC_RECV_BLOCKED) = false;
        *addr_of_mut!(IPC_BLOCK_MODE) = true;
        *addr_of_mut!(CURRENT) = Some(Trial {
            engine,
            store: Store::new(),
            caps,
            action: "ipc.msg",
            armed: false,
            allowed: false,
            isolation_held: false,
            fault_va: 0,
        });
    }
    let mut peng = CapEngine::new(0x00EE, 1000);
    let acq = peng.mint("sched", "ep.acquire", Scope::All, Constraints::none());
    let mut ps = PriorityScheduler::new("ep.acquire");
    ps.admit(LOW, Priority(1));
    ps.admit(MED, Priority(5));
    ps.admit(HIGH, Priority(10));
    let _ = ps.acquire(&peng, EP, LOW, &[acq]);

    let mut high_frame = make_frame(USER_CODE_VA, USER_STACK_TOP, 0, 0, 0);
    let mut low_frame = make_frame(USER_CODE_VA, USER_STACK_TOP, BODY, BODY, 0);

    // Step 1 — HIGH runs first and BLOCKS; it then WAITS on the endpoint LOW holds, donating to LOW.
    // SAFETY: root_h replicates the kernel identity map.
    unsafe {
        vm::switch_address_space(root_h);
        run_one_shot(&mut high_frame);
        vm::switch_address_space(root_main);
    }
    let high_blocked = unsafe { *addr_of!(IPC_RECV_BLOCKED) };
    if high_blocked {
        let _ = ps.wait(&peng, EP, HIGH, &[acq]);
    }

    // The inheritance decision: boosted LOW dispatched ahead of the Ready MEDIUM.
    let boosted = ps.effective_priority(LOW) == Priority(10);
    let picked = ps.schedule_next();
    let inversion_avoided = high_blocked && boosted && picked == Some(LOW);

    // Step 2 — run the dispatched LOW: it services the endpoint (sends), waking HIGH.
    let low_serviced = if picked == Some(LOW) {
        // SAFETY: root_l replicates the kernel identity map.
        unsafe {
            vm::switch_address_space(root_l);
            run_one_shot(&mut low_frame);
            vm::switch_address_space(root_main);
        }
        let sent = unsafe { (*addr_of!(ENDPOINT)).is_some() };
        if sent && high_blocked {
            let body = unsafe { (*addr_of_mut!(ENDPOINT)).take() }.unwrap_or(0);
            unsafe { *addr_of_mut!(IPC_RECEIVED) = body };
            high_frame.regs[10] = body; // deliver into the woken HIGH receiver's a0
            let _ = ps.release(EP, LOW);
            body == BODY
        } else {
            false
        }
    } else {
        false
    };

    // Step 3 — HIGH resumes as highest-priority and receives the body across spaces.
    sched_report(0, false);
    // SAFETY: root_h replicates the kernel identity map.
    unsafe {
        vm::switch_address_space(root_h);
        run_one_shot(&mut high_frame);
        vm::switch_address_space(root_main);
    }
    let (reported, exited) = unsafe {
        let s = &*addr_of!(SCHED);
        (s.last_magic, s.exited)
    };
    let high_received = exited && reported == BODY;

    unsafe {
        *addr_of_mut!(IPC_BLOCK_MODE) = false;
        (*addr_of_mut!(CURRENT)).take();
    }
    (inversion_avoided, low_serviced, high_received)
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
// Selftest — 29 U-mode boundary invariants, riscv64-only. `Ok(n)` all passed; `Err((idx,name))`
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

    // The resident risk advisor is consulted about REAL U-mode tasks, not only about the
    // commissioning workload (REQ-ML-003, ADR-056). Same address spaces, same trap frames, same
    // `run_one_shot` + satp switch as the round-robin run above.
    let (advised_ran, advised_magic, both_advised) = run_advised_scheduler();
    check!(
        advised_ran,
        "u-mode: two REAL U-mode tasks admitted through the resident advisor each get every slice and exit"
    );
    check!(
        advised_magic,
        "u-mode: the full register file survives each context switch under the advised scheduler too"
    );
    check!(
        both_advised,
        "u-mode: the advisor was consulted once per real user-mode task — a live spawn reaches the model"
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

    // 17, 18 & 19 — real BLOCKING IPC (REQ-IPC-010): recv on empty BLOCKS, send WAKES + delivers
    // across satp spaces, the woken receiver RESUMES past its ecall with the body in a0 and reports it.
    let (recv_blocked, send_woke, receiver_resumed) = run_blocking_ipc();
    check!(
        recv_blocked,
        "u-mode: recv on an empty endpoint BLOCKS the receiver — it is descheduled (kernel_core::sched)"
    );
    check!(
        send_woke,
        "u-mode: a send WAKES the blocked receiver (unblock ⇒ Ready) and delivers the body across spaces"
    );
    check!(
        receiver_resumed,
        "u-mode: the woken receiver RESUMES past its ecall with the body in a0 and exits reporting it"
    );

    // 20, 21 & 22 — priority inheritance end-to-end (REQ-IPC-009): a blocked HIGH donates to the LOW
    // endpoint holder, so the boosted LOW is dispatched over a Ready MEDIUM; LOW services, HIGH wakes.
    let (inversion_avoided, low_serviced, high_received) = run_priority_ipc();
    check!(
        inversion_avoided,
        "u-mode: blocked HIGH donates to the LOW endpoint holder — scheduler dispatches boosted LOW over Ready MEDIUM"
    );
    check!(
        low_serviced,
        "u-mode: the boosted LOW runs and services the endpoint (sends), waking HIGH"
    );
    check!(
        high_received,
        "u-mode: HIGH resumes as highest-priority and receives the body across address spaces"
    );

    // The task supervisor's policy and end-to-end kill/reclaim/continue path are live in THIS kernel
    // (REQ-REL-002, ADR-042). An undeclared U-mode fault must kill one task, reclaim its space, and leave
    // the machine able to admit another task.
    {
        use kernel_core::faultclass::{classify, from_x86_error_code, verdict};
        use kernel_core::sched::TaskId;
        use kernel_core::supervisor::{Supervisor, SupervisorAction, TerminationReason};
        let mut probe = Supervisor::new();
        let user = from_x86_error_code(0b100); // user, not present
        let ukind = classify(&user);
        let contained = probe.on_fault(Some(TaskId(1)), ukind, verdict(ukind));
        let kernelf = from_x86_error_code(0b011); // kernel write to a present page
        let kkind = classify(&kernelf);
        let escalated = probe.on_fault(Some(TaskId(2)), kkind, verdict(kkind));
        check!(
            contained == SupervisorAction::TaskTerminated(TerminationReason::Fault(ukind))
                && !probe.may_run(TaskId(1))
                && matches!(escalated, SupervisorAction::Escalate(_))
                && probe.may_run(TaskId(2))
                && probe.escalations() == 1,
            "supervisor: the policy is live in this kernel — a user fault terminates that task, a kernel fault escalates"
        );
        check!(
            supervisor().terminated() == 0 && supervisor().escalations() == 0,
            "supervisor: no task was terminated during this boot (every fault here was a declared trial)"
        );
    }

    {
        let before = supervisor().terminated();
        let dead = run_unexpected_fault();
        check!(
            dead != 0 && supervisor().terminated() == before + 1,
            "supervisor: undeclared U-mode fault terminates exactly one task and boot continues"
        );
        check!(
            !supervisor().may_run(kernel_core::sched::TaskId(dead))
                && matches!(
                    supervisor().reason(kernel_core::sched::TaskId(dead)),
                    Some(kernel_core::supervisor::TerminationReason::Fault(_))
                ),
            "supervisor: terminated U-mode task is never runnable and records fault reason"
        );
        let (held, _va) = run_isolation();
        check!(
            held,
            "supervisor: later U-mode task runs after faulted address-space teardown"
        );
    }

    let (_denied_info, denied_allowed) = run_process_info(false);
    check!(
        !denied_allowed,
        "process-info: no process.inspect capability is denied at the U-mode boundary"
    );
    let (granted_info, granted_allowed) = run_process_info(true);
    let (terminated, escalations) = kernel_core::syscall::unpack_process_info(granted_info);
    check!(
        granted_allowed
            && (terminated as usize, escalations as usize)
                == (supervisor().terminated(), supervisor().escalations()),
        "process-info: capability-bound U-mode query returns live supervisor counters"
    );

    // IPC across address spaces, timed (ADR-179): the number is REPORTED, never gated.
    let (crossed, ns_per_rt, grown) = run_ipc_pingpong(IPC_BENCH_ROUND_TRIPS);
    kprintln!(
        "[bench] ipc: {} cross-address-space round trips | {} ns/round-trip (4 U-mode entries, 4 syscall traps, 8 satp switches each) | heap +{} B",
        IPC_BENCH_ROUND_TRIPS,
        ns_per_rt,
        grown
    );
    check!(
        crossed && grown == 0,
        "ipc: a thousand round trips between two address spaces all cross intact, and the heap does not move"
    );
    // Programs from the namespace's format, contained (ADR-201, ADR-202): `hello` exits with its
    // status; `trap` executes an undefined instruction and costs exactly that task; sixteen more
    // faulting runs hold no death record and give back every frame and live heap byte.
    {
        use kernel_core::elf::{build, hello_code, judge, spin_code, trap_code, HELLO_STATUS};
        let t = PROGRAM_TARGET;
        let hello = build(t, hello_code(t.machine));
        let trap = build(t, trap_code(t.machine));
        let ran = judge(&hello, t).ok().and_then(|p| {
            run_program(
                &p,
                b"",
                &mut kernel_core::progout::NoServices,
                false,
                PROGRAM_SLICES,
            )
        });
        check!(
            ran.as_ref().is_some_and(|r| r.exited && r.status == HELLO_STATUS && r.terminated.is_none()),
            "run: a program image from the namespace format runs in user mode and exits with its status (55)"
        );
        // Programs speak (ADR-204): `hello`'s line reaches the console; a write outside the
        // program's two pages, straddling their end, or past the copy budget is refused with nothing
        // kept; a flood keeps exactly the sink's bound and counts the rest.
        check!(
            ran.as_ref()
                .is_some_and(|r| r.output == kernel_core::elf::HELLO_LINE && r.dropped == 0),
            "write: a program's line reaches the console through SYS_WRITE_CONSOLE"
        );
        // A program written in Rust (ADR-205): the seeded `hello`, compiled from userland/ for this
        // CPU, is judged and runs exactly as the hand-assembled one does.
        let rust = judge(USERLAND_HELLO, t).ok().and_then(|p| {
            run_program(
                &p,
                b"",
                &mut kernel_core::progout::NoServices,
                false,
                PROGRAM_SLICES,
            )
        });
        check!(
            rust.is_some_and(|r| r.exited
                && r.status == HELLO_STATUS
                && r.output == kernel_core::elf::HELLO_LINE),
            "run: hello built from Rust source in userland/ runs, prints its line and exits with 55"
        );
        // Arguments (ADR-206): handed at entry, greeted back; one byte past the bound is refused
        // before anything runs.
        let greeted = judge(USERLAND_HELLO, t).ok().and_then(|p| {
            run_program(
                &p,
                b"World",
                &mut kernel_core::progout::NoServices,
                false,
                PROGRAM_SLICES,
            )
        });
        check!(
            greeted.is_some_and(|r| r.exited && r.output == b"hello from user mode: World\n"),
            "run: a program is handed its arguments at entry and reads them from its own stack page"
        );
        let too_many = [b'a'; kernel_core::elf::MAX_ARGS + 1];
        check!(
            judge(USERLAND_HELLO, t)
                .ok()
                .and_then(|p| run_program(
                    &p,
                    &too_many,
                    &mut kernel_core::progout::NoServices,
                    false,
                    PROGRAM_SLICES
                ))
                .is_none(),
            "run: arguments past the bound are refused before the program starts"
        );
        // Programs read the namespace that started them (ADR-207), over a scratch namespace the
        // suite formats itself: served through the program's frames, refused by name otherwise,
        // and allocating nothing per read.
        {
            use kernel_core::fs::Filesystem;
            use kernel_core::progout::{FsServices, NoServices, ProgramServices};
            let mut dev =
                kernel_core::storage::MemBlockDevice::new(kernel_core::fs::FILE_DATA_START + 16);
            let mounted = Filesystem::format(&mut dev)
                .ok()
                .and_then(|_| Filesystem::mount(&mut dev).ok());
            let Some(mut fs) = mounted else {
                return Err((n + 1, "read: the suite's scratch namespace would not mount"));
            };
            let created = fs.create(&mut dev, "note", b"just words").is_ok();
            let run = |img: &[u8], a: &[u8], svc: &mut dyn ProgramServices| {
                judge(img, t).ok().and_then(|p| {
                    let p = &p;
                    run_program(p, a, svc, false, PROGRAM_SLICES)
                })
            };
            let mut svc = FsServices { fs: &fs, dev: &dev };
            let read = run(USERLAND_PROBE, b"read", &mut svc);
            check!(
                created
                    && read
                        .is_some_and(|r| r.exited && r.status == 10 && r.output == b"just words"),
                "read: a program reads an object from the namespace that started it"
            );
            let shown = run(USERLAND_SHOW, b"note", &mut svc);
            let bare = run(USERLAND_PROBE, b"read", &mut NoServices);
            check!(
                shown.is_some_and(|r| r.status == 10 && r.output == b"just words\n")
                    && bare
                        .is_some_and(|r| r.exited && r.status == u64::MAX && r.output.is_empty()),
                "read: `show` prints an object; a run handed no namespace is refused"
            );
            // A program with real mutable globals (ADR-210): its `.data` arrives as the image
            // declared it (40), its `.bss` arrives zeroed, both are writable, and a read of the
            // namespace lands in a global buffer on the writable page.
            let counted = run(USERLAND_COUNTER, b"", &mut svc);
            check!(
                counted.is_some_and(|r| r.exited && r.status == 55 && r.output.is_empty()),
                "data: a program's .data arrives as declared and its .bss arrives zeroed, both writable"
            );
            let into_globals = run(USERLAND_COUNTER, b"note", &mut svc);
            check!(
                into_globals.is_some_and(|r| r.exited
                    && r.status == 65
                    && r.output == b"counter read: just words\n"),
                "data: an object read by a program lands in a buffer on its writable page"
            );
            // A program whose code is three pages (ADR-211): every page the segment declares is
            // mapped, so the table it sums is all there and its exit value is the whole sum.
            let big = run(USERLAND_BIG, b"", &mut svc);
            check!(
                big.is_some_and(|r| r.exited
                    && r.status == 0x5839_3C00
                    && r.output == b"big ran\n"),
                "code: a program larger than one page runs, with every page of its segment mapped"
            );
            // A program whose writable memory is three pages (ADR-214): each page arrives zeroed
            // and distinct (a fill of 1, 2, 3 sums to 24,576), and a read lands in the second.
            let wide = run(USERLAND_WIDE, b"", &mut svc);
            let wide_read = run(USERLAND_WIDE, b"note", &mut svc);
            check!(
                wide.is_some_and(|r| r.exited && r.status == 24_576 && r.output.is_empty())
                    && wide_read.is_some_and(|r| r.exited
                        && r.status == 25_601
                        && r.output == b"wide read: just words\n"),
                "data: a program with three pages of writable memory gets each one zeroed and distinct, and reads into its second"
            );
            // Writable memory grown at run time (ADR-230): a top past the ceiling is refused, eight
            // pages arrive zeroed and distinct (a fill of 10..17 sums to 442,368), a read lands in
            // the last, and asking for less never shrinks.
            let grown = run(USERLAND_WIDE, b"grow", &mut svc);
            check!(
                grown.is_some_and(|r| r.exited
                    && r.status == 442_368
                    && r.output == b"wide grow: just words\n"),
                "brk: a program grows its writable memory by whole zeroed pages, inside its ceiling, and reads into them"
            );
            // A program that draws (ADR-215): a gate image has no live desktop, so its frame is
            // admitted from its data pages, refused, and it ends cleanly having shown none.
            let drawn = run(USERLAND_DRAW, b"once", &mut svc);
            check!(
                drawn.is_some_and(|r| r.exited && r.status == 0 && r.terminated.is_none()),
                "present: a frame with no live desktop to show it is refused and the program ends cleanly"
            );
            // The clock (ADR-222): two readings a program takes move forward.
            let clocked = run(USERLAND_SNAKE, b"clock", &mut svc);
            check!(
                clocked.is_some_and(|r| r.exited && r.status == 1),
                "clock: a program's two readings of the machine's clock move forward"
            );
            // Input (ADR-216): a program that holds no window is refused when it asks for input.
            let polled = run(USERLAND_DRAW, b"poll", &mut svc);
            check!(
                polled.is_some_and(|r| r.exited && r.status == 7),
                "input: a program with no window of its own is refused the window's input"
            );
            // A game (ADR-222): with no live desktop its first frame is refused, and it ends
            // cleanly with a score of nothing (2000 + 0).
            let played = run(USERLAND_SNAKE, b"", &mut svc);
            check!(
                played.is_some_and(|r| r.exited && r.status == 2000 && r.terminated.is_none()),
                "game: snake with no desktop to play on ends cleanly with no score"
            );
            let refused = [
                &b"codebuf"[..],
                b"straddle",
                b"outside",
                b"noname",
                b"pastend",
            ]
            .iter()
            .all(|case| {
                run(USERLAND_PROBE, case, &mut svc)
                    .is_some_and(|r| r.exited && r.status == u64::MAX)
            });
            check!(
                refused,
                "read: a buffer in the code page, a name straddling the pages or outside them, an empty name, and a buffer past the stack are refused"
            );
            if STORMS {
                let gross = |case: &[u8], svc: &mut dyn ProgramServices| {
                    let before = crate::heap::used_bytes();
                    let r = run(USERLAND_PROBE, case, svc);
                    (
                        crate::heap::used_bytes() - before,
                        r.is_some_and(|r| r.exited && r.status == 10),
                    )
                };
                let (few, few_ok) = gross(b"loop16", &mut svc);
                let (many, many_ok) = gross(b"loop256", &mut svc);
                crate::kprintln!(
                    "[usermode] read storm: 16 reads moved the gross heap {} B, 256 reads {} B",
                    few,
                    many
                );
                check!(
                    few_ok && many_ok && few == many,
                    "read: 256 reads allocate exactly what 16 do - nothing per read"
                );
            }
        }
        let cv = t.code_va;
        let write = |addr: u64, len: u64| {
            let img = build(t, &kernel_core::elf::writer_code(t.machine, addr, len));
            judge(&img, t).ok().and_then(|p| {
                run_program(
                    &p,
                    b"",
                    &mut kernel_core::progout::NoServices,
                    false,
                    PROGRAM_SLICES,
                )
            })
        };
        // Below the code pages, straddling the last page's end, and past the copy budget: a
        // program's window is its code pages, its stack and (if it has one) its data page.
        let refused = [
            (cv - 0x1000, 8),
            (PROGRAM_STACK_TOP as u64 - 4, 8),
            (cv, 65_537),
        ]
        .iter()
        .all(|&(a, l)| {
            write(a, l).is_some_and(|r| r.exited && r.status == u64::MAX && r.output.is_empty())
        });
        check!(
            refused,
            "write: a range outside the program's pages, straddling their end, or past the copy budget is refused and nothing is kept"
        );
        let flood = write(cv, 200);
        check!(
            flood.is_some_and(|r| r.exited
                && r.status == 56
                && r.output.len() == kernel_core::progout::CAPACITY
                && r.dropped == 144),
            "write: a flood keeps exactly the console's 256-byte bound and counts the other 144 bytes"
        );
        // The write path allocates nothing per call (ADR-086 storm discipline): the gross heap
        // watermark moves exactly as much across 1024 writes as across 16.
        if STORMS {
            let gross = |count: u64| {
                let img = build(t, &kernel_core::elf::loop_writer_code(t.machine, count, 16));
                let before = crate::heap::used_bytes();
                let r = judge(&img, t).ok().and_then(|p| {
                    run_program(
                        &p,
                        b"",
                        &mut kernel_core::progout::NoServices,
                        false,
                        PROGRAM_SLICES,
                    )
                });
                (crate::heap::used_bytes() - before, r)
            };
            // One run first, unmeasured: anything allocated once per boot on a run's path lands
            // there and not in either side.
            let _ = gross(16);
            let (few, few_run) = gross(16);
            let (many, many_run) = gross(1024);
            crate::kprintln!(
            "[usermode] write storm: 16 writes moved the gross heap {} B, 1024 writes {} B ({:?} / {:?} slices)",
            few,
            many,
            few_run.as_ref().map(|r| (r.slices, r.dropped)),
            many_run.as_ref().map(|r| (r.slices, r.dropped))
        );
            check!(
                few == many
                    && few_run.is_some_and(|r| r.exited && r.output.len() == 256 && r.dropped == 0)
                    && many_run.is_some_and(|r| r.exited
                        && r.output.len() == 256
                        && r.dropped == 1008 * 16),
                "write: 1024 writes allocate exactly what 16 do - nothing per call"
            );
        }
        let before = supervisor().terminated();
        let trapped = judge(&trap, t).ok().and_then(|p| {
            run_program(
                &p,
                b"",
                &mut kernel_core::progout::NoServices,
                false,
                PROGRAM_SLICES,
            )
        });
        check!(
            trapped.is_some_and(|r| r.terminated.is_some() && !r.exited)
                && supervisor().terminated() == before + 1,
            "run: a program whose first instruction is undefined is TERMINATED by the supervisor and the machine continues"
        );
        // A program that never yields (ADR-203): the timer ends every slice, the budget ends it.
        let spin = build(t, spin_code(t.machine));
        let spun = judge(&spin, t).ok().and_then(|p| {
            run_program(
                &p,
                b"",
                &mut kernel_core::progout::NoServices,
                false,
                BOOT_SPIN_SLICES,
            )
        });
        check!(
            spun.is_some_and(|r| !r.exited && r.terminated.is_none() && r.preempted == BOOT_SPIN_SLICES),
            "run: a program that never yields is preempted by the timer every slice and abandoned at its budget"
        );
        let after_spin = judge(&hello, t).ok().and_then(|p| {
            run_program(
                &p,
                b"",
                &mut kernel_core::progout::NoServices,
                false,
                PROGRAM_SLICES,
            )
        });
        check!(
            after_spin.is_some_and(|r| r.exited && r.status == HELLO_STATUS),
            "run: after an abandoned spinner, a program still runs and exits with its status"
        );
        // Programs run together (ADR-212): the spinner is admitted first and never yields, yet
        // `hello` finishes while it is still running, the faulting one is charged alone, and the
        // set gives back every frame and heap byte it took.
        let (frames0, heap0) = (frames::free_count(), crate::heap::live_bytes());
        let together = match (judge(&spin, t), judge(&hello, t), judge(&trap, t)) {
            (Ok(s), Ok(h), Ok(f)) => run_programs(
                &[(&s, b""), (&h, b""), (&f, b"")],
                &mut kernel_core::progout::NoServices,
                false,
                BOOT_SPIN_SLICES,
            ),
            _ => None,
        };
        check!(
            together.as_deref().is_some_and(|r| r.len() == 3
                && !r[0].exited
                && r[0].terminated.is_none()
                && r[0].preempted == BOOT_SPIN_SLICES
                && r[1].exited
                && r[1].status == HELLO_STATUS
                && r[2].terminated.is_some()
                && r[1].ended_at < r[0].ended_at
                && r[2].ended_at < r[0].ended_at),
            "together: a program and a faulting one finish while a spinner admitted before them still runs; each is charged its own end"
        );
        drop(together);
        check!(
            frames::free_count() == frames0 && crate::heap::live_bytes() == heap0,
            "together: three programs run at once give back every frame and heap byte"
        );
        // Programs left running (ADR-213): a spinner and `hello` started in the background; turns
        // are given until `hello` ends on its own, a foreground `run` works while the spinner is
        // still live, `kill` ends the spinner, and every frame and heap byte comes back.
        let (frames0, heap0) = (frames::free_count(), crate::heap::live_bytes());
        let (spin_job, hello_job) = match (judge(&spin, t), judge(&hello, t)) {
            (Ok(s), Ok(h)) => (
                start_program_live("spin", &s, b""),
                start_program_live("hello", &h, b""),
            ),
            _ => (Err("judge"), Err("judge")),
        };
        let mut ended: Option<(u32, kernel_core::shell::ProgramRun)> = None;
        let mut turns = 0;
        while ended.is_none() && turns < 64 {
            tick_jobs(
                &mut kernel_core::progout::NoServices,
                &mut |id, _, run| ended = Some((id, run.clone())),
                false,
            );
            turns += 1;
        }
        let foreground = judge(&hello, t).ok().and_then(|p| {
            run_program(
                &p,
                b"",
                &mut kernel_core::progout::NoServices,
                false,
                PROGRAM_SLICES,
            )
        });
        let mut listed = 0;
        list_jobs(&mut |j| {
            if j.name.as_str() == "spin" && j.slices > 0 {
                listed += 1;
            }
        });
        let killed = spin_job.ok().and_then(kill_job_live);
        check!(
            hello_job.is_ok()
                && ended.as_ref().is_some_and(|(id, r)| Ok(*id) == hello_job
                    && r.exited
                    && r.status == HELLO_STATUS)
                && foreground.is_some_and(|r| r.exited && r.status == HELLO_STATUS)
                && listed == 1
                && killed.is_some_and(|(n, r)| n.as_str() == "spin" && !r.exited && r.slices > 0)
                && !jobs_live(),
            "jobs: a background program ends on its own while a spinner runs beside it, a foreground run still works, and kill ends the spinner"
        );
        drop(ended);
        check!(
            frames::free_count() == frames0 && crate::heap::live_bytes() == heap0,
            "jobs: background programs give back every frame and heap byte"
        );
        if STORMS {
            let (held, frames0, heap0) = (
                supervisor().held(),
                frames::free_count(),
                crate::heap::live_bytes(),
            );
            let mut contained = 0;
            for _ in 0..16 {
                if judge(&trap, t)
                    .ok()
                    .and_then(|p| {
                        run_program(
                            &p,
                            b"",
                            &mut kernel_core::progout::NoServices,
                            false,
                            PROGRAM_SLICES,
                        )
                    })
                    .is_some_and(|r| r.terminated.is_some())
                {
                    contained += 1;
                }
            }
            check!(
            contained == 16
                && supervisor().held() == held
                && frames::free_count() == frames0
                && crate::heap::live_bytes() == heap0,
            "run: sixteen faulting runs are all contained, hold no death record, and give back every frame and heap byte"
        );
        }
    }

    Ok(n)
}

/// Round trips the boot's IPC benchmark times (ADR-179).
const IPC_BENCH_ROUND_TRIPS: u64 = 1000;
