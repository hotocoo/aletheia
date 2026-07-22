//! Ring-3 user-mode, the capability-gated syscall boundary, and preemptive multitasking
//! (PRD P5) — the x86-64 twin of the aarch64 backend's EL0 layer (`kernel/src/usermode.rs`).
//!
//! WHY THIS MATTERS: until this layer the x86-64 kernel re-proved every invariant *in ring 0* —
//! isolation was logical, not hardware-enforced. This module makes the boundary REAL: it drops the
//! CPU to **ring 3** (unprivileged) via `iretq`, runs genuinely less-privileged instruction streams
//! in user-only pages, and lets them reach the OS through one door — an `int 0x80` trap that lands
//! in a DPL=3 IDT gate and is authorized by the **same `CapEngine`** the deterministic pipeline
//! uses. It then gives each process its **own PML4 address space** (isolation across processes, not
//! just ring-3-vs-ring-0) and finally **context-switches** between ring-3 tasks — cooperatively
//! (tasks `SYS_YIELD`) and PREEMPTIVELY (the 8254 PIT's IRQ0, taken in ring 3, preempts a
//! non-yielding task and the round-robin scheduler switches it).
//!
//! ONE TRAP PATH (save-first). Every ring3->ring0 entry (`int 0x80`, timer IRQ) saves the FULL
//! register file into the running task's `TrapFrame` — pointed at by `CURRENT_FRAME` — BEFORE
//! touching anything, then dispatches. `resume_frame` restores a whole frame and `iretq`s, so a
//! task resumes *after* its trap; the same primitive starts a fresh task and resumes a preempted
//! one. `resume_return` restores the scheduler's callee-saved context (`KERNEL_CTX`) and returns to
//! it. This unification means the capability/isolation invariants and the scheduler run one path.
//!
//! ISOLATION, HONESTLY (advisor): aarch64 has separate "lower-EL" vectors, so an IRQ taken in EL1
//! never reaches the EL0 handler. x86 has ONE IDT entry per vector regardless of source ring, so
//! the discipline that replaces the vector split is: the kernel runs with **IF=0** for the whole
//! suite; only a ring-3 task's `RFLAGS` sets IF, so a timer IRQ is delivered *only* while a task
//! runs. The entries additionally fail closed if a saved `CS` shows ring 0 (would-be corruption).
//! And the ring-3 isolation proof reads a page WE mapped supervisor-only (not an OVMF-dependent
//! kernel address), so the `#PF` is guaranteed rather than a bet on firmware's U/S bits.
//!
//! Contract-honest (ADR-010): every line executes under QEMU and is asserted by `selftest()`; an
//! *unexpected* fault stays fatal, and the preemption test's loops are *bounded* so a dead timer
//! fails cleanly rather than hanging. Requires the frame allocator + live paging (both up by now).

use crate::spine::{CapEngine, CapToken, Constraints, Decision, Scope, Store, Target};
use crate::{frames, gdt, idt, vm};
use alloc::vec::Vec;
use core::ptr::{addr_of, addr_of_mut};
// REQ-KERN-005: the x86-64 target DRIVES the shared arch-independent scheduling policy from
// kernel-core rather than hand-rolling its own rotation — kernel-core decides which task runs next;
// this module performs only the context-switch MECHANISM (resume_frame + CR3 address-space switch).
use kernel_core::sched::{RoundRobin, TaskId, TaskState};
// REQ-IPC-008: the shared grant-table is the arch-independent authority/lifecycle layer over a
// shared-memory region; THIS target's PML4 `vm.rs` performs the real page mapping into each space.
use kernel_core::grant::{GrantTable, ShareMode};
// REQ-IPC-009/010: shared priority-inheritance scheduler for the blocking-IPC dispatch decision.
use kernel_core::priosched::{Endpoint, Priority, PriorityScheduler};

core::arch::global_asm!(
    r#"
.section .text

// ---- resume_frame(frame: *mut TrapFrame /* rdi */) -------------------------------------------
// Save the scheduler's callee-saved context into KERNEL_CTX so a trap can return to it, publish the
// frame in CURRENT_FRAME (so an entry saves back into it), build an iretq frame from *frame, load
// the whole register file, and iretq to ring 3. Starts a fresh task and resumes a yielded/preempted
// one identically.
.global resume_frame
resume_frame:
    lea     rax, [rip + KERNEL_CTX]
    mov     [rax + 0], rbx
    mov     [rax + 8], rbp
    mov     [rax + 16], r12
    mov     [rax + 24], r13
    mov     [rax + 32], r14
    mov     [rax + 40], r15
    mov     [rax + 48], rsp          // scheduler rsp (points at resume_frame's return addr)
    lea     rax, [rip + CURRENT_FRAME]
    mov     [rax], rdi               // current frame ptr for the entry's save-first
    // build the iretq frame: push SS, RSP, RFLAGS, CS, RIP (reverse of pop order)
    mov     rax, [rdi + 152]
    push    rax                      // SS
    mov     rax, [rdi + 144]
    push    rax                      // RSP
    mov     rax, [rdi + 136]
    push    rax                      // RFLAGS
    mov     rax, [rdi + 128]
    push    rax                      // CS
    mov     rax, [rdi + 120]
    push    rax                      // RIP
    // load the general-purpose register file (rdi loaded last — it holds the frame base)
    mov     rbx, [rdi + 8]
    mov     rcx, [rdi + 16]
    mov     rdx, [rdi + 24]
    mov     rsi, [rdi + 32]
    mov     rbp, [rdi + 48]
    mov     r8,  [rdi + 56]
    mov     r9,  [rdi + 64]
    mov     r10, [rdi + 72]
    mov     r11, [rdi + 80]
    mov     r12, [rdi + 88]
    mov     r13, [rdi + 96]
    mov     r14, [rdi + 104]
    mov     r15, [rdi + 112]
    mov     rax, [rdi + 0]
    mov     rdi, [rdi + 40]
    iretq

// ---- resume_return: restore KERNEL_CTX and RET to the caller of resume_frame -----------------
.global resume_return
resume_return:
    lea     rax, [rip + KERNEL_CTX]
    mov     rbx, [rax + 0]
    mov     rbp, [rax + 8]
    mov     r12, [rax + 16]
    mov     r13, [rax + 24]
    mov     r14, [rax + 32]
    mov     r15, [rax + 40]
    mov     rsp, [rax + 48]
    ret

// ---- SAVE-FIRST macro: stash the full register file of the trapping task into CURRENT_FRAME ----
// On entry the CPU has switched to RSP0 and pushed [SS][RSP][RFLAGS][CS][RIP] (no error code for an
// int gate / hardware IRQ). Leaves rbx = frame base, rsp = the CPU frame. Fails closed if the trap
// came from ring 0 (saved CS.RPL != 3) — the x86 stand-in for aarch64's lower-EL vector split.
.macro save_frame
    push    rax                      // scratch A
    push    rbx                      // scratch B
    lea     rax, [rip + CURRENT_FRAME]
    mov     rbx, [rax]               // rbx = *mut TrapFrame
    mov     [rbx + 16], rcx
    mov     [rbx + 24], rdx
    mov     [rbx + 32], rsi
    mov     [rbx + 40], rdi
    mov     [rbx + 48], rbp
    mov     [rbx + 56], r8
    mov     [rbx + 64], r9
    mov     [rbx + 72], r10
    mov     [rbx + 80], r11
    mov     [rbx + 88], r12
    mov     [rbx + 96], r13
    mov     [rbx + 104], r14
    mov     [rbx + 112], r15
    mov     rax, [rsp + 0]           // original rbx
    mov     [rbx + 8], rax
    mov     rax, [rsp + 8]           // original rax
    mov     [rbx + 0], rax
    add     rsp, 16                  // drop the two scratch words; rsp -> CPU iretq frame
    mov     rax, [rsp + 0]
    mov     [rbx + 120], rax         // RIP
    mov     rax, [rsp + 8]
    mov     [rbx + 128], rax         // CS
    mov     rax, [rsp + 16]
    mov     [rbx + 136], rax         // RFLAGS
    mov     rax, [rsp + 24]
    mov     [rbx + 144], rax         // RSP
    mov     rax, [rsp + 32]
    mov     [rbx + 152], rax         // SS
    mov     rax, [rbx + 128]
    and     rax, 3
    cmp     rax, 3
    jne     from_ring0_fatal
.endm

// ---- isr_syscall_entry (int 0x80, DPL=3): dispatch x86_syscall(num = rax, arg = rdi) ----------
.global isr_syscall_entry
isr_syscall_entry:
    save_frame
    mov     rdi, [rbx + 0]           // num  = saved rax
    mov     rsi, [rbx + 40]          // arg  = saved rdi
    and     rsp, -16                 // 16-align before a System V call
    call    x86_syscall
    jmp     resume_return

// ---- isr_timer_entry (IRQ0): acknowledge + mark preempted, then resume the scheduler ----------
.global isr_timer_entry
isr_timer_entry:
    save_frame
    and     rsp, -16
    call    x86_irq
    jmp     resume_return

// ---- isr_pf_entry (#PF): the faulting task is abandoned; hand CR2 to the armed-isolation check --
// #PF pushes an error code then the frame; we neither save nor unwind that stack (resume_return
// restores rsp from KERNEL_CTX), so we just read CR2 and dispatch.
.global isr_pf_entry
isr_pf_entry:
    mov     rdi, cr2
    and     rsp, -16
    call    x86_page_fault
    jmp     resume_return

// A trap arrived from ring 0 (should be impossible: the kernel runs IF=0). Fail closed.
from_ring0_fatal:
    mov     edi, 111
    and     rsp, -16
    call    usermode_fatal
    ud2

// ---- ring-3 stubs -----------------------------------------------------------------------------
// Assembler-encoded (no hand-hex), position-independent (only int/jmp-rel/reg-rel). Copied verbatim
// into a user code page and executed at USER_CODE_VA. Magic/counters are primed via the initial
// TrapFrame registers, so ONE stub serves every task.

// One syscall then park. Number in rax, arg in rdi (both primed by the frame).
.global stub_syscall_start
stub_syscall_start:
    int     0x80
10: jmp     10b
.global stub_syscall_end
stub_syscall_end:

// Read the address handed in rdi, then park. If rdi is unreadable the read faults first.
.global stub_read_start
stub_read_start:
    mov     rcx, [rdi]
11: jmp     11b
.global stub_read_end
stub_read_end:

// Read [rdi], then syscall (rax primed), then park. A successful syscall proves the read landed.
.global stub_read_syscall_start
stub_read_syscall_start:
    mov     rcx, [rdi]
    int     0x80
12: jmp     12b
.global stub_read_syscall_end
stub_read_syscall_end:

// Cooperative task: replay rbx (the frame-primed magic) into the syscall arg before each of three
// yields and one exit. rbx is NEVER written here, so a task presenting its own magic each slice
// proves the whole register file rides through every context switch.
.global stub_coop_start
stub_coop_start:
    mov     eax, 2                   // SYS_YIELD
    mov     rdi, rbx
    int     0x80
    mov     eax, 2
    mov     rdi, rbx
    int     0x80
    mov     eax, 2
    mov     rdi, rbx
    int     0x80
    mov     eax, 3                   // SYS_EXIT
    mov     rdi, rbx
    int     0x80
13: jmp     13b
.global stub_coop_end
stub_coop_end:

// Preemption task: a tight loop incrementing rbx (progress) while draining rcx (a bounded
// fallback). If rcx ever hits zero the task self-exits, so a NEVER-FIRING timer fails cleanly
// instead of hanging. A working timer preempts long before rcx drains.
.global stub_spin_start
stub_spin_start:
14: inc     rbx
    dec     rcx
    jnz     14b
    mov     eax, 3                   // SYS_EXIT
    mov     rdi, rbx
    int     0x80
15: jmp     15b
.global stub_spin_end
stub_spin_end:

// Blocking-IPC receiver: recv (blocks on empty; the kernel delivers the body into rdi on wake), then
// EXIT carrying rdi as the arg — so the received body is reported back through sched_report. `mov eax`
// does not touch rdi, so the delivered body survives to the exit syscall.
.global stub_recv_exit_start
stub_recv_exit_start:
    mov     eax, 5                   // SYS_RECV
    int     0x80
    mov     eax, 3                   // SYS_EXIT (rdi unchanged = delivered body)
    int     0x80
16: jmp     16b
.global stub_recv_exit_end
stub_recv_exit_end:
"#
);

// NOTE: `x86_64-unknown-uefi` makes `extern "C"` the Microsoft x64 ABI (args in RCX/RDX/R8/R9). Our
// hand-written trap assembly uses the System V ABI (arg0 in RDI, arg1 in RSI), so every function on
// the asm boundary is declared `sysv64` — otherwise `resume_frame` would read its frame pointer
// from the wrong register.
extern "sysv64" {
    /// Restore a full `TrapFrame` and `iretq` to ring 3; returns (via `resume_return`) when the
    /// task traps back and the handler resumes the caller.
    fn resume_frame(frame: *mut TrapFrame);
    static isr_syscall_entry: u8;
    static isr_timer_entry: u8;
    static isr_pf_entry: u8;
    static stub_syscall_start: u8;
    static stub_syscall_end: u8;
    static stub_read_start: u8;
    static stub_read_end: u8;
    static stub_read_syscall_start: u8;
    static stub_read_syscall_end: u8;
    static stub_coop_start: u8;
    static stub_coop_end: u8;
    static stub_spin_start: u8;
    static stub_spin_end: u8;
    static stub_recv_exit_start: u8;
    static stub_recv_exit_end: u8;
}

/// The running task's frame, published by `resume_frame` and saved into by every entry. One
/// excursion is ever in flight (single-core, no preemption of the kernel), so one slot.
#[no_mangle]
#[used]
static mut CURRENT_FRAME: u64 = 0;

/// The scheduler's callee-saved context (rbx, rbp, r12–r15, rsp) stashed by `resume_frame` and
/// restored by `resume_return`. One resume is ever in flight, so one slot.
#[no_mangle]
#[used]
static mut KERNEL_CTX: [u64; 7] = [0; 7];

// Register slot indices into `TrapFrame::regs` (byte offset = index * 8). The trap assembly
// hard-codes these offsets; the `const _` block below fails the build if the layout drifts.
const RAX: usize = 0;
const RBX: usize = 1;
const RCX: usize = 2;
const RDI: usize = 5;

/// A full ring-3 register context. `#[repr(C)]` fixes the byte offsets the trap asm hard-codes.
#[repr(C)]
#[derive(Clone, Copy)]
struct TrapFrame {
    regs: [u64; 15], // rax,rbx,rcx,rdx,rsi,rdi,rbp,r8..r15 (offsets 0..112)
    rip: u64,        // 120
    cs: u64,         // 128
    rflags: u64,     // 136
    rsp: u64,        // 144
    ss: u64,         // 152
}

const _: () = {
    assert!(core::mem::size_of::<TrapFrame>() == 160);
    assert!(core::mem::offset_of!(TrapFrame, rip) == 120);
    assert!(core::mem::offset_of!(TrapFrame, cs) == 128);
    assert!(core::mem::offset_of!(TrapFrame, rflags) == 136);
    assert!(core::mem::offset_of!(TrapFrame, rsp) == 144);
    assert!(core::mem::offset_of!(TrapFrame, ss) == 152);
};

/// RFLAGS bit 1 is reserved and must read 1. IF (bit 9) gates interrupt delivery in ring 3.
const RFLAGS_COOP: u64 = 0x0000_0002; // IF clear — cooperative / one-shot tasks (no preemption)
const RFLAGS_IF: u64 = 0x0000_0202; // IF set — preemptible tasks (the timer IRQ is delivered)

impl TrapFrame {
    const fn zeroed() -> Self {
        TrapFrame {
            regs: [0; 15],
            rip: 0,
            cs: 0,
            rflags: 0,
            rsp: 0,
            ss: 0,
        }
    }
    /// A fresh ring-3 task frame: entry RIP, user stack top, ring-3 selectors (RPL forced to 3).
    fn new_user(entry: u64, sp: u64, rflags: u64) -> Self {
        let sel = gdt::selectors();
        let mut f = Self::zeroed();
        f.rip = entry;
        f.rsp = sp;
        f.cs = (sel.user_code.0 | 3) as u64;
        f.ss = (sel.user_data.0 | 3) as u64;
        f.rflags = rflags;
        f
    }
}

/// Syscall numbers the ring-3 boundary understands. Anything else is denied (fail closed).
const SYS_EMIT: u64 = 1;
const SYS_YIELD: u64 = 2;
const SYS_EXIT: u64 = 3;
/// Capability-secure kernel IPC (gap register Issue 2): send/receive a message body through the
/// kernel endpoint, each authorized by the same `CapEngine` (`ipc.send` / `ipc.recv`).
const SYS_SEND: u64 = 4;
const SYS_RECV: u64 = 5;

/// User virtual addresses — the 1..2 GiB region (`vm::USER_REGION_PDPT_INDEX`). BELOW 4 GiB because
/// QEMU/OVMF enforce the ring-3 code segment's 4 GiB limit on the `iret` target; `build_space`
/// privatizes this 1 GiB region per process so mappings here are genuinely isolated.
const USER_CODE_VA: u64 = 0x4000_0000;
const USER_STACK_VA: u64 = USER_CODE_VA + 0x1000;
const USER_STACK_TOP: u64 = USER_STACK_VA + 0x1000;
/// A per-process private data page for the cross-process isolation test.
const VA_P: u64 = USER_CODE_VA + 0x3000;
/// A supervisor-only (no USER bit) page a ring-3 read must fault on — the isolation proof.
const VA_SUP: u64 = USER_CODE_VA + 0x5000;

/// Countdown preloaded into the spin task's rcx: large enough that a working 100 Hz timer preempts
/// before it drains, small enough that a BROKEN timer drains it (task self-exits) within the VM
/// watchdog. Not correctness-critical — mirrors the aarch64 bound.
const SPIN_COUNTDOWN: u64 = 0x2000_0000;

/// Bring-up gate (advisor): while `true`, run only the core round-trip invariants (1–2) so a
/// boot-or-die smoke test yields a legible pass before the full suite is enabled. Flip to `false`
/// to run all ten.
const BRINGUP_CORE_ONLY: bool = false;

// ---------------------------------------------------------------------------
// One-shot trial state (capability + isolation invariants) — reached by the Rust dispatchers.
// ---------------------------------------------------------------------------

struct Trial {
    engine: CapEngine,
    store: Store,
    caps: Vec<CapToken>,
    action: &'static str,
    /// When set, a `#PF` at `expect_fault_va` is the *expected* isolation test, not a fatal bug.
    armed: bool,
    expect_fault_va: u64,
    // outcomes, read back after the excursion returns
    allowed: bool,
    isolation_held: bool,
    fault_va: u64,
}

static mut CURRENT: Option<Trial> = None;

/// SAFETY: single-threaded; `CURRENT` is set immediately before an excursion and mutated only by
/// the dispatcher that excursion drives. No concurrent access exists.
#[inline]
fn current() -> Option<&'static mut Trial> {
    unsafe { (*addr_of_mut!(CURRENT)).as_mut() }
}

/// Kernel IPC endpoint (single-slot mailbox). A `SYS_SEND` deposits a body; a `SYS_RECV` drains it.
/// Sender and receiver run in SEPARATE PML4 spaces, so the body travels only through this kernel
/// object — never shared user memory.
static mut ENDPOINT: Option<u64> = None;
/// The body the most recent authorized `SYS_RECV` drained.
static mut IPC_RECEIVED: u64 = 0;
// Blocking IPC (REQ-IPC-010): when set (only during run_blocking_ipc/run_priority_ipc), an authorized
// SYS_RECV on an empty endpoint records that the caller must BLOCK instead of returning fail-value;
// the scheduler deschedules it until a SYS_SEND wakes it. Default off ⇒ run_ipc semantics untouched.
static mut IPC_BLOCK_MODE: bool = false;
static mut IPC_RECV_BLOCKED: bool = false;

// ---------------------------------------------------------------------------
// Scheduler state (multitasking invariants).
// ---------------------------------------------------------------------------

struct SchedState {
    last_magic: u64,
    exited: bool,
    /// Set by the timer entry: the task was involuntarily preempted (not a yield/exit).
    preempted: bool,
}
static mut SCHED: SchedState = SchedState {
    last_magic: 0,
    exited: false,
    preempted: false,
};

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

// ---------------------------------------------------------------------------
// Rust dispatchers, called from the assembly entries.
// ---------------------------------------------------------------------------

/// The capability-gated syscall AND the scheduler hooks, over one path.
#[no_mangle]
pub extern "sysv64" fn x86_syscall(num: u64, arg: u64) -> u64 {
    match num {
        SYS_EMIT => {
            let t = match current() {
                Some(t) => t,
                None => return u64::MAX,
            };
            match t.engine.evaluate(t.action, &Target::default(), &t.caps) {
                Decision::Allow => {
                    t.store.record_event(t.action, "ring3-process");
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
            let t = match current() {
                Some(t) => t,
                None => return u64::MAX,
            };
            match t.engine.evaluate(t.action, &Target::default(), &t.caps) {
                Decision::Allow => {
                    t.allowed = true;
                    // SAFETY: single-threaded; only the running task's trap touches the endpoint.
                    match unsafe { (*addr_of_mut!(ENDPOINT)).take() } {
                        Some(body) => {
                            unsafe { *addr_of_mut!(IPC_RECEIVED) = body };
                            body
                        }
                        None => {
                            // Empty. In blocking mode, signal the scheduler to deschedule this
                            // caller until a SYS_SEND wakes it; else non-blocking fail-value.
                            if unsafe { IPC_BLOCK_MODE } {
                                unsafe { *addr_of_mut!(IPC_RECV_BLOCKED) = true };
                            }
                            u64::MAX
                        }
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

/// Timer IRQ dispatch. Acknowledge the PIC and mark the running task preempted so the scheduler
/// round-robins to the next one. The PIT runs free (periodic mode 3), so no re-arm is needed.
#[no_mangle]
pub extern "sysv64" fn x86_irq() {
    crate::pic::eoi(idt::TIMER_VECTOR);
    // SAFETY: single-threaded; only the running task's IRQ writes this, read by the scheduler.
    unsafe { (*addr_of_mut!(SCHED)).preempted = true };
}

/// `#PF` dispatch. An armed isolation trial treats a fault at the expected VA as the proof and
/// resumes (the task is abandoned); any UNEXPECTED fault stays fatal so bugs cannot hide here.
#[no_mangle]
pub extern "sysv64" fn x86_page_fault(fault_va: u64) {
    match current() {
        Some(t) if t.armed && fault_va == t.expect_fault_va => {
            t.isolation_held = true;
            t.fault_va = fault_va;
            t.armed = false;
        }
        _ => {
            kprintln!("[usermode] UNEXPECTED ring-3 #PF at {:#x}", fault_va);
            usermode_fatal(104);
        }
    }
}

/// Fatal user-mode error — reached from the ring-0 guard and unexpected faults. Never returns.
#[no_mangle]
pub extern "sysv64" fn usermode_fatal(code: u32) -> ! {
    crate::exit::exit(code as i32)
}

/// Record what the running task reported this slice.
fn sched_report(magic: u64, exited: bool) {
    // SAFETY: single-threaded; only the running task's trap writes this, read by the scheduler.
    unsafe {
        let s = &mut *addr_of_mut!(SCHED);
        s.last_magic = magic;
        s.exited = exited;
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// A `&[u8]` view of an assembler-emitted stub between its `_start`/`_end` extern labels. Taking
/// the address of an `extern static` is `unsafe`, so the whole range read lives in one `unsafe`.
macro_rules! stub_bytes {
    ($start:ident, $end:ident) => {{
        // SAFETY: `$start`/`$end` bound a contiguous byte range in the kernel `.text`.
        unsafe {
            let s = addr_of!($start);
            let e = addr_of!($end);
            core::slice::from_raw_parts(s, (e as usize) - (s as usize))
        }
    }};
}

fn install_entries() {
    // SAFETY: valid raw interrupt entry points; called single-core with IF=0.
    unsafe {
        idt::install_usermode(
            addr_of!(isr_syscall_entry) as u64,
            addr_of!(isr_timer_entry) as u64,
            addr_of!(isr_pf_entry) as u64,
        );
    }
}

fn set_trial(t: Trial) {
    // SAFETY: single-threaded; install the trial the dispatcher reads for this excursion.
    unsafe { *addr_of_mut!(CURRENT) = Some(t) };
}
fn take_trial() -> Trial {
    // SAFETY: excursion complete; no other access to CURRENT exists.
    unsafe { (*addr_of_mut!(CURRENT)).take() }.expect("trial present")
}

/// Reclaim a mapped leaf page in `root`. (The page-table trees themselves are an intentional,
/// bounded, one-time boot-test leak — the pool has tens of thousands of frames; this runs once.)
fn free_leaf(root: u64, va: u64, f: Option<frames::Frame>) {
    if let Some(f) = f {
        vm::unmap_user(root, va);
        frames::free(f);
    }
}

// ---------------------------------------------------------------------------
// Excursions.
// ---------------------------------------------------------------------------

/// Run one ring-3 syscall excursion in a fresh address space. `grant` decides whether the process
/// holds the `event.emit` capability. Returns `(authorized, event_count_after)`.
fn run_syscall(grant: bool) -> (bool, usize) {
    let root_main = vm::active_root();
    let root = match vm::build_space() {
        Some(r) => r,
        None => return (false, usize::MAX),
    };
    let code = vm::map_stub_frame(
        root,
        USER_CODE_VA,
        stub_bytes!(stub_syscall_start, stub_syscall_end),
    );
    let stack = vm::map_user(root, USER_STACK_VA, true);
    if code.is_none() || stack.is_none() {
        free_leaf(root, USER_STACK_VA, stack);
        free_leaf(root, USER_CODE_VA, code);
        return (false, usize::MAX);
    }

    let mut engine = CapEngine::new(0xA5A5, 1000);
    let mut caps = Vec::new();
    if grant {
        caps.push(engine.mint(
            "ring3-process",
            "event.emit",
            Scope::All,
            Constraints::none(),
        ));
    }
    set_trial(Trial {
        engine,
        store: Store::new(),
        caps,
        action: "event.emit",
        armed: false,
        expect_fault_va: 0,
        allowed: false,
        isolation_held: false,
        fault_va: 0,
    });

    let mut f = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    f.regs[RAX] = SYS_EMIT;
    f.regs[RDI] = 0;
    // SAFETY: `root` maps the running kernel; switch into it, run the ring-3 excursion until it
    // traps, restore the scheduler's space. The frame lives on this stack (kernel, shared slot 0).
    unsafe {
        vm::switch_to(root);
        resume_frame(&mut f as *mut TrapFrame);
        vm::switch_to(root_main);
    }

    free_leaf(root, USER_STACK_VA, stack);
    free_leaf(root, USER_CODE_VA, code);
    let t = take_trial();
    (t.allowed, t.store.event_count())
}

/// Prove hardware isolation: a ring-3 read of a supervisor-only page faults and is contained (not
/// fatal). Returns `(isolation_held, fault_va)`.
fn run_isolation() -> (bool, u64) {
    let root_main = vm::active_root();
    let root = match vm::build_space() {
        Some(r) => r,
        None => return (false, 0),
    };
    let code = vm::map_stub_frame(
        root,
        USER_CODE_VA,
        stub_bytes!(stub_read_start, stub_read_end),
    );
    let stack = vm::map_user(root, USER_STACK_VA, true);
    let sup = vm::map_supervisor(root, VA_SUP);
    if code.is_none() || stack.is_none() || sup.is_none() {
        free_leaf(root, VA_SUP, sup);
        free_leaf(root, USER_STACK_VA, stack);
        free_leaf(root, USER_CODE_VA, code);
        return (false, 0);
    }

    set_trial(Trial {
        engine: CapEngine::new(0xA5A5, 1000),
        store: Store::new(),
        caps: Vec::new(),
        action: "event.emit",
        armed: true,
        expect_fault_va: VA_SUP,
        allowed: false,
        isolation_held: false,
        fault_va: 0,
    });

    let mut f = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    f.regs[RDI] = VA_SUP; // the supervisor page the ring-3 stub tries to read
                          // SAFETY: see `run_syscall`.
    unsafe {
        vm::switch_to(root);
        resume_frame(&mut f as *mut TrapFrame);
        vm::switch_to(root_main);
    }

    free_leaf(root, VA_SUP, sup);
    free_leaf(root, USER_STACK_VA, stack);
    free_leaf(root, USER_CODE_VA, code);
    let t = take_trial();
    (t.isolation_held, t.fault_va)
}

/// Run one ring-3 process in a dedicated address space (`switch_to(root)` around the excursion,
/// restore `root_main` after). `armed`/`expect` mark whether a fault is the expected proof. Returns
/// the taken `Trial`.
#[allow(clippy::too_many_arguments)] // an isolated ring-3 excursion legitimately needs all of these
fn run_in_space(
    root: u64,
    root_main: u64,
    rdi: u64,
    rax: u64,
    engine: CapEngine,
    caps: Vec<CapToken>,
    armed: bool,
    expect: u64,
) -> Trial {
    set_trial(Trial {
        engine,
        store: Store::new(),
        caps,
        action: "event.emit",
        armed,
        expect_fault_va: expect,
        allowed: false,
        isolation_held: false,
        fault_va: 0,
    });
    let mut f = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    f.regs[RAX] = rax;
    f.regs[RDI] = rdi;
    // SAFETY: `root` maps the running kernel; restored to `root_main` immediately after.
    unsafe {
        vm::switch_to(root);
        resume_frame(&mut f as *mut TrapFrame);
        vm::switch_to(root_main);
    }
    take_trial()
}

/// Prove **per-process address-space isolation**: two ring-3 processes in separate PML4 spaces,
/// where a page private to A is unreachable from B — even at the *same* virtual address. Returns
/// `(a_reached_own_page, b_isolated, b_fault_va)`.
fn run_cross_process_isolation() -> (bool, bool, u64) {
    let root_main = vm::active_root();
    let (root_a, root_b) = match (vm::build_space(), vm::build_space()) {
        (Some(a), Some(b)) => (a, b),
        _ => return (false, false, 0),
    };
    let stub_rs = stub_bytes!(stub_read_syscall_start, stub_read_syscall_end);
    // A: stub + stack + the private data page VA_P.
    let a_code = vm::map_stub_frame(root_a, USER_CODE_VA, stub_rs);
    let a_stack = vm::map_user(root_a, USER_STACK_VA, true);
    let a_data = vm::map_user(root_a, VA_P, true);
    // B: stub + stack only — VA_P deliberately left unmapped.
    let b_code = vm::map_stub_frame(root_b, USER_CODE_VA, stub_rs);
    let b_stack = vm::map_user(root_b, USER_STACK_VA, true);
    if a_code.is_none()
        || a_stack.is_none()
        || a_data.is_none()
        || b_code.is_none()
        || b_stack.is_none()
    {
        free_leaf(root_a, VA_P, a_data);
        free_leaf(root_a, USER_STACK_VA, a_stack);
        free_leaf(root_a, USER_CODE_VA, a_code);
        free_leaf(root_b, USER_STACK_VA, b_stack);
        free_leaf(root_b, USER_CODE_VA, b_code);
        return (false, false, 0);
    }

    // A reads its own VA_P (mapped) then makes an authorized syscall -> allowed proves both.
    let mut a_engine = CapEngine::new(0xA5A5, 1000);
    let a_caps =
        alloc::vec![a_engine.mint("process-a", "event.emit", Scope::All, Constraints::none())];
    let a = run_in_space(
        root_a, root_main, VA_P, SYS_EMIT, a_engine, a_caps, false, 0,
    );
    // B reads the SAME VA_P (unmapped in its space) -> armed fault at VA_P, contained.
    let b = run_in_space(
        root_b,
        root_main,
        VA_P,
        SYS_EMIT,
        CapEngine::new(0xA5A5, 1000),
        Vec::new(),
        true,
        VA_P,
    );

    free_leaf(root_a, VA_P, a_data);
    free_leaf(root_a, USER_STACK_VA, a_stack);
    free_leaf(root_a, USER_CODE_VA, a_code);
    free_leaf(root_b, USER_STACK_VA, b_stack);
    free_leaf(root_b, USER_CODE_VA, b_code);
    (a.allowed, b.isolation_held, b.fault_va)
}

// ---------------------------------------------------------------------------
// Capability-secure kernel IPC (gap register Issue 2). Two ring-3 processes in SEPARATE PML4 spaces
// exchange a message through a kernel endpoint — authorized by the same `CapEngine`, kernel-mediated,
// never shared user memory. x86-64 twin of the aarch64 IPC suite.
// ---------------------------------------------------------------------------

/// Run one endpoint excursion in space `root`: a ring-3 process with (optionally) an `action`
/// capability issues syscall `rax` with arg `rdi` and traps once. Returns whether it was authorized.
/// Precondition: `root` already maps the syscall stub + stack at the user VAs.
fn run_endpoint_excursion(
    root: u64,
    root_main: u64,
    action: &'static str,
    grant: bool,
    rax: u64,
    rdi: u64,
) -> bool {
    let mut engine = CapEngine::new(0xA5A5, 1000);
    let mut caps = Vec::new();
    if grant {
        caps.push(engine.mint("ipc-process", action, Scope::All, Constraints::none()));
    }
    set_trial(Trial {
        engine,
        store: Store::new(),
        caps,
        action,
        armed: false,
        expect_fault_va: 0,
        allowed: false,
        isolation_held: false,
        fault_va: 0,
    });
    let mut f = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    f.regs[RAX] = rax;
    f.regs[RDI] = rdi;
    // SAFETY: `root` maps the running kernel; switch in, run the ring-3 excursion, restore.
    unsafe {
        vm::switch_to(root);
        resume_frame(&mut f as *mut TrapFrame);
        vm::switch_to(root_main);
    }
    take_trial().allowed
}

/// Prove **capability-secure kernel IPC**: a message sent by one ring-3 process is delivered to
/// another in a DIFFERENT address space, through the kernel endpoint, only when both hold the
/// authorizing capability. Returns `(delivered_across_spaces, uncapable_send_denied,
/// uncapable_recv_denied)`.
fn run_ipc() -> (bool, bool, bool) {
    let root_main = vm::active_root();
    let (root_a, root_b) = match (vm::build_space(), vm::build_space()) {
        (Some(a), Some(b)) => (a, b),
        _ => return (false, false, false),
    };
    let stub = stub_bytes!(stub_syscall_start, stub_syscall_end);
    let a_code = vm::map_stub_frame(root_a, USER_CODE_VA, stub);
    let a_stack = vm::map_user(root_a, USER_STACK_VA, true);
    let b_code = vm::map_stub_frame(root_b, USER_CODE_VA, stub);
    let b_stack = vm::map_user(root_b, USER_STACK_VA, true);
    if a_code.is_none() || a_stack.is_none() || b_code.is_none() || b_stack.is_none() {
        free_leaf(root_a, USER_STACK_VA, a_stack);
        free_leaf(root_a, USER_CODE_VA, a_code);
        free_leaf(root_b, USER_STACK_VA, b_stack);
        free_leaf(root_b, USER_CODE_VA, b_code);
        return (false, false, false);
    }

    let body: u64 = 0xC0FF_EE42;

    // 1 — capable sender deposits, capable receiver drains; body survives the kernel trip.
    // SAFETY: single-threaded reset of the endpoint before the exchange.
    unsafe {
        *addr_of_mut!(ENDPOINT) = None;
        *addr_of_mut!(IPC_RECEIVED) = 0;
    }
    let send_ok = run_endpoint_excursion(root_a, root_main, "ipc.send", true, SYS_SEND, body);
    let recv_ok = run_endpoint_excursion(root_b, root_main, "ipc.recv", true, SYS_RECV, 0);
    let received = unsafe { *addr_of!(IPC_RECEIVED) };
    let spaces_distinct = root_a != root_b && root_a != root_main && root_b != root_main;
    let delivered = send_ok && recv_ok && received == body && spaces_distinct;

    // 2 — no ipc.send cap => cannot post (fail-closed, slot untouched).
    // SAFETY: single-threaded reset.
    unsafe { *addr_of_mut!(ENDPOINT) = None };
    let bad_send = run_endpoint_excursion(root_a, root_main, "ipc.send", false, SYS_SEND, body);
    let send_denied = !bad_send && unsafe { (*addr_of!(ENDPOINT)).is_none() };

    // 3 — no ipc.recv cap => cannot drain a queued message (fail-closed, slot intact).
    // SAFETY: single-threaded seed of a queued message.
    unsafe { *addr_of_mut!(ENDPOINT) = Some(body) };
    let bad_recv = run_endpoint_excursion(root_b, root_main, "ipc.recv", false, SYS_RECV, 0);
    let recv_denied = !bad_recv && unsafe { (*addr_of!(ENDPOINT)).is_some() };

    free_leaf(root_a, USER_STACK_VA, a_stack);
    free_leaf(root_a, USER_CODE_VA, a_code);
    free_leaf(root_b, USER_STACK_VA, b_stack);
    free_leaf(root_b, USER_CODE_VA, b_code);
    (delivered, send_denied, recv_denied)
}

/// Free each scheduled task's mapped leaf pages in its own space. (Table trees leak by design.)
fn cleanup_tasks(
    roots: &[u64; NTASK],
    code: &mut [Option<frames::Frame>; NTASK],
    stack: &mut [Option<frames::Frame>; NTASK],
) {
    for i in 0..NTASK {
        free_leaf(roots[i], USER_STACK_VA, stack[i].take());
        free_leaf(roots[i], USER_CODE_VA, code[i].take());
    }
}

/// Set up NTASK tasks, each in its own space, all sharing USER_CODE_VA (a different space is the
/// only thing routing that VA to the right task's stub). Returns roots, or `None` on exhaustion.
fn setup_tasks(
    code: &mut [Option<frames::Frame>; NTASK],
    stack: &mut [Option<frames::Frame>; NTASK],
    stub_bytes: &[u8],
) -> Option<[u64; NTASK]> {
    let mut roots = [0u64; NTASK];
    for i in 0..NTASK {
        roots[i] = vm::build_space()?;
        code[i] = vm::map_stub_frame(roots[i], USER_CODE_VA, stub_bytes);
        stack[i] = vm::map_user(roots[i], USER_STACK_VA, true);
        if code[i].is_none() || stack[i].is_none() {
            cleanup_tasks(&roots, code, stack);
            return None;
        }
    }
    Some(roots)
}

/// Run the round-robin scheduler over two cooperative ring-3 tasks, EACH IN ITS OWN SPACE. Returns
/// `(round_robin_and_both_exited, every_slice_presented_its_own_magic, spaces_distinct)`.
fn run_scheduler() -> (bool, bool, bool) {
    let root_main = vm::active_root();
    let magics: [u64; NTASK] = [0xA1A1, 0xB2B2];
    let mut code: [Option<frames::Frame>; NTASK] = [None, None];
    let mut stack: [Option<frames::Frame>; NTASK] = [None, None];
    let roots = match setup_tasks(
        &mut code,
        &mut stack,
        stub_bytes!(stub_coop_start, stub_coop_end),
    ) {
        Some(r) => r,
        None => return (false, false, false),
    };
    // SAFETY: single-threaded; init the TCBs before any resume. Each frame is primed with its
    // task's magic in rbx (frame-primed, never written by the stub).
    unsafe {
        let tcbs = &mut *addr_of_mut!(TCBS);
        for i in 0..NTASK {
            let mut f = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
            f.regs[RBX] = magics[i];
            tcbs[i] = Tcb {
                frame: f,
                done: false,
            };
        }
    }

    // Scheduling POLICY driven by the shared kernel_core::sched::RoundRobin (REQ-KERN-005): the
    // x86-64 target drives the SAME scheduler proved on the host and used by the aarch64 + RISC-V
    // backends, performing only the context-switch MECHANISM (resume_frame + CR3 switch) behind the
    // TaskContext seam. `schedule_next` picks; a yielded task rotates to the tail; an exited task is
    // `finish`ed. Reproduces the same A,B,A,B,A,B,A,B order (asserted below).
    let mut policy = RoundRobin::new();
    for i in 0..NTASK {
        policy.spawn(TaskId(i as u64));
    }
    let mut order: Vec<(usize, u64)> = Vec::new();
    while let Some(TaskId(id)) = policy.schedule_next() {
        let slot = id as usize;
        sched_report(0, false); // reset for this slice
                                // SAFETY: roots[slot] maps the kernel; switch into the task's space, resume until it
                                // yields/exits, restore the scheduler's space. The TCB frame is kernel data (shared).
        unsafe {
            vm::switch_to(roots[slot]);
            resume_frame(&mut (*addr_of_mut!(TCBS))[slot].frame as *mut TrapFrame);
            vm::switch_to(root_main);
        }
        let (mag, exited) = unsafe {
            let s = &*addr_of!(SCHED);
            (s.last_magic, s.exited)
        };
        order.push((slot, mag));
        if exited {
            // SAFETY: single-threaded write of run state.
            unsafe { (*addr_of_mut!(TCBS))[slot].done = true };
            policy.finish(TaskId(id));
        }
        if order.len() > 4 * NTASK {
            break; // safety bound — a correct run is exactly 2*NTASK*2 (8) slices
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
    // Every slice must report the magic of the task that ran it — proof the full register file
    // (rbx magic) rode through each context switch. And because both tasks share ONE code VA in
    // DIFFERENT spaces, a correct magic each slice ALSO proves the per-slice CR3 switch happened.
    let magic_ok = order.len() == 8 && order.iter().all(|(slot, mag)| *mag == magics[*slot]);
    let spaces_distinct = roots[0] != roots[1] && roots[0] != root_main && roots[1] != root_main;
    (order_ok && both_done, magic_ok, spaces_distinct)
}

/// Prove **timer-driven (involuntary) preemption**: two ring-3 tasks that never yield (tight
/// increment loops, IF set) are preempted by the PIT's IRQ0 and round-robined. Returns
/// `(both_tasks_preempted_fairly, each_task_progressed_across_preemptions)`.
fn run_preemptive() -> (bool, bool) {
    let root_main = vm::active_root();
    let mut code: [Option<frames::Frame>; NTASK] = [None, None];
    let mut stack: [Option<frames::Frame>; NTASK] = [None, None];
    let roots = match setup_tasks(
        &mut code,
        &mut stack,
        stub_bytes!(stub_spin_start, stub_spin_end),
    ) {
        Some(r) => r,
        None => return (false, false),
    };
    // Preemptible frames: IF set (RFLAGS 0x202), rbx = progress (0), rcx = bounded fallback.
    // SAFETY: single-threaded; init the TCBs before any resume.
    unsafe {
        let tcbs = &mut *addr_of_mut!(TCBS);
        for tcb in tcbs.iter_mut() {
            let mut f = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_IF);
            f.regs[RBX] = 0;
            f.regs[RCX] = SPIN_COUNTDOWN;
            *tcb = Tcb {
                frame: f,
                done: false,
            };
        }
    }

    const SLICES: usize = 6;
    let mut counts = [0usize; NTASK];
    let mut last_progress = [0u64; NTASK];
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
        // SAFETY: roots[slot] maps the kernel; run the task until the timer preempts it.
        unsafe {
            vm::switch_to(roots[slot]);
            resume_frame(&mut (*addr_of_mut!(TCBS))[slot].frame as *mut TrapFrame);
            vm::switch_to(root_main);
        }
        let (was_preempt, was_exit, progress) = unsafe {
            let s = &*addr_of!(SCHED);
            (
                s.preempted,
                s.exited,
                (*addr_of!(TCBS))[slot].frame.regs[RBX],
            )
        };
        if was_exit || !was_preempt {
            clean = false; // timer never fired (countdown drained) or an unexpected return
            break;
        }
        if seen[slot] && progress <= last_progress[slot] {
            progress_ok = false; // counter did not advance across the involuntary switch
        }
        seen[slot] = true;
        last_progress[slot] = progress;
        counts[slot] += 1;
        cur = (cur + 1) % NTASK;
    }

    cleanup_tasks(&roots, &mut code, &mut stack);

    let fair = clean && counts.iter().all(|&c| c > 0);
    (fair, progress_ok && clean)
}

/// Real blocking IPC on x86-64 (REQ-IPC-010) — the aarch64/RISC-V twin: a receiver that `recv`s an
/// EMPTY endpoint BLOCKS (descheduled via `kernel_core::sched`), a sender's `send` WAKES it and the
/// kernel delivers the body across PML4 spaces (into the receiver's saved `rdi`), and the woken
/// receiver RESUMES past its `int 0x80` and exits reporting the body. Returns
/// `(recv_blocked, send_woke_and_delivered, receiver_resumed_with_body)`.
fn run_blocking_ipc() -> (bool, bool, bool) {
    let root_main = vm::active_root();
    let (root_r, root_s) = match (vm::build_space(), vm::build_space()) {
        (Some(r), Some(s)) => (r, s),
        _ => return (false, false, false),
    };
    let r_code = vm::map_stub_frame(
        root_r,
        USER_CODE_VA,
        stub_bytes!(stub_recv_exit_start, stub_recv_exit_end),
    );
    let r_stack = vm::map_user(root_r, USER_STACK_VA, true);
    let s_code = vm::map_stub_frame(
        root_s,
        USER_CODE_VA,
        stub_bytes!(stub_syscall_start, stub_syscall_end),
    );
    let s_stack = vm::map_user(root_s, USER_STACK_VA, true);
    if r_code.is_none() || r_stack.is_none() || s_code.is_none() || s_stack.is_none() {
        free_leaf(root_r, USER_STACK_VA, r_stack);
        free_leaf(root_r, USER_CODE_VA, r_code);
        free_leaf(root_s, USER_STACK_VA, s_stack);
        free_leaf(root_s, USER_CODE_VA, s_code);
        return (false, false, false);
    }

    const BODY: u64 = 0xB10C_CAFE;
    let mut engine = CapEngine::new(0xB10C, 1000);
    let caps = alloc::vec![engine.mint("ipc", "ipc.msg", Scope::All, Constraints::none())];
    // SAFETY: single-threaded; reset endpoint/flag state before any excursion.
    unsafe {
        *addr_of_mut!(ENDPOINT) = None;
        *addr_of_mut!(IPC_RECEIVED) = 0;
        *addr_of_mut!(IPC_RECV_BLOCKED) = false;
        *addr_of_mut!(IPC_BLOCK_MODE) = true;
    }
    set_trial(Trial {
        engine,
        store: Store::new(),
        caps,
        action: "ipc.msg",
        armed: false,
        expect_fault_va: 0,
        allowed: false,
        isolation_held: false,
        fault_va: 0,
    });
    let mut recv_frame = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    recv_frame.regs[RAX] = SYS_RECV;
    recv_frame.regs[RDI] = 0;
    let mut send_frame = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    send_frame.regs[RAX] = SYS_SEND;
    send_frame.regs[RDI] = BODY;
    let mut sched = RoundRobin::new();
    sched.spawn(TaskId(0)); // receiver
    sched.spawn(TaskId(1)); // sender

    // Step 1 — receiver recv's the empty endpoint and must BLOCK.
    // SAFETY: root_r maps the running kernel.
    unsafe {
        vm::switch_to(root_r);
        resume_frame(&mut recv_frame as *mut TrapFrame);
        vm::switch_to(root_main);
    }
    let recv_blocked = unsafe { *addr_of!(IPC_RECV_BLOCKED) };
    if recv_blocked {
        sched.block(TaskId(0));
    }

    // Step 2 — sender sends; the kernel WAKES the blocked receiver, delivers the body into its rdi.
    // SAFETY: root_s maps the running kernel.
    unsafe {
        vm::switch_to(root_s);
        resume_frame(&mut send_frame as *mut TrapFrame);
        vm::switch_to(root_main);
    }
    let sent = unsafe { (*addr_of!(ENDPOINT)).is_some() };
    let send_woke_and_delivered = if sent && recv_blocked {
        let body = unsafe { (*addr_of_mut!(ENDPOINT)).take() }.unwrap_or(0);
        unsafe { *addr_of_mut!(IPC_RECEIVED) = body };
        recv_frame.regs[RDI] = body; // deliver into the woken receiver's rdi (its exit arg)
        sched.unblock(TaskId(0));
        body == BODY && sched.state(TaskId(0)) == Some(TaskState::Ready)
    } else {
        false
    };

    // Step 3 — resume the woken receiver: continues past its recv int 0x80 with rdi = body, then
    // EXITs reporting rdi — a reported magic == BODY proves it received across spaces.
    sched_report(0, false);
    // SAFETY: root_r maps the running kernel.
    unsafe {
        vm::switch_to(root_r);
        resume_frame(&mut recv_frame as *mut TrapFrame);
        vm::switch_to(root_main);
    }
    let (reported, exited) = unsafe {
        let s = &*addr_of!(SCHED);
        (s.last_magic, s.exited)
    };
    let receiver_resumed_with_body = exited && reported == BODY;

    unsafe { *addr_of_mut!(IPC_BLOCK_MODE) = false };
    take_trial();
    free_leaf(root_r, USER_STACK_VA, r_stack);
    free_leaf(root_r, USER_CODE_VA, r_code);
    free_leaf(root_s, USER_STACK_VA, s_stack);
    free_leaf(root_s, USER_CODE_VA, s_code);
    (
        recv_blocked,
        send_woke_and_delivered,
        receiver_resumed_with_body,
    )
}

/// Priority inheritance end-to-end on x86-64 (REQ-IPC-009) through the real blocking-IPC path — the
/// aarch64/RISC-V twin: a HIGH ring-3 receiver blocks on the endpoint a LOW task services; the blocked
/// HIGH donates its priority (`PriorityScheduler`) so the boosted LOW is dispatched ahead of a Ready
/// MEDIUM (inversion avoided), LOW services, and HIGH wakes. MEDIUM is a scheduler-only competitor.
/// Returns `(inversion_avoided, low_serviced, high_received)`.
fn run_priority_ipc() -> (bool, bool, bool) {
    let root_main = vm::active_root();
    let (root_h, root_l) = match (vm::build_space(), vm::build_space()) {
        (Some(h), Some(l)) => (h, l),
        _ => return (false, false, false),
    };
    let h_code = vm::map_stub_frame(
        root_h,
        USER_CODE_VA,
        stub_bytes!(stub_recv_exit_start, stub_recv_exit_end),
    );
    let h_stack = vm::map_user(root_h, USER_STACK_VA, true);
    let l_code = vm::map_stub_frame(
        root_l,
        USER_CODE_VA,
        stub_bytes!(stub_syscall_start, stub_syscall_end),
    );
    let l_stack = vm::map_user(root_l, USER_STACK_VA, true);
    if h_code.is_none() || h_stack.is_none() || l_code.is_none() || l_stack.is_none() {
        free_leaf(root_h, USER_STACK_VA, h_stack);
        free_leaf(root_h, USER_CODE_VA, h_code);
        free_leaf(root_l, USER_STACK_VA, l_stack);
        free_leaf(root_l, USER_CODE_VA, l_code);
        return (false, false, false);
    }

    const BODY: u64 = 0x9A9A_5C5C;
    const LOW: TaskId = TaskId(0);
    const MED: TaskId = TaskId(1);
    const HIGH: TaskId = TaskId(2);
    const EP: Endpoint = Endpoint(1);

    let mut engine = CapEngine::new(0x9A9A, 1000);
    let caps = alloc::vec![engine.mint("ipc", "ipc.msg", Scope::All, Constraints::none())];
    // SAFETY: single-threaded; reset endpoint/flag state.
    unsafe {
        *addr_of_mut!(ENDPOINT) = None;
        *addr_of_mut!(IPC_RECEIVED) = 0;
        *addr_of_mut!(IPC_RECV_BLOCKED) = false;
        *addr_of_mut!(IPC_BLOCK_MODE) = true;
    }
    set_trial(Trial {
        engine,
        store: Store::new(),
        caps,
        action: "ipc.msg",
        armed: false,
        expect_fault_va: 0,
        allowed: false,
        isolation_held: false,
        fault_va: 0,
    });
    let mut peng = CapEngine::new(0x00EE, 1000);
    let acq = peng.mint("sched", "ep.acquire", Scope::All, Constraints::none());
    let mut ps = PriorityScheduler::new("ep.acquire");
    ps.admit(LOW, Priority(1));
    ps.admit(MED, Priority(5));
    ps.admit(HIGH, Priority(10));
    let _ = ps.acquire(&peng, EP, LOW, &[acq]);

    let mut high_frame = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    high_frame.regs[RAX] = SYS_RECV;
    high_frame.regs[RDI] = 0;
    let mut low_frame = TrapFrame::new_user(USER_CODE_VA, USER_STACK_TOP, RFLAGS_COOP);
    low_frame.regs[RAX] = SYS_SEND;
    low_frame.regs[RDI] = BODY;

    // Step 1 — HIGH runs first and BLOCKS; it then WAITS on the endpoint LOW holds, donating to LOW.
    // SAFETY: root_h maps the running kernel.
    unsafe {
        vm::switch_to(root_h);
        resume_frame(&mut high_frame as *mut TrapFrame);
        vm::switch_to(root_main);
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
        // SAFETY: root_l maps the running kernel.
        unsafe {
            vm::switch_to(root_l);
            resume_frame(&mut low_frame as *mut TrapFrame);
            vm::switch_to(root_main);
        }
        let sent = unsafe { (*addr_of!(ENDPOINT)).is_some() };
        if sent && high_blocked {
            let body = unsafe { (*addr_of_mut!(ENDPOINT)).take() }.unwrap_or(0);
            unsafe { *addr_of_mut!(IPC_RECEIVED) = body };
            high_frame.regs[RDI] = body; // deliver into the woken HIGH receiver's rdi
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
    // SAFETY: root_h maps the running kernel.
    unsafe {
        vm::switch_to(root_h);
        resume_frame(&mut high_frame as *mut TrapFrame);
        vm::switch_to(root_main);
    }
    let (reported, exited) = unsafe {
        let s = &*addr_of!(SCHED);
        (s.last_magic, s.exited)
    };
    let high_received = exited && reported == BODY;

    unsafe { *addr_of_mut!(IPC_BLOCK_MODE) = false };
    take_trial();
    free_leaf(root_h, USER_STACK_VA, h_stack);
    free_leaf(root_h, USER_CODE_VA, h_code);
    free_leaf(root_l, USER_STACK_VA, l_stack);
    free_leaf(root_l, USER_CODE_VA, l_code);
    (inversion_avoided, low_serviced, high_received)
}

/// Shared VA for the grant-table test — an unused page in the user 1 GiB PDPT slot (below 4 GiB, as
/// the ring-3 code segment's `iret` limit requires), distinct from the code/stack/private VAs.
const SHARED_VA: u64 = USER_CODE_VA + 0x5000;

/// Prove the zero-copy shared-memory grant-table (REQ-IPC-008) through the REAL x86-64 PML4 MMU path,
/// exactly as the aarch64 (TTBR0) and RISC-V (satp) backends do — the shared `GrantTable` is the
/// arch-independent authority/lifecycle layer; THIS target's `vm.rs` performs the actual page
/// mapping. Proves, live:
///   * a `memory.share` grant maps ONE physical frame into TWO distinct process PML4 address spaces,
///     so both resolve the SAME physical frame — zero-copy across address spaces;
///   * establishing the grant is capability-gated (no `memory.share` ⇒ no grant, nothing mapped);
///   * revoking the grant unmaps the grantee's page while leaving the grantor's intact.
///
/// Returns `(cap_gated, shared_across_spaces, revoke_unmaps)`.
fn run_shared_memory() -> (bool, bool, bool) {
    let (root_a, root_b) = match (vm::build_space(), vm::build_space()) {
        (Some(a), Some(b)) => (a, b),
        _ => return (false, false, false),
    };
    let shf = match frames::alloc_zeroed() {
        Some(f) => f,
        None => return (false, false, false),
    };
    let pa = shf.addr() as u64;

    let mut engine = CapEngine::new(0x5EED, 1000);
    let share_cap = engine.mint("proc-a", "memory.share", Scope::All, Constraints::none());
    let mut gt = GrantTable::new("memory.share");
    let region = gt.create_region("proc-a", pa, frames::FRAME_SIZE);

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

    // Map the ONE frame into BOTH process PML4 spaces at the shared VA (ring-3 writable).
    let mapped = granted.is_ok()
        && vm::map_user_frame(root_a, SHARED_VA, pa, true)
        && vm::map_user_frame(root_b, SHARED_VA, pa, true);

    // (shared_across_spaces) Both distinct roots translate the shared VA to the SAME frame.
    let shared_across_spaces = mapped
        && root_a != root_b
        && vm::translate_in(root_a, SHARED_VA) == Some(pa)
        && vm::translate_in(root_b, SHARED_VA) == Some(pa);

    // (revoke_unmaps) Revocation PATH: consult the grant-table's revoke authority, and ONLY on
    // success tear down the grantee's mapping — the unmap is a consequence of a successful revoke,
    // not unconditional. The grantor keeps its own access.
    let grant_id = granted.unwrap_or(0);
    let revoke_unmaps = if gt.revoke(grant_id) {
        vm::unmap_user(root_b, SHARED_VA);
        vm::translate_in(root_b, SHARED_VA).is_none()
            && vm::translate_in(root_a, SHARED_VA) == Some(pa)
    } else {
        false
    };

    vm::unmap_user(root_a, SHARED_VA);
    frames::free(shf);

    (cap_gated, shared_across_spaces, revoke_unmaps)
}

/// Prove the ring-3 boundary + multitasking invariants live. `Ok(n)` all passed; `Err((idx,name))`.
pub fn selftest() -> Result<u32, (u32, &'static str)> {
    // Mask interrupts for the whole suite, THEN repoint the vectors (advisor: a tick landing
    // between repoint and mask would hit the context-switch entry with a stale CURRENT_FRAME).
    x86_64::instructions::interrupts::disable();
    install_entries();

    // On the ring0->ring3 iret the CPU revalidates DS/ES/FS/GS against the new CPL. FS/GS still hold
    // OVMF's stale 0x30 selector (which now indexes our TSS descriptor's upper half); null the data
    // segments up front — in 64-bit mode their bases are ignored, so kernel data access is unaffected.
    // SAFETY: single-core; long mode ignores DS/ES/FS/GS bases.
    unsafe {
        core::arch::asm!(
            "xor ax, ax",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            out("ax") _,
            options(nostack, preserves_flags)
        );
    }

    // Precondition (advisor): a freshly-built space must leave the user region UNMAPPED, so the
    // isolation proofs are real rather than silently sharing OVMF's identity map. `build_space`
    // privatizes the user PDPT slot, so this holds even though OVMF identity-maps 1..2 GiB. Fail loud.
    match vm::build_space() {
        Some(probe) if vm::translate_in(probe, USER_CODE_VA).is_none() => {} // private — good
        _ => {
            kprintln!("[usermode] FATAL: built space does not privatize the user region");
            usermode_fatal(110);
        }
    }

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

    // 1 — a ring-3 process with NO capability cannot cross the boundary: syscall denied, no effect.
    let (allowed, events) = run_syscall(false);
    check!(
        !allowed && events == 0,
        "ring3: uncapable process — syscall denied at the boundary, zero effect"
    );

    // 2 — a capability-granted ring-3 process performs EXACTLY the authorized effect (one event).
    let (allowed, events) = run_syscall(true);
    check!(
        allowed && events == 1,
        "ring3: capable process — syscall authorized via the same CapEngine, one event recorded"
    );

    if BRINGUP_CORE_ONLY {
        return Ok(n);
    }

    // 3 — hardware isolation: a ring-3 read of a supervisor-only page faults and is contained.
    let (held, fault_va) = run_isolation();
    check!(
        held && fault_va == VA_SUP,
        "ring3: read of a supervisor-only page faults — address-space isolation holds"
    );

    // 4 & 5 — per-process address spaces: a page private to process A is reachable by A but NOT by
    // process B at the SAME virtual address (each process has its own PML4 space).
    let (a_reached, b_isolated, b_fault_va) = run_cross_process_isolation();
    check!(
        a_reached,
        "ring3: process A reaches a page in its own address space (mapped VA resolves)"
    );
    check!(
        b_isolated && b_fault_va == VA_P,
        "ring3: process B cannot reach A's page at the same VA — per-process isolation holds"
    );

    // 6, 7 & 8 — cooperative multitasking with per-task address spaces: two ring-3 tasks in
    // SEPARATE PML4 spaces context-switch via yield under a round-robin scheduler, each resuming
    // with full register state, and the two tasks occupy genuinely distinct address spaces.
    let (order_ok, magic_ok, spaces_distinct) = run_scheduler();
    check!(
        order_ok,
        "ring3: round-robin scheduler runs two tasks (each in its own space) A,B,A,B,... to completion"
    );
    check!(
        magic_ok,
        "ring3: each task resumes with its own magic at the shared VA — full context + per-slice CR3 switch"
    );
    check!(
        spaces_distinct,
        "ring3: the two scheduled tasks occupy distinct PML4 address spaces"
    );

    // 9 & 10 — timer-driven (involuntary) preemption: two non-yielding ring-3 tasks are preempted
    // by the PIT IRQ0 and round-robined; each resumes with its progress counter intact.
    let (preempt_fair, preempt_progress) = run_preemptive();
    check!(
        preempt_fair,
        "ring3: PIT IRQ0 preempts two non-yielding tasks — scheduler round-robins both"
    );
    check!(
        preempt_progress,
        "ring3: each task's register counter advances across timer preemptions — state preserved"
    );

    // 11, 12 & 13 — capability-secure kernel IPC (gap register Issue 2): a message crosses from one
    // ring-3 process to another in a DIFFERENT PML4 space only through the kernel endpoint, gated by
    // the same CapEngine; an uncapable sender/receiver is denied fail-closed.
    let (delivered, send_denied, recv_denied) = run_ipc();
    check!(
        delivered,
        "ring3: capability-secure IPC — message delivered kernel-mediated across distinct address spaces"
    );
    check!(
        send_denied,
        "ring3: IPC send without the ipc.send capability is denied — endpoint untouched (fail-closed)"
    );
    check!(
        recv_denied,
        "ring3: IPC recv without the ipc.recv capability is denied — queued message intact (fail-closed)"
    );

    // 14, 15 & 16 — zero-copy shared memory (gap register Issue 2 / REQ-IPC-008): a memory.share
    // grant maps ONE physical frame into TWO distinct PML4 address spaces (zero-copy across AS),
    // establishing it is capability-gated (fail-closed), and revocation unmaps the grantee's page.
    let (cap_gated, shared_across_spaces, revoke_unmaps) = run_shared_memory();
    check!(
        cap_gated,
        "ring3: shared-memory grant is capability-gated — no memory.share ⇒ no grant, nothing mapped (fail-closed)"
    );
    check!(
        shared_across_spaces,
        "ring3: grant-table maps one frame into two distinct PML4 spaces — zero-copy shared memory across address spaces"
    );
    check!(
        revoke_unmaps,
        "ring3: a successful grant revoke gates the unmap of the grantee's page; the grantor keeps access"
    );

    // 17, 18 & 19 — real BLOCKING IPC (REQ-IPC-010): recv on empty BLOCKS, send WAKES + delivers
    // across PML4 spaces, the woken receiver RESUMES past its int 0x80 with the body in rdi + reports.
    let (recv_blocked, send_woke, receiver_resumed) = run_blocking_ipc();
    check!(
        recv_blocked,
        "ring3: recv on an empty endpoint BLOCKS the receiver — it is descheduled (kernel_core::sched)"
    );
    check!(
        send_woke,
        "ring3: a send WAKES the blocked receiver (unblock ⇒ Ready) and delivers the body across spaces"
    );
    check!(
        receiver_resumed,
        "ring3: the woken receiver RESUMES past its int 0x80 with the body in rdi and exits reporting it"
    );

    // 20, 21 & 22 — priority inheritance end-to-end (REQ-IPC-009): a blocked HIGH donates to the LOW
    // endpoint holder, so the boosted LOW is dispatched over a Ready MEDIUM; LOW services, HIGH wakes.
    let (inversion_avoided, low_serviced, high_received) = run_priority_ipc();
    check!(
        inversion_avoided,
        "ring3: blocked HIGH donates to the LOW endpoint holder — scheduler dispatches boosted LOW over Ready MEDIUM"
    );
    check!(
        low_serviced,
        "ring3: the boosted LOW runs and services the endpoint (sends), waking HIGH"
    );
    check!(
        high_received,
        "ring3: HIGH resumes as highest-priority and receives the body across address spaces"
    );

    Ok(n)
}
